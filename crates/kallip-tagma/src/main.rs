//! Shared-state conventions: `std::sync::Mutex` guards short critical
//! sections that never span an `.await` (small cells: state bytes, optional
//! snapshots); `tokio::sync::Mutex` guards state held across `.await`
//! points (stores, approval tables). Field-level docs at each use site name
//! the writers and readers; this note is the rule they assume.

mod args;
mod auth;
mod backend;
mod bridge;
mod bus;
mod credentials;
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
mod pump_driver;
mod relay;
pub(crate) mod routes;
mod shutdown;
mod sse;
mod state;
mod task_watcher;
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
use tracing::{info, warn};

use args::Args;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    init_logging(&filter);

    // The whole body lives in `run` so a fatal error at any point —
    // startup or the serving loop — is logged through the subscriber
    // before main returns it; the Rust runtime's stderr report alone
    // would bypass the instance log file. Returning the error keeps
    // the exit code; in the stderr-logging mode the runtime line and
    // this one differ in shape (debug form carries the source chain).
    if let Err(e) = run(args).await {
        tracing::error!(error = format!("{e:#}"), "fatal error, exiting");
        return Err(e);
    }
    Ok(())
}

async fn run(args: Args) -> Result<()> {
    // Identity first: everything below (log placement, profile config, the
    // data root itself) hangs off the slug-derived tree, so an unnamed or
    // legacy-addressed boot must fail before it touches the filesystem.
    boot_identity()?;
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
    let registry = Arc::new(ProfileRegistry::new(cfg.sets.clone(), source)?);
    let profiles = Arc::new(arc_swap::ArcSwap::from_pointee(ProfileBundle {
        config: cfg,
        registry,
    }));

    // Startup token budget: the env read and parse happen here (not inside
    // AppState::with_limits) so an invalid KALLIP_TOKEN_BUDGET fails the
    // boot through this anyhow chain — a clean operator-facing error, not a
    // panic with a backtrace. Unset boots unlimited; `0` boots paused.
    let token_budget = AppState::startup_token_budget(std::env::var("KALLIP_TOKEN_BUDGET").ok())
        .context("invalid KALLIP_TOKEN_BUDGET")?;
    let state = Arc::new(AppState::with_limits(
        operator.hash().clone(),
        args.max_agents,
        args.max_subagents,
        args.prompt_queue_size,
        profiles,
        kallip_runtime::config::policy_preset_from_env(),
        token_budget,
    ));

    // The typed topic bus registered inline at construction; surface the
    // topic set once at boot (this is also the counters' live reader).
    info!(topics = ?state.bus.stats(), "typed topic bus registered");
    // Load exec-hook rules (builtin preset + exec_hooks.toml overrides,
    // tagma-wide) here, once: a present-but-malformed file panics
    // (fail-closed — the operator asked for hooks and would otherwise
    // silently lose them), and every spawned agent clones this same set.
    // Rule edits take effect on the next start.
    // A missing config root (no slug / no config home) degrades to the
    // builtin preset with a warning: hooks are an operator addition, and
    // their absence must not block a boot that could otherwise run.
    let hook_rules = match exec_hooks_toml_path() {
        Ok(path) => kallip_runtime::config::load_exec_hook_rules(&path),
        Err(error) => {
            tracing::warn!(
                %error,
                "exec-hook overrides unavailable; builtin preset only"
            );
            kallip_runtime::config::builtin_exec_hook_rules()
        }
    };
    state.hook_rules.set(Arc::new(hook_rules)).ok();

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

    // Open the task coordination store and install it on AppState.
    // `TaskStore::open` runs the migration chain, so the boot brings the
    // schema to head. This process is the SOLE writer of tasks.sqlite.
    let task_store = kallip_task::TaskStore::open(
        &kallip_runtime::persistence::data_dir_root()?.join("tasks.sqlite"),
    )
    .await
    .context("open task store")?;
    state.tasks.set(Arc::new(task_store)).ok();
    state
        .task_blobs
        .set(kallip_blob_store::LocalBackend::arc(
            kallip_runtime::persistence::data_dir_root()?.join("task-blobs"),
        ))
        .ok();

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
    // A root that failed to restore boots the tagma degraded (registered
    // faulted, fixable via the API); contradictory or unreadable disk state
    // aborts the boot here instead of risking a second root.
    lifecycle::restore_agents(&state).await?;
    routes::ensure_root_agent(&state).await?;

    // Resolve the relay plan (entries + per-entry boot decisions) ONCE
    // before either serving path starts: the projector's identity below is
    // the primary archeion's (the first entry with stored credentials), and
    // `None` ids on an all-fresh deployment are claimed by the first
    // successful enrollment. Every configuration error aborts boot here —
    // the per-entry local-only degrade later must not swallow one.
    let relay_plan = resolve_relay_plan(&args)?;
    let (tagma_id, conversation_id) = resolve_primary_identity(&relay_plan)?;

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
        // TODO(label-plumb): the enrolled label is not yet plumbed from the
        // archeion enroll response into local credentials; fall back to
        // "Tagma" until that lands (paired site: the RelayHandle::new call in
        // `activate_relay`). The tagma_id (the load-bearing part for
        // multi-tagma disambiguation) IS set, so the agent sender is correct.
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

    // Task-ledger change watcher: turns task-route writes into wake hints
    // for the task's people (see task_watcher). Best-effort by contract —
    // a dropped hint costs one missed wake, never data.
    tokio::spawn(task_watcher::run(state.clone()));

    // Always-on direct (local) serving path: serves the external event
    // vocabulary to any local frontend client over a plain SSE. Forwards the
    // projector's bus; owns no store of its own.
    init_direct(&state).await?;

    // Activate each relay entry serially (enroll is a network call;
    // serial keeps failure attribution clear and boot logs readable).
    // Runtime activation failures degrade that entry alone to local-only
    // (logged, skipped); configuration errors already aborted boot above.
    for (entry, boot) in relay_plan {
        if let Err(e) = activate_relay(&state, &entry, boot).await {
            tracing::error!(
                relay = %entry.name,
                "relay activation failed, entry degraded to local-only: {e:#}"
            );
        }
    }

    let app = routes::router().with_state(state.clone());

    // Layering: each `.layer` wraps what was built so far, so the LAST layer
    // added (TraceLayer) is outermost and the body size limit is innermost.
    // The limit is enforced when the body is extracted, and neither CORS nor
    // tracing consumes it in passing. When max_body_size_kb > 0, enforce the
    // configured limit; when 0, axum's built-in default (2 MB) applies.
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
    // Resolve the advertised URL: explicit --advertise-url wins; otherwise
    // derive it from the bound socket so an ephemeral port advertises its
    // real value instead of the historical 3000 default. Safe to publish
    // here: agents spawn only via API handlers once the server is serving.
    let bound_addr = listener
        .local_addr()
        .context("reading the bound local address")?;
    let advertise_url = match args.advertise_url.clone() {
        Some(url) => url,
        None => derive_advertise_url(&args.listen_addr, bound_addr.port())?,
    };
    unsafe {
        std::env::set_var("KALLIP_TAGMA_URL", &advertise_url);
    }
    info!(
        addr = %bound_addr,
        advertise = %advertise_url,
        "tagma listening"
    );
    // Publish the runtime identity (pid + bound port) into the instance dir
    // right after the bind — unconditionally now: every boot carries a slug,
    // so every boot owns an instance dir the daemon (or an operator) can
    // discover it by. There is no unmarked mode left to stay silent in.
    write_instance_state(&data_root()?, &listener)?;
    let shutdown_token = state.shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_token))
        .await?;

    // Drain the relay task first (it tears down the tunnel + pump), then the
    // agents. Both observe the tagma-wide `shutdown` token.
    shutdown::drain_relays(&state).await;
    shutdown::graceful_agent_shutdown(&state).await;
    info!("tagma exited");

    Ok(())
}

