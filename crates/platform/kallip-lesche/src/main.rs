//! `kallip-lesche`: the kallip data-plane relay (λέσχη -- the Greek conversation
//! hall, beside the archeion).
//!
//! The lesche owns every agent/human communication surface -- tagma tunnels,
//! app event streams, envelope routing, key exchange, presence -- plus the
//! durable chat store (rooms, membership, message payloads). The in-memory
//! surfaces are soft-state rebuilt on restart;
//! the chat schema persists in the lesche's own Postgres. It authenticates
//! requests, resolves tagma metadata, attests identity facts, and advances the
//! tunnel-proof replay guard through a narrow `ControlPlane` trait implemented
//! by an HTTP client (`HttpControlPlane`) that calls the archeion's `/internal/*`
//! API. All app<->tagma business evolution happens in this crate and the shared
//! `kallip-archeion-common` wire types, never in the registry.

mod args;
mod auth;
mod control_plane_http;
mod control_policy;
mod db;
mod fan;
mod identity;
mod member_identity;
mod middleware;
mod room_presence;
mod routes;
mod sse;
mod state;

#[cfg(test)]
mod test_support;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
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
    // File logging is opt-in via KALLIP_LESCHE_LOG_DIR: set, events are
    // double-written to a rolling file and stdout; unset keeps the historical
    // stdout-only behavior, so container and dev forms are untouched.
    let log_dir =
        kallip_common::logging::parse_log_dir(std::env::var("KALLIP_LESCHE_LOG_DIR").ok());
    kallip_common::logging::init_service_logging(&filter, "lesche", log_dir.as_deref());

    // The registry is reached only through the HTTP ControlPlane client. There
    // is intentionally no auth cache: long-lived connections (tagma tunnel, app
    // SSE) authenticate once at open, and the remaining verify calls are
    // human-paced, so uncached per-request RPC is negligible load. See
    // `control_plane_http`.
    let control = Arc::new(HttpControlPlane::new(
        args.archeion_internal_url.clone(),
        kallip_common::secret_file::read_trimmed_with_retry(
            std::path::Path::new(&args.archeion_internal_token_file),
            kallip_common::secret_file::BOOT_RETRY,
        )?,
    ));

    // The durable room-message store, connected + migrated at boot. The state
    // carries it as `Option<Db>` so the mock routing tests can pass `None`
    // (they do not exercise persistence); the relay fan-out branches on
    // `state.db.as_ref()`.
    let db = db::connect_and_migrate(&args.database_url).await?;

    let conv_state: SharedConvState = Arc::new(state::ConversationsState {
        control,
        registry: std::sync::RwLock::new(state::Registry::new()),
        pending_key_exchange: std::sync::Mutex::new(std::collections::HashMap::new()),
        proof_skew_secs: args.proof_skew_secs,
        key_exchange_timeout: Duration::from_secs(args.key_exchange_timeout_secs),
        db: Some(db),
        agent_profiles: state::AgentProfileCache::default(),
    });

    // The relay routes carry `SharedConvState` (already applied inside
    // `routes::router`); the result is a stateless `Router<()>`. Merge it in:
    // mounted at the root, so the data-plane paths are bare resource paths;
    // CSRF guard to the whole relay surface (it gates the cookie-bearing
    // `POST /conversations` and is a no-op for bearer/machine requests).
    let internal_token_hash = (!args.internal_token.is_empty())
        .then(|| kallip_common::authtoken::TokenHash::of(&args.internal_token));
    let v1 = routes::router(conv_state.clone(), internal_token_hash)
        .layer(axum::middleware::from_fn(middleware::csrf_guard));

    // Outermost layers: body limit, then CORS (explicit allowlist, never Any),
    // then request tracing. Mirrors the archeion's layer order.
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .merge(v1)
        .layer(axum::extract::DefaultBodyLimit::max(body_size_bytes(
            args.max_body_size_kb,
        )))
        .layer(routes::cors_layer(&args.cors_origins))
        // INFO-level request spans: the failure line must carry the
        // method/uri to be triageable (the default DEBUG span is filtered
        // out by the info log level, which is exactly why the in-the-wild
        // 503s had no target path in the logs).
        .layer(
            tower_http::trace::TraceLayer::new_for_http().make_span_with(
                tower_http::trace::DefaultMakeSpan::new().level(tracing::Level::INFO),
            ),
        );

    let listener = tokio::net::TcpListener::bind(&args.listen_addr)
        .await
        .with_context(|| format!("binding listen addr {}", args.listen_addr))?;
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

/// Readiness: the process is up. readyz deliberately checks process liveness
/// only (not DB connectivity), so it stays "can accept connections" -- always
/// true until shutdown begins.
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
