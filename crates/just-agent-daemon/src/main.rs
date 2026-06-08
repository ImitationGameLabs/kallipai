mod args;
mod auth;
mod bridge;
mod error;
mod routes;
mod skill_promote;
mod sse;
mod state;

#[cfg(test)]
mod test_helpers;

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

    anyhow::ensure!(
        args.prompt_queue_size >= 1,
        "JUST_AGENT_PROMPT_QUEUE_SIZE must be >= 1, got {}",
        args.prompt_queue_size
    );
    anyhow::ensure!(
        (1..=args::MAX_AGENTS_LIMIT).contains(&args.max_agents),
        "JUST_AGENT_MAX_AGENTS must be 1..={}, got {}",
        args::MAX_AGENTS_LIMIT,
        args.max_agents
    );
    anyhow::ensure!(
        (1..=args::MAX_SUBAGENTS_LIMIT).contains(&args.max_subagents),
        "JUST_AGENT_MAX_SUBAGENTS must be 1..={}, got {}",
        args::MAX_SUBAGENTS_LIMIT,
        args.max_subagents
    );
    // 0 means "use axum default", so skip validation. Otherwise cap at 1 GB
    // to prevent silent overflow when converting KB → bytes (* 1024).
    if args.max_body_size_kb > 0 {
        anyhow::ensure!(
            args.max_body_size_kb <= 1_048_576,
            "JUST_AGENT_MAX_BODY_SIZE_KB must be <= 1048576 (1 GB), got {}",
            args.max_body_size_kb
        );
    }

    let state = Arc::new(AppState::with_limits(
        operator_token,
        args.max_agents,
        args.max_subagents,
        args.prompt_queue_size,
    ));

    // Restore persisted agents before accepting requests.
    routes::restore_agents(&state).await;

    let app = routes::router().with_state(state.clone());

    // Apply body size limit first (outermost layer), then tracing.
    // When max_body_size_kb > 0, enforce the configured limit.
    // When 0, axum's built-in default (2 MB) applies instead.
    let app = if args.max_body_size_kb > 0 {
        app.layer(axum::extract::DefaultBodyLimit::max(
            args.max_body_size_kb * 1024,
        ))
    } else {
        app
    }
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
