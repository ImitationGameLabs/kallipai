//! `kallipai-keryx`: the tagma-link and user-DM relay (keryx -- the Greek
//! herald): the tagma-facing inbound channel, the E2E user conversations,
//! and the plaintext state streams. All state is in-memory; the service
//! holds no database. It resolves credentials and tagma metadata through
//! a narrow `ControlPlane` trait implemented by an HTTP client
//! (`HttpControlPlane`) that calls the archeion's `/internal/*` API.
//!
//! The mounted route surface is the app events face (`/me/events`), the
//! E2E conversations, the tagma tunnel, plus the liveness pair.
mod args;
mod auth;
mod control_plane_http;
mod control_policy;
mod events;
mod link;
mod middleware;
mod routes;
mod sse;
mod state;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::get;
use clap::Parser;
use tracing::info;

use args::Args;
use control_plane_http::HttpControlPlane;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // File logging is opt-in via KALLIPAI_KERYX_LOG_DIR: set, the rolling
    // file is the only event channel; unset keeps the stdout-only
    // behavior, so container and dev forms are untouched.
    let log_dir =
        kallipai_common::logging::parse_log_dir(std::env::var("KALLIPAI_KERYX_LOG_DIR").ok());

    // Spans export when OTEL_EXPORTER_OTLP_ENDPOINT says where; the guard
    // lives to the end of main so the exporter flushes on the way out.
    let _telemetry =
        kallipai_common::telemetry::init_service_telemetry(&filter, "keryx", log_dir.as_deref());

    // The registry is reached only through the HTTP ControlPlane client
    // (there is intentionally no auth cache: long-lived connections
    // authenticate once at open, and the remaining verify calls are
    // human-paced, so uncached per-request RPC is negligible load).
    let control = Arc::new(HttpControlPlane::new(
        args.archeion_internal_url.clone(),
        kallipai_common::secret_file::read_trimmed_with_retry(
            std::path::Path::new(&args.archeion_internal_token_file),
            kallipai_common::secret_file::BOOT_RETRY,
        )?,
    ));

    let conv_state: state::SharedConvState = Arc::new(state::ConversationsState {
        control,
        registry: std::sync::RwLock::new(state::Registry::new()),
        pending_key_exchange: std::sync::Mutex::new(std::collections::HashMap::new()),
        proof_skew_secs: args.proof_skew_secs,
        key_exchange_timeout: std::time::Duration::from_secs(args.key_exchange_timeout_secs),
    });

    // The relay routes carry `SharedConvState` (already applied inside
    // `routes::router`); the result is a stateless `Router<()>`. Merge it in:
    // mounted at the root, so the data-plane paths are bare resource paths;
    // CSRF guard to the whole relay surface (it gates cookie-bearing
    // mutating requests and is a no-op for bearer/machine requests).
    let internal_token_hash = (!args.internal_token.is_empty())
        .then(|| kallipai_common::authtoken::TokenHash::of(&args.internal_token));
    let v1 = routes::router(conv_state, internal_token_hash)
        .layer(axum::middleware::from_fn(middleware::csrf_guard));

    // Layer order, outermost first: the request span mounted below by
    // `with_server_span`, then CORS (explicit allowlist, never Any), then the body limit.
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .merge(v1)
        .layer(axum::extract::DefaultBodyLimit::max(body_size_bytes(
            args.max_body_size_kb,
        )))
        .layer(routes::cors_layer(&args.cors_origins));
    let app = kallipai_common::telemetry::with_server_span(app);

    let listener = tokio::net::TcpListener::bind(&args.listen_addr)
        .await
        .with_context(|| format!("binding listen addr {}", args.listen_addr))?;
    info!(addr = %args.listen_addr, "kallipai-keryx listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    Ok(())
}

/// Liveness: the process is up.
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: the process is up. readyz deliberately checks process liveness
/// only (no dependency probes), so it stays "can accept connections" --
/// always true until shutdown begins.
async fn readyz() -> &'static str {
    "ready"
}

/// Resolve the body-size limit in bytes. `0` means "use axum's default"
/// (2 MB); any other value is kilobytes.
fn body_size_bytes(max_body_size_kb: usize) -> usize {
    if max_body_size_kb > 0 {
        max_body_size_kb.saturating_mul(1024)
    } else {
        2 * 1024 * 1024
    }
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
