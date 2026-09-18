//! `kallip-model-gateway`: the kallip model gateway.
//!
//! Two physically separated faces: the distribution face serves model profile
//! data to clients and never touches credential material; the secret face
//! stores provider/proxy credentials and injects Authorization headers on
//! the forwarding path. The separation is structural, not just convention:
//! sibling `distribution` / `secret` modules share no code path, and a
//! credential's key bytes have no accessor -- their only consumer is the
//! secret module's `inject`, and `Debug` prints them as `[REDACTED]`.
//! Forwarding (`forward`) authorizes under the same allowed-sets contract
//! as distribution: both the explicit profile override and the default-set
//! fallback answer 403 when the target profile is outside the presenting
//! key's allowed sets.
//! Forwarding also enforces the authorization matrix (the per-tagma quota
//! share and the profile total, 429 when either is exhausted) and lands
//! one audit row per request in Postgres.
//! The management face (the /admin routes) authenticates with its
//! own credential family -- the static management token behind
//! the ManagementAuth trait -- and lands every change in
//! management_events inside the change's own transaction.

mod args;
mod db;
mod quota;
mod routes;
mod state;

mod audit;
mod distribution;
mod forward;
mod management;
mod registry;
mod secret;

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
    // File logging is opt-in via KALLIP_MODEL_GATEWAY_LOG_DIR (same shape as
    // the lesche: set, events are double-written to a rolling file and
    // stdout; unset keeps stdout-only).
    let log_dir =
        kallip_common::logging::parse_log_dir(std::env::var("KALLIP_MODEL_GATEWAY_LOG_DIR").ok());
    kallip_common::logging::init_service_logging(&filter, "model-gateway", log_dir.as_deref());

    // The durable store (profile/credential registry) is connected and
    // the full schema migrations are applied at boot.
    let db = db::connect_and_migrate(&args.database_url).await?;

    // The management credential is hashed into the authenticator once,
    // here at boot; rotation means restarting the process. An empty
    // config closes the management face (Disabled) instead of failing
    // the boot: the distribution and forwarding faces do not need it.
    let management_auth: std::sync::Arc<dyn management::ManagementAuth> = if args
        .management_token
        .is_empty()
    {
        tracing::warn!("no management token configured: the management face refuses every request");
        std::sync::Arc::new(management::Disabled)
    } else {
        std::sync::Arc::new(management::StaticManagementToken::new(
            &args.management_token,
        ))
    };
    let state = AppState {
        db,
        public_base_url: args.public_base_url.clone(),
        quota: std::sync::Arc::new(quota::QuotaLedger::new()),
        management: management_auth,
        key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
    };
    let data_plane = routes::data_plane_router(state.clone(), &args.cors_origins);
    let management_plane = routes::management_plane_router(state, &args.cors_origins);

    let data_listener = tokio::net::TcpListener::bind(&args.listen_addr)
        .await
        .with_context(|| format!("binding data-plane addr {}", args.listen_addr))?;
    let management_listener = tokio::net::TcpListener::bind(&args.management_listen_addr)
        .await
        .with_context(|| {
            format!(
                "binding management-plane addr {}",
                args.management_listen_addr
            )
        })?;
    info!(addr = %args.listen_addr, "kallip-model-gateway data plane listening");
    info!(
        addr = %args.management_listen_addr,
        "kallip-model-gateway management plane listening"
    );

    let data_server = axum::serve(
        data_listener,
        data_plane.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());
    let management_server = axum::serve(
        management_listener,
        management_plane.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());

    tokio::try_join!(
        async { data_server.await.context("data plane server error") },
        async {
            management_server
                .await
                .context("management plane server error")
        }
    )?;

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
