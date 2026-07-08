use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_llm_client::types::chat::ChatMessage;
use kallip_common::agentid::AgentId;
use kallip_common::policy::{ExecPolicy, ToolPolicy};
use kallip_common::protocol::ApiError;
use kallip_common::protocol::SseEvent;
use kallip_runtime::agent_task::{self, AgentContext};
use kallip_runtime::approval::ApprovalStore;
use kallip_runtime::config::{
    AgentConfig, PermissionClass, PermissionProfile, permission_class_from_env,
    tool_policy_from_env,
};
use kallip_runtime::context::{AgenticContext, ContextStore, ContextSummarizer};
use kallip_runtime::history::HistoryWriter;
use kallip_runtime::persistence;
use kallip_runtime::policy::{AgentPolicy, AuthorizedToolExecutor};
use kallip_runtime::tools::{
    ToolDispatchInputs, build_tool_dispatch, load_skill, meta_skill_content,
};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use kallip_common::protocol::{
    CreateAgentRequest, CreateAgentResponse, ListAgentsQuery, UpdateActivityRequest,
    UpdateAgentMetadataRequest,
};

use super::ListAgentsResponse;
use crate::bridge::bridge_task;
use crate::state::{Agent, AgentEntry, AgentState, AgentSummary, SharedState};
use crate::token::{MintedToken, TokenHash, TokenKind};

pub(crate) struct SpawnArgs {
    pub agent_id: AgentId,
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
    pub tool_policy: Arc<std::sync::RwLock<ToolPolicy>>,
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
    pub fn default_env(agent_id: &AgentId, auth_token: &str) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("KALLIP_ID".into(), agent_id.to_string());
        env.insert("KALLIP_AUTH_TOKEN".into(), auth_token.to_owned());
        env
    }
}

/// Reconstruct runtime resources shared by create and restore.
pub(crate) async fn spawn_agent(mut args: SpawnArgs) -> anyhow::Result<Agent> {
    let cancel = args.shutdown_cancel.child_token();
    let notify = Arc::new(Notify::new());
    // Round-scoped interrupt slot: `Some` only while a round runs. Shared with the agent
    // task so `interrupt_agent` can cancel the current round without terminating the task.
    let round_cancel: Arc<std::sync::Mutex<Option<kallip_runtime::agent_task::RoundToken>>> =
        Arc::new(std::sync::Mutex::new(None));

    let system_prompt = {
        let meta = meta_skill_content();
        let mut sp = args.config.system_prompt.clone();
        sp.push_str("\n\n");
        sp.push_str(meta);
        sp
    };
    let client = {
        // Install the active profile's declared context window (authoritative on both paths — the
        // implicit env profile derives it from KALLIP_CONTEXT_WINDOW_TOKENS), then build the
        // client. The tier's remaining profiles are the within-tier failover chain, walked by the
        // runner on `RequestFailure::Failover`.
        let profile = args.tier.active_profile();
        args.config.set_context_window(profile.max_context_window)?;
        args.shared_state
            .profiles
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

    // Ensure the agent's local-skills dir exists before any landlock apply. The
    // sandbox baseline grants write on `agent_dir/skills` (so the agent can author
    // local skills); landlock `PathBeneath` silently skips non-existent paths, so
    // a missing dir would drop the grant and make skill authoring fail with EACCES.
    // Idempotent; the dir already exists for restored agents.
    std::fs::create_dir_all(args.agent_dir.join("skills")).map_err(ApiError::internal)?;

    let dispatch = build_tool_dispatch(ToolDispatchInputs {
        ctx: args.store.clone(),
        config: &args.config,
        env: args.env.clone(),
        notice_sink,
        exec_policy: args.exec_policy.clone(),
        lock_manager: args.shared_state.lock_manager.clone(),
        agent_id: args.agent_id.clone(),
        agent_dir: args.agent_dir.clone(),
    })
    .await?;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);

    let executor = AuthorizedToolExecutor::new(
        dispatch,
        AgentPolicy::new(args.tool_policy.clone(), args.exec_policy.clone()),
        args.approvals.clone(),
    );
    let tool_defs = executor.tool_definitions();
    args.store.lock().await.set_tool_definitions(tool_defs);
    args.store
        .lock()
        .await
        .set_pinned_budget(args.config.pinned_budget());
    let summarizer = ContextSummarizer::new(args.config.summary_max_tokens);

    let token_budget = args.shared_state.token_budget.clone();

    let ctx = AgentContext {
        client,
        failover: kallip_runtime::FailoverState::new(
            args.tier,
            args.shared_state.profiles.clone(),
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
        token_budget: token_budget.clone(),
    };

    let agent_handle = tokio::spawn(agent_task::agent_task(
        ctx,
        args.initial_prompt,
        prompt_rx,
        agent_tx,
    ));
    let state = Arc::new(AtomicU8::new(AgentState::IDLE));
    let agent_id = args.agent_id;
    let bridge_handle = tokio::spawn(bridge_task(
        agent_id.clone(),
        agent_rx,
        args.events_tx.clone(),
        args.shutdown_cancel.clone(),
        state.clone(),
        activity.clone(),
        args.shared_state.clone(),
    ));

    Ok(Agent {
        prompt_tx,
        events_tx: args.events_tx,
        approvals: args.approvals,
        config: args.config,
        agent_handle,
        bridge_handle,
        store: args.store,
        agent_dir: Some(args.agent_dir),
        cancel,
        round_cancel,
        notify,
        state,
        activity,
        auth_token_hash: args.auth_token_hash,
        env: args.env,
        tool_policy: args.tool_policy,
        exec_policy: args.exec_policy,
    })
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
            supervisor.subagent_ids.retain(|sid| sid != id);
        }
    }
    remove_agent_dir(agent_dir);
}

