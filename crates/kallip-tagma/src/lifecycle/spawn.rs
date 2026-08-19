//! Agent spawning: SpawnArgs, the spawn/abort helpers, and the Materialize
//! pipeline that carries a resolved config to a registered running task.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use just_llm_client::types::chat::ChatMessage;
use kallip_common::agentid::AgentId;
use kallip_common::authtoken::{MintedToken, TokenHash};
use kallip_common::policy::{ExecPolicy, PolicyPreset};
use kallip_common::protocol::{ApiError, SseEvent, TransientRetryInfo};
use kallip_runtime::agent_task::{self, AgentContext};
use kallip_runtime::approval::ApprovalStore;
use kallip_runtime::config::AgentConfig;
use kallip_runtime::context::{AgenticContext, ContextStore, ContextSummarizer};
use kallip_runtime::history::HistoryWriter;
use kallip_runtime::persistence;
use kallip_runtime::policy::{AgentPolicy, AuthorizedToolExecutor};
use kallip_runtime::tools::{ToolDispatchInputs, build_tool_dispatch, load_skill};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::identity::{compose_system_prompt, inject_identity_env};
use super::workspace::{establish_lock_api_error, establish_workspace_lock, exec_gate_failure};
use crate::bridge::bridge_task;
use crate::state::{Agent, AgentEntry, AgentIdentity, AgentState, RegistryEntry, SharedState};

pub(crate) struct SpawnArgs {
    pub agent_id: AgentId,
    /// The tagma root agent for this spawn. Computed by the caller
    /// (Materialize::run / restore_one / reactivation), never re-derived inside
    /// spawn_agent: create/restore derive it as `supervisor_chain_ids(...).last()`
    /// (or `self.agent_id` for a root spawn), while reactivation resolves it
    /// authoritatively via `resolve_root_agent(registry.root_agent())`.
    /// Surfaced to the agent via `KALLIP_ROOT_AGENT_ID` and baked into the
    /// identity section of the system prompt.
    pub root_agent_id: AgentId,
    pub store: Arc<tokio::sync::Mutex<ContextStore>>,
    pub approvals: Arc<tokio::sync::Mutex<ApprovalStore>>,
    pub agent_dir: PathBuf,
    pub config: AgentConfig,
    pub initial_prompt: Option<String>,
    pub shutdown_cancel: CancellationToken,
    pub events_tx: broadcast::Sender<SseEvent>,
    pub auth_token_hash: TokenHash,
    pub env: HashMap<String, String>,
    pub shared_state: SharedState,
    pub preset: PolicyPreset,
    pub exec_policy: Arc<std::sync::RwLock<ExecPolicy>>,
    pub prompt_queue_size: usize,
    /// The resolved model tier (selected by the caller). The active profile is
    /// `tier.profiles[0]`; the rest form the within-tier failover chain. Owned so the
    /// runtime can carry the chain without re-touching the registry.
    pub tier: kallip_runtime::profile::Tier,
    /// Pre-created prompt channel for reactivation. When provided,
    /// `prompt_queue_size` is ignored and both ends are used as-is.
    /// The sender is already installed in the registry entry; spawn_agent
    /// only stores it in the Agent struct and passes the receiver to the
    /// agent task.
    pub prompt_channel: Option<(
        tokio::sync::mpsc::Sender<String>,
        tokio::sync::mpsc::Receiver<String>,
    )>,
}

impl SpawnArgs {
    /// Build the standard env map for an agent.
    pub fn default_env(
        agent_id: &AgentId,
        auth_token: &str,
        supervisor_agent_id: Option<&AgentId>,
        root_agent_id: &AgentId,
    ) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("KALLIP_ID".into(), agent_id.to_string());
        env.insert("KALLIP_AUTH_TOKEN".into(), auth_token.to_owned());
        inject_identity_env(&mut env, supervisor_agent_id, root_agent_id);
        env
    }
}

