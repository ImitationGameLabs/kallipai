//! `kallip-agora`: the public-internet relay control plane.
//!
//! Lean-B design: the agora is a **soft-state forwarder + control plane**. It
//! holds only in-memory presence/registry/routing, brokers app<->herald
//! end-to-end key exchange (without ever deriving the key), and forwards
//! envelopes it cannot decrypt. It stores **no history** — conversation history
//! lives on the host/daemon.

mod args;
mod auth;
mod routes;
mod sse;
mod state;
#[cfg(test)]
mod test_helpers;
mod token;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use kallip_common::authtoken::MintedToken;
use tracing::info;

use args::Args;
use state::{AppState, Limits};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Mint the admin token: honor KALLIP_AGORA_ADMIN_TOKEN if set, otherwise
    // generate a fresh `sk-admin-...`. Only the hash is retained; the plaintext
    // is printed once below then dropped.
    let admin = match args.admin_token.clone() {
        Some(s) => MintedToken::from_secret(s),
        None => MintedToken::generate(token::ADMIN),
    };
    println!("─────────────────────────────────────────────────");
    println!("  kallip-agora {}", env!("CARGO_PKG_VERSION"));
    println!("  Admin Token:");
    println!("    {}", admin.secret());
    println!("  (retain only this hash; plaintext shown once)");
    println!("─────────────────────────────────────────────────");

    let limits = Limits {
        max_body_size_bytes: body_size_bytes(args.max_body_size_kb),
        enrollment_code_ttl: Duration::from_secs(args.enrollment_code_ttl_secs),
        proof_skew_secs: args.proof_skew_secs,
        max_conversations_per_user: args.max_conversations_per_user,
        key_exchange_timeout: Duration::from_secs(args.key_exchange_timeout_secs),
    };
    let state: Arc<AppState> = Arc::new(AppState::new(admin.hash().clone(), limits));

    let app = routes::router().with_state(state.clone());

    // Outermost layers: body limit, then CORS (explicit allowlist, never Any),
    // then request tracing.
    let app = app
        .layer(axum::extract::DefaultBodyLimit::max(
            state.limits.max_body_size_bytes,
        ))
        .layer(routes::cors_layer(&args.cors_origins))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;
    info!(addr = %args.listen_addr, "agora listening");
    let shutdown_token = state.shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_token))
        .await?;

    Ok(())
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

async fn shutdown_signal(token: tokio_util::sync::CancellationToken) {
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
    token.cancel();
}
