mod args;
mod auth;
mod backend;
mod bridge;
mod credentials;
mod cron;
mod delivery;
mod direct;
mod duty;
mod engine;
mod external;
mod inbox;
mod lifecycle;
mod messaging;
mod probe;
mod projector;
mod relay;
pub(crate) mod routes;
mod shutdown;
mod sse;
mod state;
mod token;
mod work_schedule;

#[cfg(test)]
mod test_helpers;

use anyhow::{Context, Result};
use clap::Parser;
use kallip_common::authtoken::MintedToken;
use kallip_runtime::profile::ProfileRegistry;
use state::AppState;
use state::ProfileBundle;
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
    // backend per referenced provider and assemble the registry before restoring agents —
    // restored agents resolve their profile from here too. The tagma owns reqwest + backend
    // construction; the runtime holds the pre-built backends and does selection (plus reuse of
    // `reqwest` types for HTTP-shape retry classification). A
    // misconfigured provider (unknown family, bad config) fails fast here at startup.
    let cfg = kallip_runtime::profile::load().context("failed to load model profiles")?;
    let factory = just_llm_client::client::BackendFactory::new();
    let user_agent = backend::resolve_user_agent(args.llm_api_user_agent.as_deref());
    let source = backend::build_backends(&cfg, factory, user_agent)
        .context("failed to build LLM backends")?;
    let registry = Arc::new(ProfileRegistry::new(cfg.tiers.clone(), source)?);
    let profiles = Arc::new(arc_swap::ArcSwap::from_pointee(ProfileBundle {
        config: cfg,
        registry,
    }));

    let state = Arc::new(AppState::with_limits(
        operator.hash().clone(),
        args.max_agents,
        args.max_subagents,
        args.prompt_queue_size,
        profiles,
        kallip_runtime::config::policy_preset_from_env(),
    ));

    // Load exec-hook rules (builtin preset + exec_hooks.toml overrides,
    // tagma-wide) here, once: a present-but-malformed file panics
    // (fail-closed — the operator asked for hooks and would otherwise
    // silently lose them), and every spawned agent clones this same set.
    // Rule edits take effect on the next start.
    state
        .hook_rules
        .set(Arc::new(kallip_runtime::config::load_exec_hook_rules(
            &exec_hooks_toml_path()?,
        )))
        .ok();

    // Open the work-schedule store and install it on AppState.
    let ws_store = work_schedule::WorkScheduleStore::open(&work_schedule_path()?)
        .await
        .context("open work-schedule store")?;
    state.work_schedules.set(ws_store).ok();

    // Open the inbox store and install it on AppState.
    let inbox_store = inbox::InboxStore::open(&inbox_path()?)
        .await
        .context("open inbox store")?;
    state.inboxes.set(inbox_store).ok();

    // Start the work-schedule engine (sleeps until the next due transition;
    // wakes on store mutations, with a ~60 s heartbeat as clock-divergence net).
    engine::spawn(state.clone());

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
    lifecycle::restore_agents(&state).await?;
    routes::ensure_root_agent(&state).await?;

    // Resolve the data root, credentials, and the tagma's conversation id ONCE
    // before either serving path starts. The conversation id is
    // `ConversationId::for_tagma(tagma_id)` (pure, no network) when stored
    // credentials exist; `None` for a never-enrolled (pure-offline) tagma, in
    // which case there is no durable history (the projector forwards live
    // frames only). Hoisting this out of `init_direct`/`activate_relay` (which
    // each used to do it independently) also collapses the duplicated
    // data-root + credentials-dir setup.
    let (tagma_id, conversation_id) = resolve_identity()?;

    // The single chat_history store, shared by the projector (sole writer) and
    // GC. Opened UNCONDITIONALLY at boot: the projector's persist gate is the
    // conversation id (unset until first-run enrollment resolves it), not the
    // store's presence, so the store is always ready the moment the id lands.
    // On a never-enrolled tagma the file stays empty (writes are id-gated).
    let history = relay::chat_history::open(&chat_history_path()?)
        .await
        .context("open chat_history store")?;

    // The message burst limits (agent `send` rate cap), read from args once.
    let message_limits = relay::MessageLimits {
        max: args
            .relay_message_burst_max
            .unwrap_or(relay::DEFAULT_MESSAGE_BURST_MAX),
        window: std::time::Duration::from_secs(
            args.relay_message_burst_window_secs
                .unwrap_or(relay::DEFAULT_MESSAGE_BURST_WINDOW.as_secs()),
        ),
    };

    // Install the single external projector: the SOLE writer of chat content.
    // Owns the store + conversation id, subscribes to the root broadcast, and
    // publishes stamped frames onto a bus both serving paths forward. The store
    // is always present; the conversation id is `Some` only when stored creds
    // existed at boot (the first-run enroll boot sets it via
    // `set_conversation_id` once enrollment lands — see `activate_relay`).
    let projector = crate::external::ExternalProjector::new(
        Arc::downgrade(&state),
        Some(history.clone()),
        conversation_id,
        tagma_id.clone(),
        // The enrolled label is not yet plumbed from the agora enroll response
        // into local credentials; fall back to "Tagma" until that lands. The
        // tagma_id (the load-bearing part for multi-tagma disambiguation) IS
        // set, so the agent sender is correct.
        None,
        message_limits,
    );
    if state.external.set(projector).is_err() {
        panic!("external projector must be installed once at startup");
    }

    // Best-effort GC sweep (TTL + cap), unconditional so a tagma that shares
    // the unified store does not grow unbounded. Honors the tagma-wide shutdown
    // token; failures are logged inside `gc`, never propagated.
    {
        let shutdown = state.shutdown.clone();
        let history_ttl_days = args
            .relay_history_ttl_days
            .filter(|&v| v > 0)
            .unwrap_or(relay::chat_history::DEFAULT_HISTORY_TTL_DAYS);
        let history_cap = args
            .relay_history_cap
            .filter(|&v| v > 0)
            .unwrap_or(relay::chat_history::DEFAULT_HISTORY_CAP);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let ttl_secs = history_ttl_days.saturating_mul(24 * 3600);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return,
                    _ = interval.tick() => {
                        let reaped = relay::chat_history::gc(&history, ttl_secs, history_cap).await;
                        if reaped > 0 {
                            tracing::info!(reaped, "chat_history gc");
                        }
                    }
                }
            }
        });
    }

    // Always-on direct (local) serving path: serves the external event
    // vocabulary to any local frontend client over a plain SSE. Forwards the
    // projector's bus; owns no store of its own.
    init_direct(&state).await?;

    // Optional online-mode relay: enroll + spawn the connector task. Degrades
    // to local-only on any enrollment failure (logs error, leaves the relay
    // unset; the lesche message route then routes through the projector).
    if let Some(agora_url) = args.relay_agora_url.clone() {
        match activate_relay(&state, &args, agora_url, tagma_id).await {
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

    let listener = tokio::net::TcpListener::bind(&args.listen_addr)
        .await
        .with_context(|| format!("binding listen addr {}", args.listen_addr))?;
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

/// Resolve the data root, create it + the credentials dir owner-only, load any
/// stored tagma credential, and derive the conversation id. Returns
/// `(Option<TagmaId>, Option<ConversationId>)` — both `None` for a
/// never-enrolled (pure-offline) tagma. Centralized here so `init_direct` and
/// `activate_relay` agree on the id, and the data-root/credentials setup runs
/// once. Idempotent (`create_dir_all`).
fn resolve_identity() -> Result<(
    Option<kallip_agora_common::ids::TagmaId>,
    Option<kallip_agora_common::ids::ConversationId>,
)> {
    let data_root = data_root()?;
    std::fs::create_dir_all(&data_root).context("create data root dir")?;
    credentials::set_owner_only(&data_root)?;
    let credentials_dir = credentials_dir()?;
    std::fs::create_dir_all(&credentials_dir).context("create credentials dir")?;
    credentials::set_owner_only(&credentials_dir)?;
    Ok(match credentials::load_tagma(&credentials_dir) {
        Some((id, _token)) => {
            let tid = kallip_agora_common::ids::TagmaId::from(id);
            let cid = kallip_agora_common::ids::ConversationId::for_tagma(&tid);
            (Some(tid), Some(cid))
        }
        None => (None, None),
    })
}

fn data_root() -> Result<std::path::PathBuf> {
    use kallip_runtime::persistence::data_dir_root;
    data_dir_root()
}

fn credentials_dir() -> Result<std::path::PathBuf> {
    data_root().map(|d| d.join("credentials"))
}

/// The single chat_history store path: `<data_root>/chat_history.sqlite`.
fn chat_history_path() -> Result<std::path::PathBuf> {
    data_root().map(|d| d.join("chat_history.sqlite"))
}

/// The work-schedule store path: `<data_root>/work_schedules.sqlite`.
fn work_schedule_path() -> Result<std::path::PathBuf> {
    data_root().map(|d| d.join("work_schedules.sqlite"))
}

/// The inbox store path: `<data_root>/inboxes.sqlite`.
fn inbox_path() -> Result<std::path::PathBuf> {
    data_root().map(|d| d.join("inboxes.sqlite"))
}

/// The tagma-wide exec-hook overrides file: `<data_root>/exec_hooks.toml`.
fn exec_hooks_toml_path() -> Result<std::path::PathBuf> {
    data_root().map(|d| d.join("exec_hooks.toml"))
}

/// Install the [`DirectServing`](crate::direct::DirectServing) handle. Always
/// runs, independent of whether the relay is configured: the direct path
/// serves any local frontend client over a plain SSE. It forwards the external
/// projector's bus and owns no `chat_history` store of its own (the projector
/// is the sole writer).
async fn init_direct(state: &Arc<AppState>) -> Result<()> {
    let projector = state
        .external
        .get()
        .expect("external projector installed before init_direct");
    state.set_direct(crate::direct::DirectServing::new(
        Arc::downgrade(state),
        projector.clone(),
    ));
    Ok(())
}

/// Build and install the relay connector. `stored_tagma_id` is the id resolved
/// at boot when credentials already existed; `None` means first run, in which
/// case the enrollment code is required. The tagma id is reused as-is (not
/// re-loaded). The relay forwards the projector's bus (it does not own a
/// history store); GC runs unconditionally from `main`.
async fn activate_relay(
    state: &Arc<AppState>,
    args: &args::Args,
    agora_url: String,
    stored_tagma_id: Option<kallip_agora_common::ids::TagmaId>,
) -> Result<()> {
    let credentials_dir = credentials_dir()?;
    let device = credentials::load_or_create_device(&credentials_dir)?;

    // Use the id resolved at boot, or enroll on first run.
    let (tagma_id, tagma_token) = match stored_tagma_id {
        Some(id) => {
            let token = credentials::load_tagma(&credentials_dir)
                .map(|(_, t)| t)
                .context("tagma id resolved at boot but credentials missing on relay activation")?;
            info!(tagma = %id, "relay: loaded stored tagma credentials");
            (id, token)
        }
        None => {
            let code = args.relay_enrollment_code.as_deref().context(
                "no stored tagma token; KALLIP_TAGMA_RELAY_ENROLLMENT_CODE required for first run",
            )?;
            let (tagma_id, token) = kallip_agora_client::AgoraClient::builder(&agora_url)
                .build()?
                .enroll(code, &device)
                .await?;
            credentials::save_tagma(&credentials_dir, tagma_id.as_ref(), &token);
            info!(tagma = %tagma_id, "relay: enrolled with agora");
            // First-run enroll boot: the projector was constructed at startup
            // with `conversation_id = None` (creds did not exist yet). Now that
            // enrollment has resolved the tagma id, hand it the derived
            // conversation id so persistence + stamped echoes begin at once.
            // (The loaded-creds boot constructed the projector with the id
            // already set; this branch is the only caller, once.)
            let conv = kallip_agora_common::ids::ConversationId::for_tagma(&tagma_id);
            let projector = state
                .external
                .get()
                .expect("external projector installed before relay activation");
            projector.set_conversation_id(conv);
            projector.set_tagma_id(tagma_id.clone());
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

    // The data-plane client: tagma tunnel, envelope/KEX POSTs, room discovery
    // (the chat domain lives in lesche now). Built once and shared by the room
    // routes and the relay orchestrator (a cheap clone of the shared reqwest
    // pool + bearer).
    let lesche = kallip_lesche_client::LescheClient::builder(&lesche_url, &tagma_token).build()?;

    let handle = relay::RelayHandle::new(
        lesche,
        tagma_id,
        // Label fallback (see projector construction): "Tagma" until the enrolled
        // label is plumbed through.
        "Tagma".to_string(),
        device,
        root_agent,
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