/// Abort agent/bridge handles and remove agent dir (best-effort).
/// Used when a spawned agent cannot be registered.
pub(crate) fn abort_agent(agent: &crate::state::Agent) {
    agent.agent_handle.abort();
    agent.bridge_handle.abort();
    if let Some(ref dir) = agent.agent_dir {
        remove_agent_dir(dir);
    }
}

/// RAII guard for the workspace write-lock acquired on every materialization
/// path (create, restore, reactivation) for a Normal agent.
///
/// Releases the lock on `Drop` (every `return Err` -- and any panic -- between
/// acquire and successful registration) unless disarmed. The success path
/// disarms it so the registered agent keeps the lock for its lifetime (it is
/// released later by `remove_agent`/reactivation). This covers the panic case a
/// manual `release_all` at each error return cannot reach.
pub(crate) struct WorkspaceLockGuard<'a> {
    state: &'a SharedState,
    id: &'a AgentId,
    armed: bool,
}

impl WorkspaceLockGuard<'_> {
    /// Disarm so `Drop` no longer releases -- call exactly once, on the success
    /// path once the agent is registered and owns the lock.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorkspaceLockGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.state.lock_manager.release_all(self.id);
        }
    }
}

/// Failure to acquire the workspace write-lock for a Normal agent. Callers
/// apply path-specific policy: create rolls back the agent dir and returns 409;
/// restore bails so the caller skips the agent (and its subtree); reactivation
/// refuses to wake the agent into a state where it cannot write its own
/// workspace (returning conflict to the sender).
pub(crate) enum WorkspaceAcquireFailure {
    /// Another agent holds an overlapping write-lock.
    Busy { holder: AgentId, conflict: PathBuf },
    /// The acquire itself errored (e.g. an unresolvable workspace path).
    Other(std::io::Error),
}

/// Acquire the workspace write-lock for a Normal agent (Guests acquire nothing
/// and get `Ok(None)`). Returns an armed [`WorkspaceLockGuard`] on success --
/// its `Drop` releases the lock, so a failure on the caller's path between this
/// call and a successful spawn needs no manual `release_all`. Callers that want
/// the lock to persist past a success point call `guard.disarm()`.
///
/// `chain` is the agent's delegation ancestors (owned ids), so a nested lock
/// held under an ancestor is treated as delegation, not conflict (see
/// [`DirLockManager::acquire`]).
///
/// Pure acquire: NO rollback, NO `ApiError` mapping -- each materialization path
/// decides its own conflict policy. This is the single place the workspace lock
/// is taken, so the "a Normal agent holds a write-lock on its workspace for the
/// lifetime of its task" invariant cannot be bypassed by skipping a call site.
///
/// [`DirLockManager::acquire`]: kallip_runtime::dirlock::DirLockManager::acquire
pub(crate) fn try_acquire_workspace_lock<'a>(
    state: &'a SharedState,
    id: &'a AgentId,
    config: &AgentConfig,
    chain: &[AgentId],
) -> Result<Option<WorkspaceLockGuard<'a>>, WorkspaceAcquireFailure> {
    if config.permissions_class != PermissionClass::Normal {
        return Ok(None);
    }
    match state
        .lock_manager
        .acquire(id, &config.workspace_root, chain)
    {
        Ok(kallip_runtime::dirlock::AcquireOutcome::Acquired)
        | Ok(kallip_runtime::dirlock::AcquireOutcome::AlreadyHeld) => {
            Ok(Some(WorkspaceLockGuard {
                state,
                id,
                armed: true,
            }))
        }
        Ok(kallip_runtime::dirlock::AcquireOutcome::Busy { holder, conflict }) => {
            Err(WorkspaceAcquireFailure::Busy { holder, conflict })
        }
        Err(e) => Err(WorkspaceAcquireFailure::Other(e)),
    }
}