/// Create the data root + the credentials dir (both owner-only) and return
/// the credentials dir path. Every entry's subdirectory hangs off it; the
/// shared `device.key` lives at its root. Idempotent (`create_dir_all`).
fn ensure_credentials_root() -> Result<std::path::PathBuf> {
    let data_root = data_root()?;
    std::fs::create_dir_all(&data_root).context("create data root dir")?;
    credentials::set_owner_only(&data_root)?;
    let credentials_dir = credentials_dir()?;
    std::fs::create_dir_all(&credentials_dir).context("create credentials dir")?;
    credentials::set_owner_only(&credentials_dir)?;
    Ok(credentials_dir)
}

/// This process's instance identity: `KALLIP_TAGMA_SLUG` is mandatory and
/// grammar-checked (`kallip_daemon_common::wire::valid_slug` — the same
/// rule that names the daemon's instance tree).
fn boot_identity() -> Result<std::path::PathBuf> {
    let slug = std::env::var("KALLIP_TAGMA_SLUG")
        .ok()
        .filter(|s| !s.is_empty())
        .context(
            "KALLIP_TAGMA_SLUG is not set; name this instance with KALLIP_TAGMA_SLUG \
             (lowercase letters, digits and '-', <=64 chars, starting with a letter or digit)",
        )?;
    anyhow::ensure!(
        kallip_daemon_common::wire::valid_slug(&slug),
        "KALLIP_TAGMA_SLUG {slug:?} does not match [a-z0-9][a-z0-9-]* (<= 64 chars)"
    );
    let data_root = kallip_runtime::persistence::data_dir_root()?;
    Ok(data_root)
}

/// Where this tagma's log files live: always the state tree —
/// `<state_root>/tagmata/<slug>/logs`, mirroring how the daemon names the
/// tree. Logs are pure output residue, so they sit outside the instance
/// data tree; the slug is the process identity, so there is exactly one
/// placement. `Err` means the directory cannot be placed (state home
/// undetermined) and callers degrade to stderr-only.
fn logs_target() -> Result<std::path::PathBuf> {
    let slug = std::env::var("KALLIP_TAGMA_SLUG")
        .ok()
        .filter(|s| !s.is_empty())
        .context("cannot place logs: KALLIP_TAGMA_SLUG is not set")?;
    Ok(kallip_runtime::persistence::state_dir_root()?
        .join("tagmata")
        .join(slug)
        .join("logs"))
}