/// Reconstruct runtime resources shared by create and restore.
///
/// Returns the running [`Agent`] handle plus the durable [`AgentIdentity`]
/// (config + on-disk dir) for the caller to wrap into an [`AgentEntry`]. The
/// identity travels out of `spawn_agent` because the runtime config and agent
/// dir are *moved* into the spawn pipeline (used to build the tool dispatch and
/// `AgentContext`); the caller still needs them to construct the registry entry.
pub(crate) async fn spawn_agent(mut args: SpawnArgs) -> anyhow::Result<(Agent, AgentIdentity)> {
    let cancel = args.shutdown_cancel.child_token();
    let notify = Arc::new(Notify::new());
    // Separate wake signal for the timed transient-retry path (kept distinct from
    // `notify` so the approval arm's guard stays the sole authority for approval wakes).
    let retry_notify = Arc::new(Notify::new());
    // Round-scoped interrupt slot: `Some` only while a round runs. Shared with the agent
    // task so `interrupt_agent` can cancel the current round without terminating the task.
    let round_cancel: Arc<std::sync::Mutex<Option<kallip_runtime::agent_task::RoundToken>>> =
        Arc::new(std::sync::Mutex::new(None));

    let system_prompt = compose_system_prompt(
        &args.config,
        args.agent_id.clone(),
        args.root_agent_id.clone(),
    );
    let client = {
        // Install the active profile's declared context window (authoritative on both paths — the
        // implicit env profile derives it from KALLIP_CONTEXT_WINDOW_TOKENS), then build the
        // client. The tier's remaining profiles are the within-tier failover chain, walked by the
        // runner on `RequestFailure::Failover`.
        let profile = args.tier.active_profile();
        args.config.set_context_window(profile.max_context_window)?;
        args.shared_state
            .profiles
            .load()
            .registry
            .build_client(profile, Some(system_prompt.clone()))?
    };

    // Mint the prompt channel before building the tool dispatch so a background
    // task can push a completion notice onto it (the dispatch wires `notify` into
    // the shell backend's terminal-state observer). `try_send` drops silently
    // on a full/dead channel — the agent then falls back to polling
    // `bash_background_read`, so a dropped notice is never a correctness loss.
    let (prompt_tx, prompt_rx) = args
        .prompt_channel
        .unwrap_or_else(|| tokio::sync::mpsc::channel(args.prompt_queue_size));
    let notice_sink: Arc<dyn Fn(String) + Send + Sync> = {
        let prompt_tx = prompt_tx.clone();
        Arc::new(move |text| {
            let _ = prompt_tx.try_send(text);
        })
    };

    // Live activity cell: written by `PUT /agents/{id}/activity` (self-report),
    // cleared by the bridge on terminal events, read by list/status. Rides on
    // the returned `Agent`.
    let activity = Arc::new(std::sync::Mutex::new(String::new()));

    // Per-agent execution gate: READ across this agent's shell forks (threaded
    // into the dispatch), WRITE across a workspace carve-out when a subagent is
    // spawned under this agent (reached via the returned `Agent.exec_gate`).
    let exec_gate = kallip_runtime::ExecGate::new();

    let dispatch = build_tool_dispatch(ToolDispatchInputs {
        ctx: args.store.clone(),
        config: &args.config,
        env: args.env.clone(),
        notice_sink: notice_sink.clone(),
        exec_policy: args.exec_policy.clone(),
        lock_manager: args.shared_state.lock_manager.clone(),
        agent_id: args.agent_id.clone(),
        exec_gate: exec_gate.clone(),
    })
    .await?;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);

    // Hook rules ride AppState (parsed once at tagma startup); unset means
    // no rules and zero hook behavior. The executor keeps its own clone of
    // the notice sink so post-call hook notes reach the prompt channel
    // without any runner involvement.
    let hook_rules = args
        .shared_state
        .hook_rules
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(Vec::new()));
    let executor = AuthorizedToolExecutor::new(
        dispatch,
        AgentPolicy::new(args.exec_policy.clone(), args.preset, hook_rules),
        args.approvals.clone(),
        Some(notice_sink),
    );
    let tool_defs = executor.tool_definitions();
    args.store.lock().await.set_tool_definitions(tool_defs);
    args.store
        .lock()
        .await
        .set_pinned_budget(args.config.pinned_budget());
    let summarizer = ContextSummarizer::new(args.config.summary_max_tokens);

    let token_budget = args.shared_state.token_budget.clone();

    let pending_profile_reset = Arc::new(std::sync::Mutex::new(None));

    let bundle = args.shared_state.profiles.load();
    // Create the inbox message puller for this agent. None if the inbox store
    // is not installed (edge case: should not happen in production).
    let message_puller: Option<Arc<dyn kallip_runtime::agent_task::MessagePuller>> =
        args.shared_state.inboxes.get().map(|store| {
            Arc::new(crate::inbox::InboxPuller::new(
                store.clone(),
                args.agent_id.clone(),
            )) as Arc<dyn kallip_runtime::agent_task::MessagePuller>
        });

    let ctx = AgentContext {
        client,
        failover: kallip_runtime::FailoverState::new(
            args.tier,
            bundle.registry.clone(),
            Some(system_prompt),
        ),
        store: args.store.clone(),
        approvals: args.approvals.clone(),
        executor,
        summarizer,
        config: args.config.clone(),
        agent_dir: Some(args.agent_dir.clone()),
        history: Some(HistoryWriter::new(args.agent_dir.clone())),
        cancel: cancel.clone(),
        round_cancel: round_cancel.clone(),
        notify: notify.clone(),
        retry_notify: retry_notify.clone(),
        retry_at: Arc::new(std::sync::Mutex::new(None)),
        transient_fails: 0,
        lifecycle: std::sync::Mutex::new(kallip_runtime::LifecycleState::Idle),
        wait_until: Arc::new(std::sync::Mutex::new(None)),
        wait_notify: Arc::new(tokio::sync::Notify::new()),
        wait_armed_secs: 0,
        token_budget: token_budget.clone(),
        pending_profile_reset: pending_profile_reset.clone(),
        message_puller,
    };

    let agent_handle = tokio::spawn(agent_task::agent_task(
        ctx,
        args.initial_prompt,
        prompt_rx,
        agent_tx,
    ));
    let state = Arc::new(AtomicU8::new(AgentState::IDLE));
    let parked: Arc<std::sync::Mutex<Option<crate::state::ParkedSnapshot>>> =
        Arc::new(std::sync::Mutex::new(None));
    let retrying: Arc<std::sync::Mutex<Option<TransientRetryInfo>>> =
        Arc::new(std::sync::Mutex::new(None));
    let agent_id = args.agent_id;
    let bridge_handle = tokio::spawn(bridge_task(
        agent_id.clone(),
        agent_rx,
        args.events_tx.clone(),
        args.shutdown_cancel.clone(),
        state.clone(),
        activity.clone(),
        parked.clone(),
        retrying.clone(),
        args.shared_state.clone(),
    ));

    Ok((
        Agent {
            prompt_tx,
            events_tx: args.events_tx,
            approvals: args.approvals,
            agent_handle,
            bridge_handle,
            store: args.store,
            cancel,
            round_cancel,
            notify,
            state,
            activity,
            auth_token_hash: args.auth_token_hash,
            env: args.env,
            preset: args.preset,
            exec_policy: args.exec_policy,
            exec_gate,
            pending_profile_reset,
            parked,
            retrying,
        },
        AgentIdentity {
            config: args.config,
            agent_dir: Some(args.agent_dir),
        },
    ))
}

