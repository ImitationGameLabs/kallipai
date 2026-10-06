//! `kallipai-model-gateway`: the kallipai model gateway.
//!
//! Two physically separated faces: the data plane (pingora on the public
//! address) serves the distribution reads, the LLM-compatible forwarding
//! and the health probe, and never touches credential material beyond
//! stamping it onto the upstream request; the management face (axum on
//! its own address) stores provider/proxy credentials behind its own
//! credential family and lands every change in management_events inside
//! the change's own transaction. The separation is structural, not just
//! convention: sibling `distribution` / `secret` modules share no code
//! path, a credential's key bytes have no getter (the two credential-
//! stamping egress points are the only consumers), and `Debug` prints
//! them as `[REDACTED]`.
//! Forwarding authorizes under the consuming account's visibility
//! domain: both the explicit profile override and the default-set
//! fallback answer 403 when the target profile is outside the
//! presenting tagma's owner domain. Consuming identities are
//! platform tokens (the archeion's verify-bearer); macro
//! observability is served at /metrics, and per-request usage
//! metering awaits its body-scanning batch.

mod args;
mod audit;
mod data_plane;
mod db;
mod distribution;
mod forward;
mod management;
mod metrics;
mod registry;
mod routes;
mod secret;
mod state;

#[cfg(test)]
mod test_support;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use args::Args;
use state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // File logging is opt-in via KALLIPAI_MODEL_GATEWAY_LOG_DIR (same
    // shape as the lesche: set, the rolling file is the only event
    // channel; unset keeps stdout-only).
    let log_dir = kallipai_common::logging::parse_log_dir(
        std::env::var("KALLIPAI_MODEL_GATEWAY_LOG_DIR").ok(),
    );
    kallipai_common::logging::init_service_logging(&filter, "model-gateway", log_dir.as_deref());

    // The durable store (profile/credential registry) is connected and
    // the full schema migrations are applied at boot.
    let db = db::connect_and_migrate(&args.database_url).await?;

    // The admin face authenticates against the platform identity (the
    // archeion): the internal URL plus the shared internal secret from
    // its provisioned file (read once at boot; the bounded wait absorbs
    // the boot-ordering race). Without the wiring the face is closed
    // (Disabled) instead of failing boot: the distribution and
    // forwarding faces do not need it.
    let management_auth = match (&args.archeion_url, &args.internal_token_file) {
        (Some(url), Some(path)) => {
            let internal_token = kallipai_common::secret_file::read_trimmed_with_retry(
                std::path::Path::new(path),
                kallipai_common::secret_file::BOOT_RETRY,
            )
            .context("reading the archeion internal token file")?;
            management::AdminAuth::Platform(std::sync::Arc::new(management::ArcheionVerifier::new(
                url.clone(),
                internal_token,
            )))
        }
        _ => {
            tracing::warn!(
                "no archeion wiring configured: the management face refuses every request"
            );
            management::AdminAuth::Disabled
        }
    };
    let state = AppState {
        db,
        public_base_url: args.public_base_url.clone(),
        identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
        management: management_auth,
        metrics: crate::metrics::Metrics::default_registry(),
    };

    // The data plane: pingora drives its own runtime on a dedicated
    // thread; the tokio runtime below keeps serving the management face
    // in the same process.
    data_plane::spawn_data_plane(
        args.listen_addr.clone(),
        state.clone(),
        args.cors_origins.clone(),
    )
    .context("spawning the pingora data plane")?;
    info!(addr = %args.listen_addr, "kallipai-model-gateway data plane listening (pingora)");

    // The management plane: axum on the tokio runtime, with graceful
    // shutdown. (The pingora thread exits with the process; its graceful
    // teardown is not wired.)
    let management = routes::management_plane_router(state, &args.cors_origins);
    let management_listener = tokio::net::TcpListener::bind(&args.management_listen_addr)
        .await
        .with_context(|| {
            format!(
                "binding management-plane addr {}",
                args.management_listen_addr
            )
        })?;
    info!(
        addr = %args.management_listen_addr,
        "kallipai-model-gateway management plane listening"
    );

    axum::serve(
        management_listener,
        management.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("management plane server error")?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let sigterm = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = sigterm => {},
    }
    info!("received shutdown signal, initiating graceful shutdown");
}