/// Create-path wrapper around [`try_acquire_workspace_lock`] that maps failure
/// to [`ApiError`] and rolls back the pre-created agent dir (create-specific
/// bookkeeping the other paths do not have). Extracted from `create_agent` so
/// the acquire + rollback path is unit-testable without spawning the agent task.
async fn acquire_workspace_lock<'a>(
    state: &'a SharedState,
    id: &'a AgentId,
    config: &AgentConfig,
    chain: &[AgentId],
    created_by: Option<&AgentId>,
    agent_dir: &std::path::Path,
    log_ws: &str,
) -> Result<Option<WorkspaceLockGuard<'a>>, ApiError> {
    match try_acquire_workspace_lock(state, id, config, chain) {
        Ok(guard) => Ok(guard),
        Err(WorkspaceAcquireFailure::Busy { holder, conflict }) => {
            // `agent_dir` was created before the acquire; roll it back so
            // `scan_agents` doesn't pick up an orphan `meta.json` on restart.
            rollback_unspawned_create(state, created_by, id, agent_dir).await;
            // `conflict` is the canonical path of the overlapping lock (the
            // workspace itself, or an ancestor/descendant another agent holds
            // that is not a delegation ancestor). A nested-vs-existing-workspace
            // collision 409s here.
            Err(ApiError::conflict(format!(
                "workspace_root {log_ws} overlaps a write-lock on {} held by agent \
                 {holder}; remove it or choose a non-overlapping workspace",
                conflict.display()
            )))
        }
        Err(WorkspaceAcquireFailure::Other(e)) => {
            rollback_unspawned_create(state, created_by, id, agent_dir).await;
            Err(ApiError::bad_request(e.to_string()))
        }
    }
}

