mod args;
mod auth;
mod backend;
mod bridge;
mod error;
mod messaging;
mod routes;
mod shutdown;
mod skill_promote;
mod sse;
mod state;
mod token;

#[cfg(test)]
mod test_helpers;

use anyhow::{Context, Result};
use clap::Parser;
use kallip_common::authtoken::MintedToken;
use kallip_runtime::profile::ProfileRegistry;
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

    // Expose daemon URL so agent shells can discover it via $KALLIP_DAEMON_URL.
    // Safe: called once at startup, single-threaded, before any concurrent operations.
    unsafe {
        std::env::set_var("KALLIP_DAEMON_URL", &args.advertise_url);
    }

    // Mint the operator token: honor KALLIP_OPERATOR_TOKEN if set (back-compat
    // for automation), otherwise generate a fresh 256-bit `sk-operator-…` token.
    // Only the SHA-256 hash is retained by AppState; the plaintext is printed below
    // then dropped at end of scope.
    let operator = match std::env::var("KALLIP_OPERATOR_TOKEN") {
        Ok(s) => MintedToken::from_secret(s),
        Err(_) => MintedToken::generate(token::OPERATOR),
    };
    anyhow::ensure!(
        !operator.secret().trim().is_empty(),
        "KALLIP_OPERATOR_TOKEN must not be empty"
    );
    println!("─────────────────────────────────────────────────");
    println!("  kallipai {}", env!("CARGO_PKG_VERSION"));
    println!("  Operator Token:");
    println!("  {}", operator.secret());
    println!();
    println!("  WARNING: Do not leak this token.");
    println!();
    println!("  To authenticate, either:");
    println!("  - Set env and launch TUI:");
    println!("      export KALLIP_AUTH_TOKEN={}", operator.secret());
    println!("      kallip-tui");
    println!("  - Or enter the token when prompted inside the TUI.");
    println!("─────────────────────────────────────────────────");

    anyhow::ensure!(
        args.prompt_queue_size >= 1,
        "KALLIP_PROMPT_QUEUE_SIZE must be >= 1, got {}",
        args.prompt_queue_size
    );
    anyhow::ensure!(
        (1..=args::MAX_AGENTS_LIMIT).contains(&args.max_agents),
        "KALLIP_MAX_AGENTS must be 1..={}, got {}",
        args::MAX_AGENTS_LIMIT,
        args.max_agents
    );
    anyhow::ensure!(
        (1..=args::MAX_SUBAGENTS_LIMIT).contains(&args.max_subagents),
        "KALLIP_MAX_SUBAGENTS must be 1..={}, got {}",
        args::MAX_SUBAGENTS_LIMIT,
        args.max_subagents
    );
    // 0 means "use axum default", so skip validation. Otherwise cap at 1 GB
    // to prevent silent overflow when converting KB → bytes (* 1024).
    if args.max_body_size_kb > 0 {
        anyhow::ensure!(
            args.max_body_size_kb <= 1_048_576,
            "KALLIP_MAX_BODY_SIZE_KB must be <= 1048576 (1 GB), got {}",
            args.max_body_size_kb
        );
    }

    // Load profile config once at startup (config file or implicit env profile), then build one
    // backend per referenced endpoint and assemble the registry before restoring agents —
    // restored agents resolve their profile from here too. The daemon owns reqwest + backend
    // construction; the runtime holds the pre-built backends and does selection (plus reuse of
    // `reqwest` types for HTTP-shape retry classification). A
    // misconfigured endpoint (unknown family, bad config) fails fast here at startup.
    let cfg = kallip_runtime::profile::load().context("failed to load model profiles")?;
    let factory = just_llm_client::client::BackendFactory::new();
    let user_agent = backend::resolve_user_agent(args.llm_api_user_agent.as_deref());
    let source = backend::build_backends(&cfg, factory, user_agent)
        .context("failed to build LLM backends")?;
    let profiles = Arc::new(ProfileRegistry::new(cfg.tiers, source)?);

    let state = Arc::new(AppState::with_limits(
        operator.hash().clone(),
        args.max_agents,
        args.max_subagents,
        args.prompt_queue_size,
        profiles,
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

    shutdown::graceful_agent_shutdown(&state).await;

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
