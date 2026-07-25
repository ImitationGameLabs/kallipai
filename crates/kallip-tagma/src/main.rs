mod args;
mod auth;
mod backend;
mod bridge;
mod credentials;
mod messaging;
mod relay;
mod routes;
mod shutdown;
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

    // Expose tagma URL so agent shells can discover it via $KALLIP_TAGMA_URL.
    // Safe: called once at startup, single-threaded, before any concurrent operations.
    unsafe {
        std::env::set_var("KALLIP_TAGMA_URL", &args.advertise_url);
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
    // restored agents resolve their profile from here too. The tagma owns reqwest + backend
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
        kallip_runtime::config::policy_preset_from_env(),
    ));

    // Ensure the shared skills dir exists before any agent is restored. The
    // root agent authors shared skills via `bash_exec`, and landlock `PathBeneath`
    // silently skips non-existent paths — so without this the root carve would be
    // dropped on a fresh data dir and root's first write would fail opaquely. Must
    // precede `restore_agents`: a restored root rebuilds its tool dispatch (and
    // thus captures the landlock closure) inside restore.
    std::fs::create_dir_all(kallip_runtime::tools::skill_dir()?)
        .map_err(|e| anyhow::anyhow!("failed to create shared skills dir: {e}"))?;

    // Seed the shipped skill defaults into the now-existing (and empty on a
    // fresh data dir) shared skills dir. Same ordering rationale as create_dir
    // above: a restored root rebuilds its tool dispatch inside restore, so the
    // seed must land first for root to see the curated tree. Seeding is
    // best-effort: skills are optional context (the meta-skill is compiled in,
    // agents degrade gracefully with an empty dir), so a failure is logged and
    // the tagma continues rather than aborting boot.
    if let Err(e) = kallip_runtime::tools::seed_skills_if_empty() {
        tracing::warn!("skill seed failed: {e:#}; skipping");
    }

    // Restore persisted agents before accepting requests, then ensure the
    // tagma-global root agent exists. Both run before the router accepts a single
    // connection, so the singleton root invariant holds for every client (clients
    // fetch it via `GET /agents/root` instead of check-then-create).
    routes::restore_agents(&state).await?;
    routes::ensure_root_agent(&state).await?;

    // Optional online-mode relay: enroll + spawn the connector task. Degrades
    // to local-only on any enrollment failure (logs error, leaves the relay
    // unset; the lesche message route then returns 503). Local agents keep running.
    if let Some(agora_url) = args.relay_agora_url.clone() {
        match activate_relay(&state, &args, agora_url).await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!("relay activation failed, running local-only: {e:#}");
            }
        }
    }

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
    .layer(routes::cors_layer())
    .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;
    info!(addr = %args.listen_addr, advertise = %args.advertise_url, "tagma listening");
    let shutdown_token = state.shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_token))
        .await?;

    // Drain the relay task first (it tears down the tunnel + pump), then the
    // agents. Both observe the tagma-wide `shutdown` token.
    shutdown::drain_relay(&state).await;
    shutdown::graceful_agent_shutdown(&state).await;

    Ok(())
}

/// Build and install the relay connector: resolve the relay state dir, load or
/// enroll the tagma credential, build the lesche client, construct the
/// `RelayHandle`, and spawn its long-running tunnel task. On any error the
/// caller logs and continues local-only.
async fn activate_relay(state: &Arc<AppState>, args: &args::Args, agora_url: String) -> Result<()> {
    use kallip_runtime::persistence::data_dir_root;
    let credentials_dir = data_dir_root()?.join("credentials");
    std::fs::create_dir_all(&credentials_dir).context("create credentials dir")?;
    credentials::set_owner_only(&credentials_dir)?;

    let device = credentials::load_or_create_device(&credentials_dir)?;

    // Load stored credentials, or enroll (first run).
    let (tagma_id, tagma_token) = match credentials::load_tagma(&credentials_dir) {
        Some((id, token)) => {
            info!(tagma = %id, "relay: loaded stored tagma credentials");
            (kallip_agora_common::ids::TagmaId::from(id), token)
        }
        None => {
            let code = args.relay_enrollment_code.as_deref().context(
                "no stored tagma token; KALLIP_RELAY_ENROLLMENT_CODE required for first run",
            )?;
            let (tagma_id, token) = kallip_agora_client::AgoraClient::builder(&agora_url)
                .build()?
                .enroll(code, &device)
                .await?;
            credentials::save_tagma(&credentials_dir, tagma_id.as_ref(), &token);
            info!(tagma = %tagma_id, "relay: enrolled with agora");
            (tagma_id, token)
        }
    };

    // Resolve the root agent id (always live: ensure_root_agent ?-propagates).
    let root_agent = {
        let registry = state.registry.read().await;
        let (id, _entry) = registry
            .root_agent()
            .context("root agent missing at relay activation")?;
        id.clone()
    };

    // Default lesche URL to the agora origin if unset (same-origin only).
    let lesche_url = match args.relay_lesche_url.clone() {
        Some(u) => u,
        None => {
            let parsed = url::Url::parse(&agora_url).context("parse agora url")?;
            parsed.origin().ascii_serialization()
        }
    };

    let lesche = kallip_lesche_client::LescheClient::builder(&lesche_url, &tagma_token).build()?;

    let message_limits = relay::MessageLimits {
        max: args
            .relay_message_burst_max
            .unwrap_or(relay::DEFAULT_MESSAGE_BURST_MAX),
        window: std::time::Duration::from_secs(
            args.relay_message_burst_window_secs
                .unwrap_or(relay::DEFAULT_MESSAGE_BURST_WINDOW.as_secs()),
        ),
    };
    let handle = relay::RelayHandle::new(
        lesche,
        tagma_id,
        device,
        root_agent,
        message_limits,
        Arc::downgrade(state),
    );
    info!(tagma = %handle.tagma_id(), "relay connector active");
    let join = tokio::spawn(handle.clone().run(state.shutdown.clone()));
    state.set_relay(handle, join);
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