pub async fn create_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Root agents require operator privilege.
    if req.created_by.is_none() {
        crate::auth::require_operator(auth.identity())?;
    }

    let id = AgentId::random();
    // Mint a fresh 256-bit `sk-agent-…` token. The plaintext goes into the agent shell env
    // (`KALLIP_AUTH_TOKEN`); only its SHA-256 is indexed for auth lookup.
    let token = MintedToken::generate(TokenKind::Agent);

    let mut config = {
        let ws = req.workspace_root.map(std::path::PathBuf::from);
        AgentConfig::load(req.prompt, req.skills, ws)
            .map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    config.agent_id = Some(id.clone());
    if let Some(rounds) = req.max_tool_rounds {
        match rounds {
            kallip_common::protocol::MaxToolRounds::Unlimited => {
                config.set_max_tool_rounds(usize::MAX);
            }
            kallip_common::protocol::MaxToolRounds::Limited(n) => {
                if n == 0 {
                    return Err(ApiError::bad_request(
                        "max_tool_rounds must be greater than zero",
                    ));
                }
                config.set_max_tool_rounds(n);
            }
        }
    }
    config.role = req.role.clone();
    config.description = req.description.clone();
    // Fleet discipline: a subagent spawn must carry a non-empty role so a
    // superior can tell its subagents apart. Root/operator spawns may leave it
    // unset (backward-compatible with clients that don't send `role`).
    if req.created_by.is_some() && config.role.trim().is_empty() {
        return Err(ApiError::bad_request(
            "subagent requires a non-empty 'role'",
        ));
    }
    // Reject any workspace that overlaps the daemon data tree (one contains the
    // other). With the overlap eliminated, landlock alone enforces the data-dir
    // integrity baseline: the agent's writable set never covers daemon data
    // except its own `agents/<id>/skills/`. Bidirectional because either direction
    // is dangerous — a workspace inside (or equal to) the data dir lets the agent
    // reach peers' bookkeeping; a workspace containing it lets the broad write
    // grant cover the whole tree. `conflict` (not `bad_request`) to match the
    // neighboring workspace↔write-lock overlap at the acquire step below.
    persistence::ensure_workspace_disjoint(&config.workspace_root)
        .map_err(|e| ApiError::conflict(e.to_string()))?;
    let env = SpawnArgs::default_env(&id, token.secret());

    // Subagent: validate supervisor and delegation constraints, or use default policy.
    // Pre-reserve the subagent slot under write lock to eliminate TOCTOU.
    let (tool_policy, exec_policy) = if let Some(ref supervisor_id) = req.created_by {
        let mut registry = state.registry.write().await;
        let (permissions, policy, exec, permission_class) = validate_subagent_request(
            &registry,
            auth.identity(),
            supervisor_id,
            &config.workspace_root,
        )?;
        // Check per-agent subagent limit and pre-reserve the slot.
        let supervisor = registry
            .get_mut(supervisor_id)
            .ok_or_else(|| ApiError::not_found("supervisor not found"))?;
        if supervisor.subagent_ids.len() >= state.max_subagents {
            return Err(ApiError::unavailable(format!(
                "supervisor has {}/{max} subagents, cannot create more",
                supervisor.subagent_ids.len(),
                max = state.max_subagents
            )));
        }
        // Pre-reserve: push the new ID so concurrent requests see the updated count.
        supervisor.subagent_ids.push(id.clone());
        config.created_by = Some(supervisor_id.clone());
        config.permissions = permissions;
        config.permissions_class = permission_class;
        (policy, exec)
    } else {
        config.permissions_class = permission_class_from_env();
        (tool_policy_from_env(), ExecPolicy::default())
    };
    let tool_policy = Arc::new(std::sync::RwLock::new(tool_policy));
    let exec_policy = Arc::new(std::sync::RwLock::new(exec_policy));

    // Resolve the model tier purely by depth (positional tiers — no name/override). Carry the
    // tier into SpawnArgs for failover.
    let depth = config.permissions.depth();
    let tier = state.profiles.select_profile(depth).clone();

    let store = Arc::new(tokio::sync::Mutex::new(ContextStore::new()));
    let approvals = Arc::new(tokio::sync::Mutex::new(ApprovalStore::new()));

    // Create agent directory before loading skills so that agent-local
    // skills can be resolved from the agent dir.
    let agent_dir = persistence::create_agent_dir(
        &id,
        &config.workspace_root,
        config.created_by.as_ref(),
        &config.role,
        &config.description,
        config.permissions_class,
    )
    .map_err(ApiError::internal)?;

    for skill_name in &config.skills {
        let content = load_skill(skill_name, Some(agent_dir.as_path()))
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        store
            .lock()
            .await
            .pin(
                &format!("skill:{skill_name}"),
                ChatMessage::user(format!("[skill: {skill_name}]\n{content}")),
            )
            .map_err(ApiError::internal)?;
        info!(skill = skill_name, "loaded skill");
    }

    persistence::persist_policy(
        &agent_dir,
        &tool_policy.read().unwrap_or_else(|e| e.into_inner()),
    )
    .map_err(ApiError::internal)?;

    persistence::persist_exec_policy(
        &agent_dir,
        &exec_policy.read().unwrap_or_else(|e| e.into_inner()),
    )
    .map_err(ApiError::internal)?;

    let prompt = config.prompt.take();
    let log_ws = config.workspace_root.display().to_string();
    let log_depth = config.permissions.max_depth;
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
            registry.supervisor_chain_ids(supervisor_id)?
        }
        None => Vec::new(),
    };
    // Auto-acquire an exclusive write-lock on the workspace so no two agents edit
    // the same workspace concurrently. **Normal only** -- a Guest is readonly (its
    // landlock writable set is the skills carve alone), so it neither needs nor
    // holds a workspace write-lock (holding one would block writers and mislabel
    // the Guest as the workspace's writer in the dirlock registry). A Normal
    // agent holds this lock for the lifetime of its task: it is re-acquired on
    // every materialization path (restore, reactivation), not just create, so
    // the workspace stays in the landlock writable set across daemon restarts.
    // Done before spawn so enforcement is active for the first command; rolled
    // back on every failure path below.
    let mut workspace_lock = acquire_workspace_lock(
        &state,
        &id,
        &config,
        &chain,
        req.created_by.as_ref(),
        &agent_dir,
        &log_ws,
    )
    .await?;
    let (events_tx, _) = broadcast::channel(256);
    let agent_dir_clone = agent_dir.clone();
    let agent = match spawn_agent(SpawnArgs {
        agent_id: id.clone(),
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
        tool_policy: tool_policy.clone(),
        exec_policy: exec_policy.clone(),
        prompt_queue_size: state.prompt_queue_size,
        prompt_channel: None,
        tier,
    })
    .await
    {
        Ok(a) => a,
        Err(e) => {
            // The workspace lock is released by `workspace_lock`'s Drop on the
            // `return Err` below; roll back the subagent slot + agent dir here.
            rollback_unspawned_create(&state, req.created_by.as_ref(), &id, &agent_dir_clone).await;
            return Err(ApiError::internal(e));
        }
    };
    {
        let mut registry = state.registry.write().await;
        // Global agent count cap.
        if registry.len() >= state.max_agents {
            // Rollback: remove the pre-reserved subagent slot.
            if let Some(ref supervisor_id) = req.created_by
                && let Some(supervisor) = registry.get_mut(supervisor_id)
            {
                supervisor.subagent_ids.retain(|sid| sid != &id);
            }
            abort_agent(&agent);
            // `workspace_lock` releases the workspace lock on `return Err`.
            return Err(ApiError::unavailable(format!(
                "agent limit reached ({}/{max}), remove agents to create new ones",
                registry.len(),
                max = state.max_agents
            )));
        }
        // Re-verify supervisor was not removed during agent spawn.
        if let Some(ref supervisor_id) = req.created_by
            && !registry.contains_key(supervisor_id)
        {
            // Supervisor gone — the pre-reserved slot is already cleaned up
            // (unregistering the supervisor removes it from the map entirely).
            abort_agent(&agent);
            // `workspace_lock` releases the workspace lock on `return Err`.
            return Err(ApiError::internal(
                "supervisor agent was removed during creation",
            ));
        }
        registry.register_no_subagent_push(
            id.clone(),
            AgentEntry {
                agent,
                subagent_ids: vec![],
            },
        );
    }
    // Registered: the Normal agent now owns the workspace lock for its lifetime —
    // disarm the guard so its `Drop` does not release it. (Guest holds no lock.)
    if let Some(lock) = workspace_lock.as_mut() {
        lock.disarm();
    }
    info!(id = %id, supervisor = ?req.created_by, role = ?req.role, ws = %log_ws, depth = log_depth, "created agent");

    Ok((
        StatusCode::CREATED,
        Json(CreateAgentResponse { id: id.clone() }),
    ))
}