/// Best-effort removal of an agent's on-disk directory on a create/rollback
/// failure. Logs a warning on error and never returns `Err` — rollback must
/// proceed regardless (a leftover dir beats aborting the error path).
fn remove_agent_dir(dir: &std::path::Path) {
    if let Err(e) = std::fs::remove_dir_all(dir) {
        tracing::warn!(path = %dir.display(), "failed to clean up agent dir: {e:#}");
    }
}

/// Roll back a create that failed before the agent was registered: drop the
/// pre-reserved subagent slot (if this is a subagent) and remove the agent dir.
///
/// Used by the three pre-registration failure paths in `create_agent` (acquire
/// `Busy`/`Err`, and `spawn_agent` failure). The workspace write-lock is NOT
/// touched here — the acquire-failure paths never acquired it, and the
/// spawn-failure path leaves it to `WorkspaceLockGuard`'s `Drop`.
async fn rollback_unspawned_create(
    state: &SharedState,
    created_by: Option<&AgentId>,
    id: &AgentId,
    agent_dir: &std::path::Path,
) {
    if let Some(supervisor_id) = created_by {
        let mut registry = state.registry.write().await;
        if let Some(supervisor) = registry.get_mut(supervisor_id) {
            supervisor.subagent_ids_mut().retain(|sid| sid != id);
        }
    }
    remove_agent_dir(agent_dir);
}