/// Logging shape: events land under the resolved log directory
/// (daily-rolling files, tracing-appender, 7 files kept -- older
/// days fall off) unless `KALLIP_TAGMA_LOG_TO_STDERR` asks for the
/// terminal; stdout stays reserved for program output, so the
/// terminal layer writes stderr. The variable accepts `1`/`true`
/// case-insensitively; unset, empty, or any other value keeps the
/// file default. A log directory that cannot be resolved or created
/// degrades to stderr-only with a one-line eprintln notice. The
/// panic hook chains the default stderr banner exactly when stderr
/// is the writer -- the variable, or that fallback -- and suppresses
/// it while the file layer is live: the details live in the log file.
fn init_logging(filter: &tracing_subscriber::EnvFilter) {
    use tracing_subscriber::prelude::*;

    let stderr_layer = || {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(filter.clone())
    };
    let file_layer = if log_to_stderr_from_env() {
        None
    } else {
        logs_target()
            .map_err(|e| {
                eprintln!("kallip-tagma: resolving the log dir failed, keeping stderr-only: {e}");
                e
            })
            .ok()
            .and_then(|dir| {
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    eprintln!(
                        "kallip-tagma: creating the log dir failed, keeping stderr-only: {e}"
                    );
                    return None;
                }
                if let Err(e) = credentials::set_owner_only(&dir) {
                    eprintln!(
                        "kallip-tagma: restricting the log dir failed, keeping stderr-only: {e}"
                    );
                    return None;
                }
                let appender = tracing_appender::rolling::Builder::new()
                    .rotation(tracing_appender::rolling::Rotation::DAILY)
                    .filename_prefix("instance")
                    .filename_suffix("log")
                    .max_log_files(7)
                    .build(&dir)
                    .map_err(|e| {
                        eprintln!("kallip-tagma: file log init failed, keeping stderr-only: {e}");
                        e
                    })
                    .ok()?;
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::sync::Mutex::new(appender))
                        .with_ansi(false)
                        .with_filter(filter.clone()),
                )
            })
    };
    // Captured before the match moves the layer: the hook chains the
    // default hook whenever stderr is the writer.
    let file_layer_live = file_layer.is_some();
    match file_layer {
        // The file layer alone carries the events.
        Some(file) => {
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(file)).ok()
        }
        // The variable asked for stderr, or the file layer failed to build.
        None => tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(stderr_layer()),
        )
        .ok(),
    };

    // Forward panics into the subscriber: the default hook's stderr write
    // bypasses tracing entirely, so a crash would miss the subscriber's
    // writer. Any stderr-writer state -- the variable, or the fallback --
    // still chains the default hook: the foreground operator expects the
    // stderr banner. With the file layer live it is suppressed instead:
    // the details live in the log file. The forwarding half lives in
    // `forward_panic_to_tracing` so tests can exercise it against an in-
    // memory writer without touching the process-global default.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        forward_panic_to_tracing(info);
        if !file_layer_live {
            hook(info);
        }
    }));
}

/// `KALLIP_TAGMA_LOG_TO_STDERR`: `1`/`true` (case-insensitive) turns on
/// the terminal (stderr) writer; unset, empty, or any other value keeps
/// the file default.
fn parse_log_to_stderr(value: Option<&str>) -> bool {
    match value {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true"),
        None => false,
    }
}

fn log_to_stderr_from_env() -> bool {
    parse_log_to_stderr(std::env::var("KALLIP_TAGMA_LOG_TO_STDERR").ok().as_deref())
}

fn forward_panic_to_tracing(info: &std::panic::PanicHookInfo) {
    let msg = info.to_string();
    let loc = info
        .location()
        .map(|l| l.to_string())
        .unwrap_or_else(|| "unknown location".to_string());
    tracing::error!(%loc, %msg, "panic");
}