/// Any authenticated identity (operator or agent) may list agents.
/// The response contains no secrets (only IDs, workspace paths, and state).
/// `?created_by=<id>` optionally restricts the result to a superior's direct
/// subagents. Today the unfiltered list is already unrestricted (any identity
/// sees every agent), so this filter adds no new leakage; revisit the exposure
/// if listing is ever scoped per-caller.
pub async fn list_agents(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Query(query): Query<ListAgentsQuery>,
) -> Json<ListAgentsResponse> {
    let registry = state.registry.read().await;
    let summaries: Vec<AgentSummary> = registry
        .iter()
        .filter(|(_, entry)| {
            query
                .created_by
                .as_ref()
                .is_none_or(|sup| entry.agent.config.created_by.as_ref() == Some(sup))
        })
        .map(|(id, entry)| entry.summary(id))
        .collect();
    Json(ListAgentsResponse { agents: summaries })
}

/// `PUT /agents/{id}/metadata` — update `role` and/or `description`.
///
/// Caller must be the direct supervisor (or operator); a grandparent cannot
/// relabel a grandchild. `None` fields are left unchanged; `role: Some(s)` must
/// be non-empty (role can be changed but not cleared — `description` can be
/// cleared with `Some("")`).
///
/// Persist-first-then-memory, both under one registry **write-lock**. The lock
/// serializes the whole op, which is what makes it correct: `rewrite_meta` is a
/// read-modify-write of `meta.json`, so without the write-lock two concurrent
/// PUTs (e.g. one setting role, one setting description) would lose an update,
/// and a concurrent `remove_agent` could archive the dir mid-write. The
/// write-lock held across one tiny JSON `atomic_write` briefly stalls concurrent
/// readers; that is acceptable for a rare mutation. Crash-safe — restore reads
/// meta as the source of truth.
pub async fn update_metadata(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(body): Json<UpdateAgentMetadataRequest>,
) -> Result<Json<AgentSummary>, ApiError> {
    // An explicit role set must not be empty (role is change-only, never clearable).
    if let Some(role) = &body.role
        && role.trim().is_empty()
    {
        return Err(ApiError::bad_request("'role' must not be empty"));
    }
    if body.role.is_none() && body.description.is_none() {
        return Err(ApiError::bad_request("no fields to update"));
    }

    let mut registry = state.registry.write().await;
    registry.require_direct_supervisor(auth.identity(), &id)?;
    let entry = registry
        .get_mut(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    let agent_dir = entry
        .agent
        .agent_dir
        .clone()
        .ok_or_else(|| ApiError::internal("agent has no on-disk directory to update"))?;

    // Persist first (disk is the source of truth across restarts), then memory.
    persistence::rewrite_meta(
        &agent_dir,
        body.role.as_deref(),
        body.description.as_deref(),
    )
    .map_err(ApiError::internal)?;
    if let Some(role) = &body.role {
        entry.agent.config.role = role.clone();
    }
    if let Some(desc) = &body.description {
        entry.agent.config.description = desc.clone();
    }
    Ok(Json(entry.summary(&id)))
}

/// `PUT /agents/{id}/activity` — the agent reports its current activity.
///
/// Self-only (or operator): the agent sets its own activity; a supervisor does
/// not (it observes via `list`). Writes the live cell — a registry **read-lock**
/// is enough because the cell is an interior-mutable `Arc<Mutex<String>>`.
/// Truncated to [`MAX_ACTIVITY_CHARS`] on a char boundary; an empty string
/// clears it (the bridge also auto-clears on terminal events).
pub async fn update_activity(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(body): Json<UpdateActivityRequest>,
) -> Result<StatusCode, ApiError> {
    let registry = state.registry.read().await;
    registry.require_self_or_operator(auth.identity(), &id)?;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    let mut activity = body.activity;
    truncate_chars(&mut activity, MAX_ACTIVITY_CHARS);
    *entry
        .agent
        .activity
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = activity;
    Ok(StatusCode::NO_CONTENT)
}

/// Maximum activity length (chars). Longer inputs are truncated, not rejected,
/// so a report never fails the agent's turn.
const MAX_ACTIVITY_CHARS: usize = 256;

/// Truncate `s` to at most `max` chars in place, on a char boundary.
fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() > max {
        s.truncate(s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len()));
    }
}