/// Abort agent/bridge handles and remove agent dir (best-effort).
/// Used when a spawned agent cannot be registered. The on-disk dir is no longer
/// carried by `Agent` (it lives on `AgentIdentity`), so the caller passes it.
pub(crate) fn abort_agent(agent: &crate::state::Agent, dir: Option<&std::path::Path>) {
    agent.agent_handle.abort();
    agent.bridge_handle.abort();
    if let Some(dir) = dir {
        remove_agent_dir(dir);
    }
}

/// Materialize one agent end-to-end from a fully-resolved config: workspace
/// disjoint check, agent dir + skills, workspace write-lock, task spawn, agent
/// cap, and registration. Owns the [`WorkspaceLockGuard`](super::workspace::WorkspaceLockGuard) and disarms it on
/// success so a Normal agent keeps its workspace lock for life.
///
/// This is the shared tail of every agent-creation path. The two callers build
/// the head themselves and hand off:
/// - [`create_agent`](super::create_agent) resolves a *subagent* (supervisor validation, permission
///   ceiling, exec-policy inheritance, pre-reserved slot) and passes
///   `rollback_supervisor: Some(…)`.
/// - [`ensure_root_agent`](super::ensure_root_agent) resolves the tagma singleton *root* (env-driven
///   config, default exec-policy) and passes `rollback_supervisor: None`.
///
/// `rollback_supervisor` doubles as the creation shape: `None` means this is the
/// root, registered via [`crate::state::AgentRegistry::register_root`]; `Some`
/// means a subagent, registered via `register_no_subagent_push` (slot
/// pre-reserved by the caller) and retracted from that supervisor on failure.
pub(crate) struct Materialize<'a> {
    pub(crate) state: &'a SharedState,
    pub(crate) id: AgentId,
    pub(crate) token: MintedToken,
    pub(crate) config: AgentConfig,
    pub(crate) exec_policy: ExecPolicy,
    pub(crate) rollback_supervisor: Option<AgentId>,
}