/// `runtime.json` payload — the tagma-written half of the data-dir
/// contract (the tagma instance writes `runtime.json`; the daemon reads
/// it through the record's data-directory pointer). kallip-daemon
/// deserializes its own mirror of these keys, so the two definitions
/// must stay in lockstep. `starttime` is the kernel start time of this
/// process: the self-reported incarnation, kept verbatim in the
/// contract while the daemon verifies liveness against its own `/proc`
/// read. `0` means not captured.
#[derive(serde::Serialize)]
struct InstanceRuntime {
    pid: u32,
    port: u16,
    starttime: u64,
}
/// The kernel start time of this process (`/proc/self/stat` field 22,
/// clock ticks since boot) — the same incarnation discriminator the
/// daemon parses from `/proc/<pid>/stat`. Parsed after the comm's
/// closing paren because comm may contain spaces and parentheses of
/// its own. `None` off Linux or on a read/parse failure; the runtime
/// file then carries `0`.
fn proc_starttime() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let after_comm = stat.rfind(')')? + 1;
    stat[after_comm..].split_whitespace().nth(19)?.parse().ok()
}
/// Build the advertise URL for an unset `--advertise-url` from the bound
/// socket: scheme http, the listen address's host (an unspecified host
/// such as 0.0.0.0 or :: becomes 127.0.0.1 -- loopback is the only host a
/// same-machine agent shell can assume), IPv6 hosts bracketed, and the
/// actually-bound port. Same truth source as `write_instance_state`: the
/// port is the one actually bound (read back from the listener), so the
/// advertised URL and `runtime.json` always agree.
fn derive_advertise_url(listen_addr: &str, port: u16) -> Result<String> {
    use std::net::IpAddr;
    let host = match listen_addr.rsplit_once(':') {
        Some((host, _)) if !host.is_empty() => host.to_string(),
        _ => "127.0.0.1".to_string(),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let ip: Option<IpAddr> = host.parse().ok();
    let bracketed_host = match ip {
        Some(IpAddr::V6(v6)) if v6.is_unspecified() => "127.0.0.1".to_string(),
        Some(IpAddr::V6(_)) => format!("[{host}]"),
        Some(IpAddr::V4(v4)) if v4.is_unspecified() => "127.0.0.1".to_string(),
        _ => host.to_string(),
    };
    Ok(format!("http://{bracketed_host}:{port}"))
}

/// Publish the runtime identity of this instance (pid, actually bound
/// port, kernel start time) into `dir` as a single `runtime.json`, for
/// the local instance daemon's spawn pipeline. The key set is a
/// cross-crate contract with the daemon's reader, kept in lockstep by
/// hand — tagma does not depend on the daemon crates. Owner-only via
/// tmp+rename; the port is read back from the listener so a `:0` bind
/// is discoverable.
fn write_instance_state(dir: &std::path::Path, listener: &tokio::net::TcpListener) -> Result<()> {
    let port = listener
        .local_addr()
        .context("reading the bound local address")?
        .port();
    let runtime = InstanceRuntime {
        pid: std::process::id(),
        port,
        starttime: proc_starttime().unwrap_or(0),
    };
    write_owner_only(&dir.join("runtime.json"), &serde_json::to_string(&runtime)?)
        .context("writing the instance runtime file")?;
    info!(pid = runtime.pid, port, state_dir = %dir.display(), "instance state published");
    Ok(())
}

/// Write `text` to `path` atomically (tmp file + rename), owner-only.
/// No fsync: these are runtime hints — a torn write is at worst a stale
/// value, which the daemon's pid-liveness check already treats as
/// "not running".
fn write_owner_only(path: &std::path::Path, text: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating state tmp file {}", tmp.display()))?;
    file.write_all(text.as_bytes())?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming state file into {}", path.display()))?;
    Ok(())
}

/// The projector's single-value identity is the **primary archeion** concept:
/// the first relay entry (config order) that has stored credentials at boot.
/// It backs the frontend cache key and the offline fallback stamp only —
/// every relay stamps its own wire identity on the envelope (see the pump).
/// All-fresh deployments resolve `(None, None)` and the first successful
/// enrollment claims the ids (write-once, first wins).
fn resolve_primary_identity(
    plan: &[(RelayEntry, EnrollEntry)],
) -> Result<(
    Option<kallip_archeion_common::ids::TagmaId>,
    Option<kallip_archeion_common::ids::ConversationId>,
)> {
    let root = credentials_dir()?;
    for (entry, _) in plan {
        if let Some(stored) = credentials::load_tagma(&root.join(&entry.name)) {
            let tid = kallip_archeion_common::ids::TagmaId::from(stored.id);
            let cid = kallip_archeion_common::ids::ConversationId::for_tagma(&tid);
            return Ok((Some(tid), Some(cid)));
        }
    }
    Ok((None, None))
}

/// One configured relay entry: one archeion identity. `name` is the stable slug
/// that keys the credentials subdirectory and the AppState relay slot (so a
/// URL change never moves the identity directory).
#[derive(Debug, Clone, PartialEq)]
struct RelayEntry {
    name: String,
    archeion_url: String,
    lesche_url: Option<String>,
    enrollment_code: Option<String>,
}

/// The `[[relay]]` array of `relays.toml`.
#[derive(serde::Deserialize)]
struct RelaysFile {
    #[serde(default)]
    relay: Vec<RelaysTomlEntry>,
}

#[derive(serde::Deserialize)]
struct RelaysTomlEntry {
    name: String,
    archeion_url: String,
    lesche_url: Option<String>,
    enrollment_code: Option<String>,
}

/// A relay name must be a DNS-label-like slug (`[a-z0-9][a-z0-9-]*`): it is a
/// path component (`credentials/<name>/`) and a log key, so it stays flat,
/// lowercase, and unambiguous.
fn valid_entry_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Resolve the configured relay entries: `<data_root>/relays.toml` when
/// present, else the legacy single-entry sugar (`KALLIP_TAGMA_RELAY_*` env /
/// `--relay-*` flags → one implicit entry named "default"). Fail-fast cases
/// (all configuration errors):
///
/// - the file and any legacy relay env/flag are both set (which source
///   wins must never be implicit);
/// - the file exists but declares zero `[[relay]]` entries;
/// - a name fails the slug grammar or repeats.
fn resolve_relay_entries(args: &args::Args) -> Result<Vec<RelayEntry>> {
    let toml_path = data_root()?.join("relays.toml");
    let legacy_set = args.relay_archeion_url.is_some()
        || args.relay_lesche_url.is_some()
        || args.relay_enrollment_code.is_some();
    if toml_path.exists() {
        anyhow::ensure!(
            !legacy_set,
            concat!(
                "conflicting relay configuration: {} exists but KALLIP_TAGMA_RELAY_*",
                " env vars / --relay-* flags are also set; move the entries into the",
                " file or unset the legacy vars"
            ),
            toml_path.display()
        );
        let raw = std::fs::read_to_string(&toml_path)
            .with_context(|| format!("read {}", toml_path.display()))?;
        let file: RelaysFile =
            toml::from_str(&raw).with_context(|| format!("parse {}", toml_path.display()))?;
        anyhow::ensure!(
            !file.relay.is_empty(),
            "{} declares no [[relay]] entries; add one or delete the file",
            toml_path.display()
        );
        let mut seen = std::collections::HashSet::new();
        let mut entries = Vec::with_capacity(file.relay.len());
        for e in file.relay {
            anyhow::ensure!(
                valid_entry_name(&e.name),
                concat!(
                    "relay name {:?} is not a slug matching [a-z0-9][a-z0-9-]*",
                    " (it is a credentials/<name>/ path component)"
                ),
                e.name
            );
            anyhow::ensure!(
                seen.insert(e.name.clone()),
                "relay name {:?} repeats",
                e.name
            );
            entries.push(RelayEntry {
                name: e.name,
                archeion_url: e.archeion_url,
                lesche_url: e.lesche_url,
                enrollment_code: e.enrollment_code,
            });
        }
        Ok(entries)
    } else if legacy_set {
        // Single-entry sugar: the pre-multi-archeion deployment keeps working
        // untouched, as one entry named "default".
        let archeion_url = args
            .relay_archeion_url
            .clone()
            .context("KALLIP_TAGMA_RELAY_LESCHE_URL / _ENROLLMENT_CODE set without KALLIP_TAGMA_RELAY_ARCHEION_URL; set the archeion url too")?;
        Ok(vec![RelayEntry {
            name: "default".to_string(),
            archeion_url,
            lesche_url: args.relay_lesche_url.clone(),
            enrollment_code: args.relay_enrollment_code.clone(),
        }])
    } else {
        Ok(Vec::new()) // local-only: no relay entries
    }
}

/// Parse + fully validate the relay configuration: entries (see
/// [`resolve_relay_entries`]), the one-time legacy-credentials migration, and
/// each entry's boot decision (reuse stored vs first-run enroll). Every
/// failure is a configuration error and aborts boot — the per-entry
/// local-only degrade in `main` is for runtime activation failures only.
fn resolve_relay_plan(args: &args::Args) -> Result<Vec<(RelayEntry, EnrollEntry)>> {
    let entries = resolve_relay_entries(args)?;
    let root = ensure_credentials_root()?;
    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    credentials::migrate_legacy_layout(&root, &names)?;
    let mut plan = Vec::with_capacity(entries.len());
    for entry in entries {
        let dir = root.join(&entry.name);
        let stored = credentials::load_tagma(&dir);
        let boot = resolve_enroll_entry(
            stored.as_ref(),
            &entry.archeion_url,
            entry.enrollment_code.as_deref(),
            &dir,
        )?;
        plan.push((entry, boot));
    }
    Ok(plan)
}

/// The instance data root, slug-derived (see `boot_identity`).
fn data_root() -> Result<std::path::PathBuf> {
    kallip_runtime::persistence::data_dir_root()
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

/// The tagma-wide exec-hook overrides file:
/// `<config_root>/exec_hooks.toml` (operator-declared config lives in the
/// config tree; the runtime data tree stays data-only).
fn exec_hooks_toml_path() -> Result<std::path::PathBuf> {
    kallip_runtime::persistence::config_dir_root().map(|d| d.join("exec_hooks.toml"))
}

/// Start the direct status driver. Always runs, independent of whether the
/// relay is configured: the direct path serves any local frontend client
/// over a plain SSE. It forwards the typed topic bus's topics and owns no
/// `chat_history` store of its own (the projector is the sole writer).
async fn init_direct(state: &Arc<AppState>) -> Result<()> {
    crate::direct::start(&Arc::downgrade(state));
    Ok(())
}

/// How the relay connector enters the archeion at boot.
#[derive(Debug, PartialEq)]
enum EnrollEntry {
    /// Reuse the credentials persisted by a prior enrollment.
    Stored,
    /// Stored credentials plus an enrollment code at the same archeion (or
    /// with the enrollment origin unrecorded): the stale false-alarm
    /// shape. The code is ignored with a loud warning instead of failing
    /// the boot — consumed material left in the environment must not
    /// brick restarts.
    StoredIgnoringCode { address_recorded: bool },
    /// No stored credentials: redeem `code` for a fresh enrollment.
    Fresh { code: String },
}

/// Decide the boot-time enrollment entry: reuse stored credentials or run
/// a first-run enrollment with a code. Misconfigured states fail fast
/// instead of degrading to local-only, because the degradation would hide
/// a configuration error behind a confusing runtime failure later (a
/// silently ignored stale token pointed at a new archeion loops 401
/// reconnects forever):
///
/// - stored credentials + enrollment code at a *different* archeion: the code
///   would mint a second identity while the stored one points at the old
///   archeion, so both exits are named;
/// - neither credentials nor code: there is nothing to connect with.
///
/// Stored credentials + a code at the *same* archeion (or with the enrollment
/// origin unrecorded — credentials that predate origin recording) is the
/// stale false-alarm shape: the code is ignored with a loud warning
/// (see `ignored_code_warning`), not an error.
fn resolve_enroll_entry(
    stored: Option<&credentials::StoredTagma>,
    configured_archeion_url: &str,
    code: Option<&str>,
    credentials_dir: &std::path::Path,
) -> Result<EnrollEntry> {
    match (stored, code) {
        (Some(stored), Some(_)) => {
            let compared =
                origin_comparison(stored.archeion_url.as_deref(), configured_archeion_url);
            match compared {
                None | Some(true) => Ok(EnrollEntry::StoredIgnoringCode {
                    address_recorded: compared.is_some(),
                }),
                Some(false) => Err(anyhow::anyhow!(
                    "conflicting relay configuration: stored credentials in {} were \
                     enrolled at {} but KALLIP_TAGMA_RELAY_ENROLLMENT_CODE is set and \
                     the archeion is now {}; delete the credentials directory to \
                     re-enroll, or unset the code to reuse the stored identity",
                    credentials_dir.display(),
                    stored.archeion_url.as_deref().unwrap_or("<unrecorded>"),
                    configured_archeion_url
                )),
            }
        }
        (Some(_), None) => Ok(EnrollEntry::Stored),
        (None, Some(code)) => Ok(EnrollEntry::Fresh {
            code: code.to_owned(),
        }),
        (None, None) => Err(anyhow::anyhow!(
            "incomplete relay configuration: KALLIP_TAGMA_RELAY_ARCHEION_URL is \
             set but there are no stored credentials and no \
             KALLIP_TAGMA_RELAY_ENROLLMENT_CODE; either set the code for a \
             first-run enrollment, or unset the URL to run local-only"
        )),
    }
}

/// Parsed origin comparison (scheme + host + port — the same
/// normalization the lesche default uses): `None` means unknown (an
/// unrecorded or unparsable origin on either side), `Some(verdict)` a real
/// comparison. Unknown is treated as same by the caller: origin data is
/// hygiene, and bricking a boot over it would trade availability for
/// nothing — the loud warning still names the recovery.
fn origin_comparison(stored: Option<&str>, configured: &str) -> Option<bool> {
    let stored = stored?;
    let (Ok(stored), Ok(configured)) = (url::Url::parse(stored), url::Url::parse(configured))
    else {
        return None;
    };
    Some(stored.origin() == configured.origin())
}

/// Warning text for the same-archeion stale-code shape. Loud by design: the
/// silent-ignore alternative is exactly the 401 loop the fail-fast guards
/// against, so the operator gets the verdict, the reused identity, and
/// the recovery (deleting the credentials directory re-enrolls) in one
/// line.
fn ignored_code_warning(credentials_dir: &std::path::Path, address_recorded: bool) -> String {
    let origin = if address_recorded {
        "enrolled at this archeion"
    } else {
        "enrolled before origin recording (origin now backfilled from config)"
    };
    format!(
        "KALLIP_TAGMA_RELAY_ENROLLMENT_CODE ignored: stored credentials in {} \
         are {origin}; using the stored identity. To re-enroll, delete the \
         credentials directory",
        credentials_dir.display()
    )
}

/// Build and install one relay connector. `entry` is the config entry (name,
/// archeion/lesche URLs, first-run code) and `boot` the fail-fast decision from
/// `resolve_relay_plan` (reuse stored credentials, or first-run enrollment
/// with a code); runtime failures propagate to `main`'s per-entry degrade.
/// The relay forwards the projector's bus (it does not own a history
/// store); GC runs unconditionally from `main`.
async fn activate_relay(
    state: &Arc<AppState>,
    entry: &RelayEntry,
    boot: EnrollEntry,
) -> Result<()> {
    let root = credentials_dir()?;
    let entry_dir = root.join(&entry.name);
    std::fs::create_dir_all(&entry_dir).context("create entry credentials dir")?;
    credentials::set_owner_only(&entry_dir)?;
    // The stale-code verdict lands here rather than in the resolver so it
    // is emitted after logging is up and next to the entry that hit it.
    if let EnrollEntry::StoredIgnoringCode { address_recorded } = boot {
        warn!(
            relay = %entry.name,
            "{}",
            ignored_code_warning(&entry_dir, address_recorded)
        );
    }
    // One device, many identities: the Ed25519 device key is shared across
    // archeions (it proves "same physical tagma"), so it lives at the
    // credentials root, while each archeion's (tagma.id, tagma.token) pair
    // lives in the entry's subdirectory.
    let device = credentials::load_or_create_device(&root)?;

    // The boot decision (reuse stored credentials vs first-run enroll) was
    // made by the caller via `resolve_enroll_entry`; this only executes it.
    let (tagma_id, tagma_token) = match boot {
        EnrollEntry::Stored | EnrollEntry::StoredIgnoringCode { .. } => {
            let stored = credentials::load_tagma(&entry_dir)
                .context("stored credentials missing at relay activation (deleted after boot?)")?;
            // Both stored arms converge here, so the origin backfill runs on
            // every stored-credential boot: credentials from before origin
            // recording get the configured origin recorded once (a mislabel
            // behaves exactly like the unknown path; the next address change
            // regains the precise verdict).
            credentials::backfill_archeion_url(&entry_dir, &entry.archeion_url);
            info!(relay = %entry.name, tagma = %stored.id, "relay: loaded stored tagma credentials");
            (
                kallip_archeion_common::ids::TagmaId::from(stored.id),
                stored.token,
            )
        }
        EnrollEntry::Fresh { code } => {
            let (tagma_id, token) =
                kallip_archeion_client::ArcheionClient::builder(&entry.archeion_url)
                    .build()?
                    .enroll(&code, &device)
                    .await?;
            credentials::save_tagma(&entry_dir, tagma_id.as_ref(), &token, &entry.archeion_url);
            info!(relay = %entry.name, tagma = %tagma_id, "relay: enrolled with archeion");
            // First-run enroll boot: the projector's write-once ids are
            // claimed by the first successful enrollee — the primary-archeion
            // concept (see `resolve_primary_identity`). A loaded-creds boot
            // constructed the projector with the ids already set, and on an
            // all-fresh multi-entry boot a later enrollee's setters are
            // no-ops on the OnceLocks, by design.
            let conv = kallip_archeion_common::ids::ConversationId::for_tagma(&tagma_id);
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

    // Default lesche URL to the archeion origin if unset (same-origin only).
    let lesche_url = match entry.lesche_url.clone() {
        Some(u) => u,
        None => {
            let parsed = url::Url::parse(&entry.archeion_url).context("parse archeion url")?;
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
        entry.name.clone(),
        tagma_id,
        // TODO(label-plumb): paired with the projector-construction site —
        // "Tagma" until the enrolled label is plumbed through.
        "Tagma".to_string(),
        device,
        root_agent,
        Arc::downgrade(state),
    );
    info!(
        relay = %entry.name,
        tagma = %handle.tagma_id(),
        archeion_url = %entry.archeion_url,
        lesche_url = %lesche_url,
        "relay connector active"
    );

    let join = tokio::spawn(handle.clone().run(state.shutdown.clone()));
    state.set_relay(&entry.name, handle, join);
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
    let signal = tokio::select! {
        _ = ctrl_c => "SIGINT",
        _ = sigterm => "SIGTERM",
    };
    info!(
        signal,
        "received shutdown signal, initiating graceful shutdown"
    );
    token.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn boot_refuses_to_start_without_a_slug() {
        temp_env::with_vars_unset(["KALLIP_TAGMA_SLUG"], || {
            let err = boot_identity().unwrap_err();
            assert!(
                err.to_string().contains("KALLIP_TAGMA_SLUG is not set"),
                "{err:#}"
            );
        });
    }

    #[test]
    fn boot_refuses_an_invalid_slug() {
        temp_env::with_vars([("KALLIP_TAGMA_SLUG", Some("Bad_Slug"))], || {
            let err = boot_identity().unwrap_err();
            assert!(err.to_string().contains("does not match"), "{err:#}");
        });
    }

    #[test]
    fn boot_derives_the_data_root_from_the_slug() {
        let tmp = tempfile::tempdir().unwrap();
        temp_env::with_vars(
            [
                ("KALLIP_TAGMA_SLUG", Some("e2e")),
                ("XDG_DATA_HOME", Some(tmp.path().to_str().unwrap())),
            ],
            || {
                let root = boot_identity().unwrap();
                assert_eq!(
                    root,
                    tmp.path().join("kallipai").join("tagmata").join("e2e")
                );
            },
        );
    }

    #[test]
    fn logs_land_in_the_state_tree_under_the_slug() {
        temp_env::with_vars(
            [
                ("KALLIP_TAGMA_SLUG", Some("e2e")),
                ("XDG_STATE_HOME", Some("/state/home")),
            ],
            || {
                assert_eq!(
                    logs_target().unwrap(),
                    PathBuf::from("/state/home/kallipai/tagmata/e2e/logs")
                );
            },
        );
    }

    #[test]
    fn logs_are_unplaceable_without_a_slug() {
        temp_env::with_vars_unset(["KALLIP_TAGMA_SLUG"], || {
            assert!(logs_target().is_err(), "no slug, no log placement");
        });
    }

    fn stored(id: &str, origin: Option<&str>) -> credentials::StoredTagma {
        credentials::StoredTagma {
            id: id.to_string(),
            token: "token".to_string(),
            archeion_url: origin.map(str::to_string),
        }
    }

    /// Stored credentials and no code: the stored identity is reused.
    #[test]
    fn stored_credentials_are_reused_without_a_code() {
        let entry = resolve_enroll_entry(
            Some(&stored("tagma-test", Some("https://archeion.example.com"))),
            "https://archeion.example.com",
            None,
            std::path::Path::new("/tmp/credentials"),
        )
        .expect("stored entry resolves");
        assert_eq!(entry, EnrollEntry::Stored);
    }

    /// The forwarding hook surfaces a caught panic as a `panic` error
    /// event. Captured through an in-memory writer so the test has real
    /// discriminating power: if the forward regresses to the default
    /// stderr-only path, the buffer stays empty and the assert fires.
    #[test]
    fn panic_forward_writes_error_event() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::prelude::*;

        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
            type Writer = MutexGuardWriter<'a>;
            fn make_writer(&'a self) -> Self::Writer {
                MutexGuardWriter(self.0.lock().unwrap())
            }
        }
        struct MutexGuardWriter<'a>(std::sync::MutexGuard<'a, Vec<u8>>);
        impl std::io::Write for MutexGuardWriter<'_> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.flush()
            }
        }

        let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(SharedBuf(shared.clone()))
                .with_filter(tracing_subscriber::EnvFilter::new("error")),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        std::panic::set_hook(Box::new(forward_panic_to_tracing));
        let _ = std::panic::catch_unwind(|| panic!("hook probe"));

        let captured =
            String::from_utf8(shared.lock().unwrap().clone()).expect("log bytes are utf8");
        assert!(
            captured.contains("panic"),
            "missing panic event: {captured}"
        );
        assert!(captured.contains("hook probe"), "payload lost: {captured}");
    }

    /// The env switch accepts 1/true case-insensitively and nothing else:
    /// unset, empty, 0, yes, or padded values all keep the file default.
    #[test]
    fn log_to_stderr_accepts_only_1_and_true() {
        assert!(parse_log_to_stderr(Some("1")));
        assert!(parse_log_to_stderr(Some("true")));
        assert!(parse_log_to_stderr(Some("TRUE")));
        assert!(parse_log_to_stderr(Some("True")));
        assert!(!parse_log_to_stderr(None));
        assert!(!parse_log_to_stderr(Some("")));
        assert!(!parse_log_to_stderr(Some("0")));
        assert!(!parse_log_to_stderr(Some("yes")));
        assert!(!parse_log_to_stderr(Some("1 ")));
    }

    /// Derive covers the documented shapes: loopback passes through,
    /// an unspecified v4/v6 host becomes 127.0.0.1, other IPv6 keeps brackets.
    #[test]
    fn derive_advertise_url_shapes() {
        assert_eq!(
            derive_advertise_url("127.0.0.1:7301", 7301).unwrap(),
            "http://127.0.0.1:7301"
        );
        assert_eq!(
            derive_advertise_url("0.0.0.0:9999", 9999).unwrap(),
            "http://127.0.0.1:9999"
        );
        assert_eq!(
            derive_advertise_url("[::1]:5555", 5555).unwrap(),
            "http://[::1]:5555"
        );
        assert_eq!(
            derive_advertise_url("[::]:5555", 5555).unwrap(),
            "http://127.0.0.1:5555"
        );
    }
    /// No stored credentials and a code: first-run enrollment.
    #[test]
    fn fresh_code_enrolls_when_no_credentials_stored() {
        let entry = resolve_enroll_entry(
            None,
            "https://archeion.example.com",
            Some("sk-enroll-test"),
            std::path::Path::new("/tmp/credentials"),
        )
        .expect("fresh entry resolves");
        assert_eq!(
            entry,
            EnrollEntry::Fresh {
                code: "sk-enroll-test".to_string()
            }
        );
    }

    /// Stored credentials + code at the same archeion — the stale
    /// false-alarm shape — ignores the code instead of failing the boot.
    /// The comparison is normalized origin, so trailing-slash and
    /// explicit-default-port spellings of the same server still match.
    #[test]
    fn same_archeion_code_is_ignored_with_warning_entry() {
        for (recorded, configured) in [
            (
                "https://archeion.example.com",
                "https://archeion.example.com",
            ),
            (
                "https://archeion.example.com/",
                "https://archeion.example.com",
            ),
            (
                "https://archeion.example.com:443",
                "https://archeion.example.com",
            ),
        ] {
            let entry = resolve_enroll_entry(
                Some(&stored("tagma-test", Some(recorded))),
                configured,
                Some("sk-spent"),
                std::path::Path::new("/tmp/credentials"),
            )
            .expect("same-archeion entry resolves");
            assert_eq!(
                entry,
                EnrollEntry::StoredIgnoringCode {
                    address_recorded: true
                },
                "recorded {recorded}, configured {configured}"
            );
        }
    }

    /// An unrecorded origin (credential predates origin recording) or an
    /// unparsable one on either side counts as unknown and takes the
    /// same-archeion path: origin data is hygiene and must not brick a boot.
    #[test]
    fn unknown_origin_takes_the_same_archeion_path() {
        for (recorded, configured) in [
            (None, "https://archeion.example.com"),
            (Some("not a url"), "https://archeion.example.com"),
            (Some("https://archeion.example.com"), "not a url"),
        ] {
            let entry = resolve_enroll_entry(
                Some(&stored("tagma-test", recorded)),
                configured,
                Some("sk-spent"),
                std::path::Path::new("/tmp/credentials"),
            )
            .expect("unknown-origin entry resolves");
            assert_eq!(
                entry,
                EnrollEntry::StoredIgnoringCode {
                    address_recorded: false
                },
                "recorded {recorded:?}, configured {configured}"
            );
        }
    }

    /// Stored credentials + code at a different archeion: the true conflict
    /// fails fast, both exits named and both addresses shown.
    #[test]
    fn different_archeion_code_conflict_fails_fast() {
        let err = resolve_enroll_entry(
            Some(&stored("tagma-test", Some("https://old.example.com"))),
            "https://new.example.com",
            Some("sk-enroll-test"),
            std::path::Path::new("/tmp/credentials"),
        )
        .expect_err("conflict must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("KALLIP_TAGMA_RELAY_ENROLLMENT_CODE"), "{msg}");
        assert!(msg.contains("https://old.example.com"), "{msg}");
        assert!(msg.contains("https://new.example.com"), "{msg}");
        assert!(msg.contains("/tmp/credentials"), "{msg}");
        assert!(msg.contains("re-enroll"), "{msg}");
        assert!(msg.contains("unset"), "{msg}");
        // Mutual exclusion: distinguishable from the incomplete message.
        assert!(!msg.contains("local-only"), "{msg}");
        assert!(!msg.contains("first-run"), "{msg}");
    }

    /// Neither credentials nor code: the message names the URL env var and
    /// both exits, distinct from the conflict message.
    #[test]
    fn neither_credentials_nor_code_fails_fast() {
        let err = resolve_enroll_entry(
            None,
            "https://archeion.example.com",
            None,
            std::path::Path::new("/tmp/credentials"),
        )
        .expect_err("incomplete must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("KALLIP_TAGMA_RELAY_ARCHEION_URL"), "{msg}");
        assert!(msg.contains("first-run enrollment"), "{msg}");
        assert!(msg.contains("local-only"), "{msg}");
        // Mirror of the conflict test: no conflict markers.
        assert!(!msg.contains("ignored"), "{msg}");
        assert!(!msg.contains("re-enroll"), "{msg}");
    }

    /// The ignore warning names the env var, the stored identity, and the
    /// delete-to-re-enroll recovery; the unknown-origin spelling says so.
    #[test]
    fn ignored_code_warning_names_the_recovery() {
        let recorded = ignored_code_warning(std::path::Path::new("/tmp/credentials"), true);
        assert!(
            recorded.contains("KALLIP_TAGMA_RELAY_ENROLLMENT_CODE"),
            "{recorded}"
        );
        assert!(recorded.contains("ignored"), "{recorded}");
        assert!(recorded.contains("stored identity"), "{recorded}");
        assert!(
            recorded.contains("delete the credentials directory"),
            "{recorded}"
        );
        assert!(!recorded.contains("before origin recording"), "{recorded}");

        let unknown = ignored_code_warning(std::path::Path::new("/tmp/credentials"), false);
        assert!(unknown.contains("before origin recording"), "{unknown}");
        assert!(unknown.contains("backfilled"), "{unknown}");
    }

    /// save/load roundtrip carries the enrollment origin; a pre-origin
    /// credential (id + token only) loads with the origin absent; the
    /// backfill records the configured origin exactly once and never
    /// overwrites a recorded origin.
    #[test]
    fn credential_origin_roundtrip_and_backfill() {
        let dir = tempfile::tempdir().expect("credentials tempdir");
        credentials::save_tagma(
            dir.path(),
            "tagma-1",
            "token",
            "https://archeion.example.com",
        );
        let reloaded = credentials::load_tagma(dir.path()).expect("roundtrip loads");
        assert_eq!(reloaded.id, "tagma-1");
        assert_eq!(reloaded.token, "token");
        assert_eq!(
            reloaded.archeion_url.as_deref(),
            Some("https://archeion.example.com")
        );

        let legacy = tempfile::tempdir().expect("legacy tempdir");
        std::fs::write(legacy.path().join("tagma.id"), "tagma-2").expect("write id");
        std::fs::write(legacy.path().join("tagma.token"), "token").expect("write token");
        let legacy_stored = credentials::load_tagma(legacy.path()).expect("legacy loads");
        assert_eq!(legacy_stored.archeion_url, None);

        credentials::backfill_archeion_url(legacy.path(), "https://archeion.example.com");
        assert_eq!(
            std::fs::read_to_string(legacy.path().join("archeion.url")).expect("backfilled"),
            "https://archeion.example.com"
        );
        credentials::backfill_archeion_url(legacy.path(), "https://other.example.com");
        assert_eq!(
            std::fs::read_to_string(legacy.path().join("archeion.url")).expect("unchanged"),
            "https://archeion.example.com"
        );
    }

    /// write_instance_state contract: handed a state dir, it writes
    /// exactly one owner-only `runtime.json` carrying the process pid
    /// and the actually bound (`:0`) port.
    #[tokio::test]
    async fn instance_state_dir_gets_runtime_json_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral");
        let port = listener.local_addr().expect("local addr").port();

        write_instance_state(dir.path(), &listener).expect("write instance state");

        let text = std::fs::read_to_string(dir.path().join("runtime.json")).expect("runtime.json");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse runtime.json");
        assert_eq!(parsed["pid"], serde_json::json!(std::process::id()));
        assert_eq!(parsed["port"], serde_json::json!(port));

        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.path().join("runtime.json"))
            .expect("runtime.json metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "runtime.json must be owner-only");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read state dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(entries.len(), 1, "exactly runtime.json, no tmp leftovers");
    }
}