pub async fn remove_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, ApiError> {
    let entry = {
        let mut registry = state.registry.write().await;
        registry.require_superior(auth.identity(), &id)?;
        let Some(entry) = registry.get(&id) else {
            return Err(ApiError::not_found("agent not found"));
        };
        // Agent must be idle and have no subagents.
        if entry.agent.get_state() != AgentState::Idle {
            return Err(ApiError::conflict("agent is busy, interrupt it first"));
        }
        if !entry.subagent_ids.is_empty() {
            return Err(ApiError::conflict(
                "agent has active subagents, remove or interrupt them first",
            ));
        }
        // Unregister under the same write lock — should always succeed since
        // `get` above confirmed the agent exists. Defensive fallback in case
        // the invariant is violated by a future refactor.
        match registry.unregister(&id) {
            Some(e) => e,
            None => {
                return Err(ApiError::internal("agent vanished during removal"));
            }
        }
    };

    // Release all of this agent's directory write-locks (coupled to task death,
    // not registry removal — see DirLockManager invariants).
    state.lock_manager.release_all(&id);

    // Signal graceful cancellation; the agent persists on its way out.
    entry.agent.cancel.cancel();

    // The agent is idle, so its tasks finish in milliseconds: the agent task
    // persists and returns (dropping its sender), and the bridge exits on
    // channel-close (see `crate::bridge::bridge_task`). Await real completion
    // under a bound; force-abort only if a task is stuck.
    let bound = Duration::from_secs(crate::shutdown::REMOVE_AGENT_SHUTDOWN_TIMEOUT_SECS);
    if !entry.agent.shutdown(bound).await {
        warn!(id = %id, "agent did not shut down in time, force-aborted");
    }

    if let Err(e) = persistence::archive_agent_dir(&id) {
        info!(id = %id, "agent dir archive failed: {e:#}");
    }
    info!(id = %id, "archived agent");
    Ok(StatusCode::NO_CONTENT)
}

