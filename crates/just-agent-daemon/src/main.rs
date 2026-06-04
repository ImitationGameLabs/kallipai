mod args;
mod auth;
mod bridge;
mod routes;
mod skill_promote;
mod sse;
mod state;

use anyhow::Result;
use clap::Parser;
use state::AppState;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use args::Args;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Expose daemon URL so agent shells can discover it via $JUST_AGENT_DAEMON_URL.
    // Safe: called once at startup, single-threaded, before any concurrent operations.
    unsafe {
        std::env::set_var("JUST_AGENT_DAEMON_URL", &args.advertise_url);
    }

    let operator_token = uuid::Uuid::new_v4().to_string();
    println!("─────────────────────────────────────────────────");
    println!("  Operator Token:");
    println!("  {operator_token}");
    println!();
    println!("  WARNING: Do not leak this token.");
    println!();
    println!("  To authenticate, either:");
    println!("  - Set env and launch TUI:");
    println!("      export JUST_AGENT_AUTH_TOKEN={operator_token}");
    println!("      just-agent-tui");
    println!("  - Or enter the token when prompted inside the TUI.");
    println!("─────────────────────────────────────────────────");

    let state = Arc::new(AppState::new(operator_token));

    // Restore persisted agents before accepting requests.
    routes::restore_sessions(&state).await;

    let app = routes::router()
        .with_state(state.clone())
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;
    info!(addr = %args.listen_addr, advertise = %args.advertise_url, "daemon listening");
    let shutdown_token = state.shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_token))
        .await?;

    // After HTTP server stops: give agents a brief window to persist.
    graceful_agent_shutdown(&state).await;

    Ok(())
}

async fn shutdown_signal(token: CancellationToken) {
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

/// Give in-flight agent tasks a brief window to persist, then abort.
async fn graceful_agent_shutdown(state: &AppState) {
    let registry = state.registry.read().await;
    if registry.is_empty() {
        return;
    }
    info!(count = registry.len(), "waiting for agents to persist");

    // Allow agents a few seconds to finish persisting after cancellation.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    for entry in registry.values() {
        entry.agent.agent_handle.abort();
        entry.agent.bridge_handle.abort();
    }
    info!("all agents shut down");
}