impl<'a> Materialize<'a> {
    pub(crate) async fn run(self) -> Result<AgentId, ApiError> {
        let state = self.state;
        let id = self.id;
        let token = self.token;
        let rollback_supervisor = self.rollback_supervisor;
        let is_root = self.config.is_root();
        let mut config = self.config;
        let exec_policy = Arc::new(std::sync::RwLock::new(self.exec_policy));

        // Resolve the model tier purely by depth (positional tiers — no
        // name/override). Carry the tier into SpawnArgs for failover.
        let depth = config.permissions.depth();
        let tier = state.profiles.load().registry.select_profile(depth).clone();

        let store = Arc::new(tokio::sync::Mutex::new(ContextStore::new()));
        let approvals = Arc::new(tokio::sync::Mutex::new(ApprovalStore::new()));

        // Create the agent directory before persisting the exec policy and the
        // agent's metadata files.
        let agent_dir = persistence::create_agent_dir(
            &id,
            &config.workspace_root,
            config.created_by.as_ref(),
            &config.role,
            &config.description,
            config.permissions_class,
            config.delegation_mode,
        )
        .map_err(ApiError::internal)?;

        for skill_name in &config.skills {
            let content = match load_skill(skill_name) {
                Ok(c) => c,
                Err(e) => {
                    // The agent dir already exists; roll it back so `scan_agents`
                    // does not pick up an orphan `meta.json` on restart.
                    rollback_unspawned_create(state, rollback_supervisor.as_ref(), &id, &agent_dir)
                        .await;
                    return Err(ApiError::bad_request(e.to_string()));
                }
            };
            if let Err(e) = store.lock().await.pin(
                &format!("skill:{skill_name}"),
                ChatMessage::user(format!("[skill: {skill_name}]\n{content}")),
            ) {
                rollback_unspawned_create(state, rollback_supervisor.as_ref(), &id, &agent_dir)
                    .await;
                return Err(ApiError::internal(e));
            }
            info!(skill = skill_name, "loaded skill");
        }

        // Bind the result so the `exec_policy.read()` guard (a `!Send`
        // `std::sync::RwLockReadGuard`) drops at the `;`, before the `.await`
        // in the error arm below.
        let persist_result = persistence::persist_exec_policy(
            &agent_dir,
            &exec_policy.read().unwrap_or_else(|e| e.into_inner()),
        );
        if let Err(e) = persist_result {
            rollback_unspawned_create(state, rollback_supervisor.as_ref(), &id, &agent_dir).await;
            return Err(ApiError::internal(e));
        }

        let prompt = config.prompt.take();
        let log_ws = config.workspace_root.display().to_string();
        let log_depth = config.permissions.max_depth;
        let log_role = config.role.clone();
        // Compute the delegation ancestor chain under a registry read lock, then
        // drop the guard before acquiring. `created_by` is immutable post-creation,
        // so the owned id snapshot is stable; dropping the guard keeps the critical
        // section minimal and lets a nested lock held under an ancestor be delegated
        // (see `DirLockManager::acquire`). A supervisor removed in the tiny window
        // between snapshot and acquire has its lock released by `release_all`, so a
        // stale id never matches a live holder; a dangling `created_by` is caught
        // downstream by the supervisor-still-registered re-check.
        let chain: Vec<AgentId> = match config.created_by.as_ref() {
            Some(supervisor_id) => {
                let registry = state.registry.read().await;
                match registry.supervisor_chain_ids(supervisor_id) {
                    Ok(c) => c,
                    Err(e) => {
                        rollback_unspawned_create(
                            state,
                            rollback_supervisor.as_ref(),
                            &id,
                            &agent_dir,
                        )
                        .await;
                        return Err(e);
                    }
                }
            }
            None => Vec::new(),
        };
        // The root is the chain's terminal ancestor (or self for a root spawn,
        // where the chain is empty). Reused for env injection and the identity
        // section; not re-derived inside spawn_agent.
        let root_agent_id = chain.last().cloned().unwrap_or_else(|| id.clone());
        // Carve-out gate: a subagent's workspace acquire narrows the supervisor's
        // writable set, so refuse if the supervisor has an in-flight shell (a
        // foreground exec holding the gate READ, or a running background task)
        // that could snapshot the pre-carve set and keep writing the carved-out
        // region. The WRITE guard is held across the workspace acquire (the
        // actual carve) and released right after: once the lock state is updated,
        // any later fork snapshots the post-carve set correctly, so holding the
        // guard through task spawn + registration would only freeze the
        // supervisor's shells for no benefit. Root (no supervisor) skips the gate.
        // Clone the supervisor's gate under a brief registry read-lock; the WRITE
        // guard below borrows this owned Arc, so it lives in this scope (outside
        // the read-lock critical section).
        let supervisor_gate: Option<Arc<kallip_runtime::ExecGate>> =
            match config.created_by.as_ref() {
                Some(supervisor_id) => {
                    let registry = state.registry.read().await;
                    registry
                        .get(supervisor_id)
                        .and_then(RegistryEntry::as_live)
                        .map(|s| s.agent.exec_gate.clone())
                }
                None => None,
            };
        let _supervisor_exec_guard = match &supervisor_gate {
            // Supervisor not live (faulted) -> None: no in-flight shell to race.
            Some(g) => Some(match g.try_write() {
                Ok(guard) => guard,
                Err(f) => {
                    rollback_unspawned_create(state, rollback_supervisor.as_ref(), &id, &agent_dir)
                        .await;
                    return Err(exec_gate_failure(f));
                }
            }),
            None => None,
        };
        // Establish the workspace carve (forward handoff transfer + acquire +
        // reverse-transfer rollback guards) via the single shared helper, so
        // spawn and restore cannot drift. The supervisor exec-gate WRITE is held
        // across this call, covering the whole carve. On error the helper has
        // already restored the dirlock; we roll back the agent dir here.
        let mut established = match establish_workspace_lock(state, &id, &config, &chain) {
            Ok(e) => e,
            Err(e) => {
                rollback_unspawned_create(state, rollback_supervisor.as_ref(), &id, &agent_dir)
                    .await;
                return Err(establish_lock_api_error(e));
            }
        };
        // Carve complete (the workspace lock state now reflects the child); the
        // supervisor's shells may fork freely again. Dropping here (not at end of
        // run) keeps the supervisor from being frozen across task spawn/register.
        drop(_supervisor_exec_guard);
        let (events_tx, _) = broadcast::channel(256);
        let agent_dir_clone = agent_dir.clone();
        let env = SpawnArgs::default_env(
            &id,
            token.secret(),
            config.created_by.as_ref(),
            &root_agent_id,
        );
        let (agent, identity) = match spawn_agent(SpawnArgs {
            agent_id: id.clone(),
            root_agent_id: root_agent_id.clone(),
            store,
            approvals,
            agent_dir,
            config,
            initial_prompt: prompt,
            shutdown_cancel: state.shutdown.clone(),
            events_tx,
            auth_token_hash: token.hash().clone(),
            env,
            shared_state: state.clone(),
            preset: state.preset,
            exec_policy: exec_policy.clone(),
            prompt_queue_size: state.prompt_queue_size,
            prompt_channel: None,
            tier,
        })
        .await
        {
            Ok(pair) => pair,
            Err(e) => {
                // Roll back the subagent slot + agent dir here; the workspace
                // carve (lock + any handoff transfer) is reversed by `established`'s
                // Drop on the `return Err` below.
                rollback_unspawned_create(
                    state,
                    rollback_supervisor.as_ref(),
                    &id,
                    &agent_dir_clone,
                )
                .await;
                return Err(ApiError::internal(e));
            }
        };
        {
            let mut registry = state.registry.write().await;
            // Global agent count cap.
            if registry.len() >= state.max_agents {
                // Rollback: remove the pre-reserved subagent slot.
                if let Some(ref supervisor_id) = rollback_supervisor
                    && let Some(supervisor) = registry.get_mut(supervisor_id)
                {
                    supervisor.subagent_ids_mut().retain(|sid| sid != &id);
                }
                abort_agent(&agent, identity.agent_dir.as_deref());
                // `established` drops on `return Err`, reversing the carve.
                return Err(ApiError::unavailable(format!(
                    "agent limit reached ({}/{max}), remove agents to create new ones",
                    registry.len(),
                    max = state.max_agents
                )));
            }
            // Re-verify supervisor was not removed during agent spawn.
            if let Some(ref supervisor_id) = rollback_supervisor
                && !registry.contains_key(supervisor_id)
            {
                // Supervisor gone — the pre-reserved slot is already cleaned up
                // (unregistering the supervisor removes it from the map entirely).
                abort_agent(&agent, identity.agent_dir.as_deref());
                // `established` drops on `return Err`, reversing the carve.
                return Err(ApiError::internal(
                    "supervisor agent was removed during creation",
                ));
            }
            let entry = RegistryEntry::Live(AgentEntry {
                identity,
                agent,
                subagent_ids: vec![],
            });
            if is_root {
                // The singleton root — register_root enforces the at-most-one
                // invariant; the eager-create caller already checked for absence.
                registry.register_root(id.clone(), entry)?;
            } else {
                // Subagent: slot was pre-reserved by the caller, so skip the push.
                registry.register_no_subagent_push(id.clone(), entry);
            }
        }
        // Registered: disarm both guards so their Drops neither release the
        // workspace lock nor reverse a handoff transfer. Drop explicitly so the
        // borrow of `id` (held by `WorkspaceLockGuard`) ends before `id` moves
        // into the return value.
        established.disarm();
        drop(established);
        info!(id = %id, root = is_root, role = %log_role, ws = %log_ws, depth = log_depth, "created agent");
        Ok(id)
    }
}