/// Interrupt the current agent operation without deleting it.
pub async fn interrupt_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, ApiError> {
    // Interrupt = cancel the current round only (the task stays alive and returns to its
    // outer loop). Cancels the round token if a round is in flight; a clean no-op when the
    // agent is idle (no round to abort). Distinct from `remove_agent`, which cancels the
    // lifecycle token and terminates the task.
    //
    // Clone the shared slot Arc under the registry read-lock, then release it before
    // touching the inner std Mutex — so the async registry guard is never held across the
    // (sync) round-cancel lock.
    let round_cancel = {
        let registry = state.registry.read().await;
        registry.require_superior(auth.identity(), &id)?;
        let Some(entry) = registry.get(&id) else {
            return Err(ApiError::not_found("agent not found"));
        };
        entry.agent.round_cancel.clone()
    };
    if let Some(round) = round_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        round.cancel();
    }
    Ok(StatusCode::ACCEPTED)
}

/// Validate supervisor constraints for a subagent creation request.
///
/// Returns `(PermissionProfile, ToolPolicy, ExecPolicy, PermissionClass)` for the
/// new subagent if valid. The subagent inherits the supervisor's exec-policy
/// overrides (cloned), so monotonic strictness holds at creation. The
/// `PermissionClass` is the ceiling for the child's model tier, clamped by the
/// supervisor's own class (the §2.3 ceiling invariant).
///
/// Lock ordering: `registry` RwLock is held when calling this function.
/// Inside, `tool_policy.read()` / `exec_policy.read()` acquire the per-agent
/// `std::sync::RwLock`s. Never acquire these in reverse order.
fn validate_subagent_request(
    registry: &crate::state::AgentRegistry,
    identity: &crate::auth::Identity,
    supervisor_id: &AgentId,
    workspace_root: &std::path::Path,
) -> Result<(PermissionProfile, ToolPolicy, ExecPolicy, PermissionClass), ApiError> {
    let supervisor = registry.require_supervisor(identity, supervisor_id)?;

    let supervisor_perms = &supervisor.agent.config.permissions;
    if supervisor_perms.max_depth == 0 {
        return Err(ApiError::forbidden(
            "supervisor has no remaining delegation depth",
        ));
    }
    let subagent_ws = workspace_root
        .canonicalize()
        .map_err(|e| ApiError::bad_request(format!("invalid workspace_root: {e}")))?;
    if !subagent_ws.starts_with(&supervisor_perms.workspace_root) {
        return Err(ApiError::forbidden(
            "workspace_root must be within supervisor's workspace",
        ));
    }

    let permissions = PermissionProfile::subagent(subagent_ws, supervisor_perms.max_depth);

    // Ceiling invariant (`.draft/design/agent-sandbox.md` §2.3): the child's
    // granted permission class cannot exceed its model tier's ceiling, nor its
    // supervisor's class. Depth monotonicity alone does NOT imply the latter
    // (tier 0/1 share Normal, 2/3 share Guest), so this is an explicit check — the
    // gate that keeps a weak model from ever being elevated. Default grant =
    // ceiling (full power for the tier); an explicit downgrade interface is a
    // later phase.
    let granted = PermissionClass::ceiling_for_tier(permissions.depth());
    let supervisor = &supervisor.agent;
    let supervisor_class = supervisor.config.permissions_class;
    if granted > supervisor_class {
        return Err(ApiError::forbidden(format!(
            "subagent permission class {granted:?} would exceed supervisor's {supervisor_class:?}"
        )));
    }

    let tool_policy = supervisor
        .tool_policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let exec_policy = supervisor
        .exec_policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    Ok((permissions, tool_policy, exec_policy, granted))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        AgentConfig, AgentId, MAX_ACTIVITY_CHARS, PermissionClass, PermissionProfile,
        acquire_workspace_lock, truncate_chars,
    };
    use crate::test_helpers::{make_entry, make_state};

    #[test]
    fn truncate_keeps_short_strings() {
        let mut s = String::from("abc");
        truncate_chars(&mut s, 10);
        assert_eq!(s, "abc");
        let mut s = String::new();
        truncate_chars(&mut s, 10);
        assert!(s.is_empty());
    }

    #[test]
    fn truncate_caps_on_char_boundary() {
        // "héllo" is 5 chars (é is one char, two bytes); cap at 2 → "hé".
        let mut s = String::from("héllo");
        truncate_chars(&mut s, 2);
        assert_eq!(s, "hé");
        let mut s = String::from("abcdef");
        truncate_chars(&mut s, 3);
        assert_eq!(s, "abc");
    }

    #[test]
    fn truncate_caps_to_max_activity_chars() {
        let mut s = "x".repeat(MAX_ACTIVITY_CHARS + 100);
        truncate_chars(&mut s, MAX_ACTIVITY_CHARS);
        assert_eq!(s.chars().count(), MAX_ACTIVITY_CHARS);
    }

    // -- acquire_workspace_lock (the :445 auto-acquire path, extracted) --

    /// A Normal `AgentConfig` rooted at `ws`, reusing `make_entry`'s template so
    /// every field is populated.
    fn normal_config(ws: &std::path::Path) -> AgentConfig {
        let mut config = make_entry(None, String::new()).agent.config;
        config.workspace_root = ws.to_path_buf();
        config.permissions = PermissionProfile::new(ws.to_path_buf());
        config.permissions_class = PermissionClass::Normal;
        config.created_by = None;
        config
    }

    /// Unique existing temp dir (acquire canonicalizes the path).
    fn ws_dir(label: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ja-acquire-ws-test-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn acquire_workspace_lock_normal_root_acquires() {
        let state = make_state();
        let root = AgentId::from("root".to_owned());
        let ws = ws_dir("root");
        let cfg = normal_config(&ws);
        let guard = acquire_workspace_lock(&state, &root, &cfg, &[], None, &ws, "ws").await;
        let guard = guard.unwrap().expect("Normal root acquires its workspace");
        // Lock is held and will release on drop.
        assert_eq!(state.lock_manager.holder(&ws).unwrap(), Some(root.clone()));
        drop(guard);
        assert!(state.lock_manager.holder(&ws).unwrap().is_none());
    }

    #[tokio::test]
    async fn acquire_workspace_lock_nested_child_no_longer_conflicts() {
        // The original bug: a Normal root holding /proj made any Normal nested
        // child's workspace acquire 409. With the chain, the child acquires.
        let state = make_state();
        let root = AgentId::from("root".to_owned());
        let root_ws = ws_dir("proj");
        let child_ws = root_ws.join("sub");
        std::fs::create_dir_all(&child_ws).unwrap();

        // Root holds /proj for the duration of the child acquire.
        let root_guard = acquire_workspace_lock(
            &state,
            &root,
            &normal_config(&root_ws),
            &[],
            None,
            &root_ws,
            "ws",
        )
        .await
        .unwrap()
        .unwrap();

        // Child's chain contains root → delegation, not conflict.
        let mut child_cfg = normal_config(&child_ws);
        child_cfg.created_by = Some(root.clone());
        let child = AgentId::from("child".to_owned());
        let child_guard = acquire_workspace_lock(
            &state,
            &child,
            &child_cfg,
            std::slice::from_ref(&root),
            Some(&root),
            &child_ws,
            "ws",
        )
        .await
        .unwrap()
        .expect("nested child acquires via delegation chain");
        // Carve-out: the child's region appears read-only in the root's view.
        let ro = state.lock_manager.readonly_paths(&root).unwrap();
        assert_eq!(ro, vec![std::fs::canonicalize(&child_ws).unwrap()]);
        drop(child_guard);
        drop(root_guard);
    }

    #[tokio::test]
    async fn acquire_workspace_lock_peer_without_chain_conflicts() {
        // Same topology, but the acquirer is NOT a delegation descendant
        // (empty chain) → Busy → Err(conflict), the pre-fix behavior.
        let state = make_state();
        let root = AgentId::from("root".to_owned());
        let root_ws = ws_dir("proj2");
        let nested = root_ws.join("sub");
        std::fs::create_dir_all(&nested).unwrap();

        let _root_guard = acquire_workspace_lock(
            &state,
            &root,
            &normal_config(&root_ws),
            &[],
            None,
            &root_ws,
            "ws",
        )
        .await
        .unwrap()
        .unwrap();

        let peer = AgentId::from("peer".to_owned());
        let err = acquire_workspace_lock(
            &state,
            &peer,
            &normal_config(&nested),
            &[],
            None,
            &nested,
            "ws",
        )
        .await
        .err()
        .expect("peer without chain must conflict");
        assert_eq!(err.status, 409);
    }

    #[tokio::test]
    async fn acquire_workspace_lock_guest_acquires_nothing() {
        let state = make_state();
        let id = AgentId::from("guest".to_owned());
        let ws = ws_dir("guest");
        let mut cfg = normal_config(&ws);
        cfg.permissions_class = PermissionClass::Guest;
        let guard = acquire_workspace_lock(&state, &id, &cfg, &[], None, &ws, "ws")
            .await
            .unwrap();
        assert!(guard.is_none());
        assert!(state.lock_manager.holder(&ws).unwrap().is_none());
    }
}
