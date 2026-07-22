//! `kallip-lesche`: the kallip data-plane relay (λέσχη -- the Greek conversation
//! hall, beside the agora).
//!
//! The lesche owns every agent/human communication surface -- herald tunnels,
//! app event streams, envelope routing, key exchange, presence -- as in-memory
//! soft-state rebuilt on restart. It never touches the durable store: it
//! authenticates requests, resolves tagma metadata, and advances the
//! tunnel-proof replay guard through a narrow `ControlPlane` trait implemented
//! by an HTTP client (`HttpControlPlane`) that calls the agora's `/internal/*`
//! API. All app<->herald business evolution happens in this crate and the shared
//! `kallip-agora-common` wire types, never in the registry.

mod args;
mod auth;
mod control_plane_http;
mod middleware;
mod routes;
mod sse;
mod state;

#[cfg(test)]
mod test_support;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use clap::Parser;
use tracing::info;

use args::Args;
use control_plane_http::HttpControlPlane;
use state::SharedConvState;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // The registry is reached only through the HTTP ControlPlane client. There
    // is intentionally no auth cache: long-lived connections (herald tunnel, app
    // SSE) authenticate once at open, and the remaining verify calls are
    // human-paced, so uncached per-request RPC is negligible load. See
    // `control_plane_http`.
    let control = Arc::new(HttpControlPlane::new(
        args.agora_internal_url.clone(),
        args.agora_internal_token.clone(),
    ));

    let conv_state: SharedConvState = Arc::new(state::ConversationsState {
        control,
        registry: std::sync::RwLock::new(state::Registry::new()),
        pending_key_exchange: std::sync::Mutex::new(std::collections::HashMap::new()),
        proof_skew_secs: args.proof_skew_secs,
        key_exchange_timeout: Duration::from_secs(args.key_exchange_timeout_secs),
    });

    // The relay routes carry `SharedConvState` (already applied inside
    // `routes::router`); the result is a stateless `Router<()>`. Nest it under
    // `/v1` so the data-plane paths keep their `/v1/...` contract, and apply the
    // CSRF guard to the whole v1 surface (it gates the cookie-bearing
    // `POST /conversations` and is a no-op for bearer/machine requests).
    let v1 = routes::router(conv_state).layer(axum::middleware::from_fn(middleware::csrf_guard));

    // Outermost layers: body limit, then CORS (explicit allowlist, never Any),
    // then request tracing. Mirrors the agora's layer order.
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .nest("/v1", v1)
        .layer(axum::extract::DefaultBodyLimit::max(body_size_bytes(
            args.max_body_size_kb,
        )))
        .layer(routes::cors_layer(&args.cors_origins))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;
    info!(addr = %args.listen_addr, "kallip-lesche listening");
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

/// Readiness: the process is up (the lesche is soft-state; readiness is "can
/// accept connections", which is always true until shutdown begins).
async fn readyz() -> &'static str {
    "ready"
}

/// Resolve the body-size limit in bytes. `0` means "use axum's default" (2 MB);
/// any other value is kilobytes.
fn body_size_bytes(max_body_size_kb: usize) -> usize {
    if max_body_size_kb > 0 {
        max_body_size_kb * 1024
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
