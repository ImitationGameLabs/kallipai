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
use kallip_common::authtoken::{MintedToken, TokenHash};
use kallip_common::policy::{ExecPolicy, PolicyPreset};
use kallip_common::protocol::ApiError;
use kallip_common::protocol::SseEvent;
use kallip_runtime::agent_task::{self, AgentContext};
use kallip_runtime::approval::ApprovalStore;
use kallip_runtime::config::{
    AgentConfig, DelegationMode, PermissionClass, PermissionProfile, permission_class_from_env,
};
use kallip_runtime::context::{AgenticContext, ContextStore, ContextSummarizer};
use kallip_runtime::history::HistoryWriter;
use kallip_runtime::persistence;
use kallip_runtime::policy::{AgentPolicy, AuthorizedToolExecutor};
use kallip_runtime::tools::{
    ToolDispatchInputs, build_tool_dispatch, load_skill, meta_skill_content, skill_dir,
};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use kallip_common::protocol::{
    CreateAgentRequest, CreateAgentResponse, ListAgentsQuery, UpdateActivityRequest,
    UpdateAgentMetadataRequest,
};

use super::ListAgentsResponse;
use crate::bridge::bridge_task;
use crate::state::{
    Agent, AgentEntry, AgentIdentity, AgentState, AgentSummary, RegistryEntry, SharedState,
};
use crate::token::AGENT;

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

/// Inject the per-agent identity env vars into an existing env map.
///
/// `KALLIP_ROOT_AGENT_ID` is always set; `KALLIP_SUPERVISOR_AGENT_ID` is set
/// only when `supervisor_agent_id` is `Some` — for the root agent it is left
/// **unset** (not set to empty) so root-ness is detectable by env absence
/// rather than empty-string parsing. Always re-derives from the caller's
/// current `supervisor` and `root` (overwriting any prior values), so it is
/// safe to call on a reused env map. Shared by fresh spawns (via
/// [`SpawnArgs::default_env`]) and the reactivation path (which reuses the
/// dead incarnation's env map).
pub(crate) fn inject_identity_env(
    env: &mut HashMap<String, String>,
    supervisor_agent_id: Option<&AgentId>,
    root_agent_id: &AgentId,
) {
    if let Some(supervisor) = supervisor_agent_id {
        env.insert("KALLIP_SUPERVISOR_AGENT_ID".into(), supervisor.to_string());
    } else {
        env.remove("KALLIP_SUPERVISOR_AGENT_ID");
    }
    env.insert("KALLIP_ROOT_AGENT_ID".into(), root_agent_id.to_string());
}

/// Resolve the root agent id for a spawn. The root is the tagma's single
/// registered root ([`AgentRegistry::root_agent`](crate::state::AgentRegistry::root_agent)),
/// always knowable at runtime and independent of the supervisor chain — so a
/// broken chain never degrades the root identity to self.
///
/// `registry_root` is `None` only when the registry has no root, which violates
/// the single-root invariant a live tagma maintains; the `expect` surfaces that
/// impossible state loudly instead of silently substituting a wrong id.
pub(crate) fn resolve_root_agent(registry_root: Option<&AgentId>) -> AgentId {
    registry_root
        .cloned()
        .expect("a live tagma always has a registered root agent")
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

/// Per-agent identity section injected at the head of every system prompt. This
/// is the ONLY part of the prompt that varies across agents; the static-shared
/// bulk (base prompt + bootstrap meta-skill) that follows is byte-identical for
/// every agent. Kept as `const` templates with `{placeholder}` substitution
/// (see `compose_system_prompt`) so the prose stays readable and the per-agent
/// diff is reviewable in one place.
const IDENTITY_ROOT: &str = "\
# Your identity

You are the root agent of this tagma — the leader of a multi-agent system. \
You own the conversation with the user and may spawn subagents to delegate \
scoped work.

- agent id: `{agent_id}`
- role: `{role}`
- permission class: `{permission_class}` ({permission_class_hint})
- skills path: `{skills_path}`

# Operating as the root

Spawn a subagent with `kallip subagent spawn` (run via bash_exec; the kallip \
command is auto-allowed). Subagents report back by messaging your id; \
inter-agent messages arrive as input carrying a `[From: agent ...]` header. \
Address the user with `kallip lesche send \"<text>\"`. A message that arrives \
from a multi-member room carries a `[From: ... | room <room_id>]` header — \
reply in that SAME room with `kallip lesche send --room <room_id> \"<text>\"` \
(copy the room id verbatim from the header). In a room header the parenthesized \
tagma id is the cryptographically-authenticated sender identity (trust that, \
not the leading display handle, which is advisory); use plain `kallip lesche \
send` (no `--room`) only for the direct 1:1 user conversation. Run \
`kallip lesche rooms` to list the rooms you have joined and `kallip lesche read \
--room <room_id>` to pull a room's recent history.";

const IDENTITY_SUBAGENT: &str = "\
# Your identity

You are a subagent in a multi-agent system — you assist your supervisor and \
the root agent in completing their work.

- agent id: `{agent_id}`
- role: `{role}`
- description: `{description}`
- permission class: `{permission_class}` ({permission_class_hint})
- supervisor: `{supervisor_id}`
- root agent: `{root_id}`
- skills path: `{skills_path}`

# Operating as a subagent

Inter-agent messages arrive as input carrying a `[From: agent ...]` header. \
Report results to your supervisor with `kallip message {supervisor_id} \
\"<text>\"`; escalate to the root with `kallip message {root_id} ...`. \
Do not address the user directly — the root owns the user conversation and \
the lesche route rejects non-root callers.";

/// Compose the full system prompt for an agent: a per-agent `# Your identity`
/// section (templated from config + spawn-time ids) followed by the
/// static-shared bulk (the base prompt from `config.system_prompt`, then the
/// bootstrap meta-skill).
///
/// The identity section is the only part that differs across agents. The
/// static-shared tail is byte-identical for every agent within a deployment
/// (because `config.system_prompt` resolves a tagma-global env var or the
/// shared default), so provider prefix-caching of that suffix is preserved.
fn compose_system_prompt(
    config: &AgentConfig,
    agent_id: AgentId,
    root_agent_id: AgentId,
) -> String {
    // `format!` requires a string literal, so the const template is rendered
    // with plain `{placeholder}` substitution. The placeholder spelling is the
    // singular `permission_class` for prose readability; the struct field is the
    // historical-plural `permissions_class`. Substitution order matters:
    // user-controlled free text (`role`, `description`) is substituted LAST so
    // a value containing a `{...}` fragment cannot be re-scanned by an earlier
    // placeholder's pass — its literal braces survive by design (pinned by
    // `compose_system_prompt_user_text_with_braces_is_not_rescanned`). The
    // remaining placeholders are all agent-controlled ids or enum names (no
    // braces), so tests assert no `{`/`}` from them remains in the rendered
    // output; a typo'd placeholder name (leaving a literal `{...}`) fails
    // loudly.
    let permission_class_hint = match config.permissions_class {
        PermissionClass::Normal => "owns a read-write workspace and home directory",
        PermissionClass::Guest => "readonly workspace, no home write",
    };
    // Absolute skills dir, resolved the same way `skill_dir()` does for the
    // tool layer — surfacing it here spares the agent from probing XDG paths.
    // Failure is near-impossible in a running tagma (skill loading already
    // depends on it); fall back to a placeholder rather than abort the prompt.
    let skills_path = skill_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unresolved>".to_owned());
    let identity = if config.is_root() {
        IDENTITY_ROOT
            .replace("{agent_id}", agent_id.as_ref())
            .replace("{permission_class}", &config.permissions_class.to_string())
            .replace("{permission_class_hint}", permission_class_hint)
            .replace("{skills_path}", &skills_path)
            // User-controlled — substitute last so its value is not re-scanned.
            .replace("{role}", &config.role)
    } else {
        IDENTITY_SUBAGENT
            .replace("{agent_id}", agent_id.as_ref())
            .replace("{permission_class}", &config.permissions_class.to_string())
            .replace("{permission_class_hint}", permission_class_hint)
            .replace(
                "{supervisor_id}",
                // Reached only when `!config.is_root()`, i.e. `created_by` is Some.
                config
                    .created_by
                    .clone()
                    .expect("subagent has created_by")
                    .as_ref(),
            )
            .replace("{root_id}", root_agent_id.as_ref())
            .replace("{skills_path}", &skills_path)
            // User-controlled — substitute last so their values are not re-scanned.
            .replace("{description}", &config.description)
            .replace("{role}", &config.role)
    };
    let mut full = identity;
    full.push_str("\n\n");
    full.push_str(&config.system_prompt);
    full.push_str("\n\n");
    full.push_str(meta_skill_content());
    full
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
        notice_sink,
        exec_policy: args.exec_policy.clone(),
        lock_manager: args.shared_state.lock_manager.clone(),
        agent_id: args.agent_id.clone(),
        exec_gate: exec_gate.clone(),
    })
    .await?;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);

    let executor = AuthorizedToolExecutor::new(
        dispatch,
        AgentPolicy::new(args.exec_policy.clone(), args.preset),
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
        retry_notify: retry_notify.clone(),
        retry_at: Arc::new(std::sync::Mutex::new(None)),
        transient_fails: 0,
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
/// restore bails and the caller registers the agent `Faulted` (its children are
/// still attempted); reactivation refuses to wake the agent into a state where
/// it cannot write its own workspace (returning conflict to the sender).
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

/// The result of [`establish_workspace_lock`]: the workspace write-lock guard
/// and, for a `FullHandoff` spawn, the data to reverse the forward transfer.
///
/// # Drop ordering is structural
///
/// The custom [`Drop`] runs the reverse handoff transfer (if armed) BEFORE the
/// `workspace` field auto-drops. This is load-bearing: the transfer back must
/// run while the dirlock's `writer == child`, before
/// [`WorkspaceLockGuard`]'s `Drop` runs `release_all(child)` and clears the
/// entry. If `release_all` ran first, `writer` would become `None` and the
/// back-transfer would be a `NotOwner` no-op, permanently stranding the lock.
/// Encoding the reverse transfer in the manual `Drop` body (which always runs
/// before field autodrop) makes this previously prose-only invariant
/// structural — every caller that lets an `EstablishedLock` go out of scope on
/// an error path gets the right order for free.
///
/// On the success path the caller calls [`EstablishedLock::disarm`] before the
/// agent is considered registered, then drops the value (a no-op).
pub(crate) struct EstablishedLock<'a> {
    handoff: Option<HandoffRollback<'a>>,
    workspace: Option<WorkspaceLockGuard<'a>>,
}

/// The reverse half of a full-handoff transfer, undone on unwind. A plain
/// struct (no `Drop`): [`EstablishedLock`]'s manual `Drop` performs the
/// transfer so the ordering relative to the workspace guard's release is
/// explicit at one site. `armed` flips to false on the success path.
struct HandoffRollback<'a> {
    state: &'a SharedState,
    child: AgentId,
    supervisor: AgentId,
    ws: std::path::PathBuf,
    armed: bool,
}

impl<'a> EstablishedLock<'a> {
    /// Disarm both the handoff rollback and the workspace guard. Call exactly
    /// once, on the success path after the agent is registered, so the imminent
    /// `Drop` neither reverses the handoff transfer nor releases the workspace
    /// lock.
    pub(crate) fn disarm(&mut self) {
        if let Some(h) = self.handoff.as_mut() {
            h.armed = false;
        }
        if let Some(g) = self.workspace.as_mut() {
            g.disarm();
        }
    }
}

impl Drop for EstablishedLock<'_> {
    fn drop(&mut self) {
        // Reverse the forward handoff transfer WHILE the dirlock writer is still
        // the child, i.e. before the `workspace` field auto-drops and runs
        // `release_all(child)`. Rust runs this manual body before field autodrop.
        if let Some(h) = self.handoff.as_ref()
            && h.armed
        {
            let _ = h
                .state
                .lock_manager
                .transfer(&h.child, &h.supervisor, &h.ws);
        }
    }
}

/// Failure to [`establish_workspace_lock`]. Each variant's doc states whether
/// any dirlock mutation happened before the error, so callers know whether the
/// lock state is intact (the helper itself guarantees the as-if-never-ran
/// contract documented on [`establish_workspace_lock`]). The `Display` impl is
/// the single source for the message prose; callers only pick a status code.
#[derive(Debug)]
pub(crate) enum EstablishLockFailure {
    /// A `FullHandoff` config with no `created_by`. Rejected before any dirlock
    /// mutation, so the state is untouched. (Spawn reaches this only on a
    /// corrupted `meta.json` at restore time; `validate_subagent_request`
    /// guarantees a live supervisor at spawn time.)
    HandoffWithoutSupervisor,
    /// The forward handoff transfer itself errored. Nothing followed it, so
    /// there is nothing to reverse.
    ForwardTransferFailed(std::io::Error),
    /// Another agent holds an overlapping write-lock. The forward transfer (if
    /// any) has already been reversed before this is returned.
    Busy { holder: AgentId, conflict: PathBuf },
    /// The acquire itself errored (e.g. an unresolvable workspace path). The
    /// forward transfer (if any) has already been reversed before this is
    /// returned.
    AcquireFailed(std::io::Error),
}

impl std::fmt::Display for EstablishLockFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HandoffWithoutSupervisor => {
                f.write_str("full-handoff agent has no supervisor (created_by); corrupt meta.json")
            }
            Self::ForwardTransferFailed(io) => {
                write!(f, "full-handoff lock transfer failed: {io}")
            }
            Self::Busy { holder, conflict } => write!(
                f,
                "workspace overlaps a write-lock on {} held by agent {holder}; \
                 remove it or choose a non-overlapping workspace",
                conflict.display()
            ),
            Self::AcquireFailed(io) => write!(f, "failed to acquire workspace lock: {io}"),
        }
    }
}

/// Execute the workspace carve-out that every materialization path shares:
/// the `FullHandoff` forward transfer, the workspace write-lock acquire, and the
/// reverse-transfer rollback guard. This is the single place the carve logic
/// lives, so spawn ([`Materialize::run`]) and restore (`restore_one`) cannot
/// drift apart as they did when each inlined its own copy.
///
/// # Error contract
///
/// On `Ok`, the dirlock state reflects the child (writer == child for the
/// workspace; a nested lock for a `CarveOut` subdir) and the returned
/// [`EstablishedLock`] holds the rollback guards. On `Err`, the dirlock state is
/// **as-if this call never ran**: a forward transfer that succeeded is reversed
/// before a `Busy`/`AcquireFailed` is returned, and the two pre-mutation
/// variants (`HandoffWithoutSupervisor`, `ForwardTransferFailed`) leave nothing
/// to reverse. The caller is responsible only for *persistence* rollback (the
/// agent dir), not the dirlock.
pub(crate) fn establish_workspace_lock<'a>(
    state: &'a SharedState,
    id: &'a AgentId,
    config: &AgentConfig,
    chain: &[AgentId],
) -> Result<EstablishedLock<'a>, EstablishLockFailure> {
    let is_handoff = config.delegation_mode == DelegationMode::FullHandoff;
    // Resolve the supervisor id once. `HandoffWithoutSupervisor` returns before
    // any mutation; this replaces the prior `.expect` that crashed restore on a
    // corrupted meta.
    let supervisor_id: Option<AgentId> = match (is_handoff, config.created_by.as_ref()) {
        (true, None) => return Err(EstablishLockFailure::HandoffWithoutSupervisor),
        (true, Some(s)) => Some(s.clone()),
        (false, _) => None,
    };

    // Forward handoff: transfer the supervisor's root lock to the child BEFORE
    // the acquire, so the child's acquire sees writer==child (AlreadyHeld),
    // not an exact-path Busy against the supervisor. Atomic under the dirlock
    // mutex. A failure here leaves nothing to reverse.
    if let Some(s) = &supervisor_id {
        state
            .lock_manager
            .transfer(s, id, &config.workspace_root)
            .map_err(EstablishLockFailure::ForwardTransferFailed)?;
    }

    // Acquire. On failure, reverse the forward transfer (if any) so the
    // supervisor keeps its lock rather than the discarded child id.
    let workspace = match try_acquire_workspace_lock(state, id, config, chain) {
        Ok(guard) => guard,
        Err(WorkspaceAcquireFailure::Busy { holder, conflict }) => {
            if let Some(s) = &supervisor_id {
                let _ = state.lock_manager.transfer(id, s, &config.workspace_root);
            }
            return Err(EstablishLockFailure::Busy { holder, conflict });
        }
        Err(WorkspaceAcquireFailure::Other(e)) => {
            if let Some(s) = &supervisor_id {
                let _ = state.lock_manager.transfer(id, s, &config.workspace_root);
            }
            return Err(EstablishLockFailure::AcquireFailed(e));
        }
    };

    // Rollback for failures AFTER this point (spawn/registration). Reversed by
    // `EstablishedLock`'s manual `Drop` before the workspace guard releases.
    let handoff = supervisor_id.map(|s| HandoffRollback {
        state,
        child: id.clone(),
        supervisor: s,
        ws: config.workspace_root.clone(),
        armed: true,
    });

    Ok(EstablishedLock { handoff, workspace })
}

/// Map an [`EstablishLockFailure`] to a client-facing [`ApiError`]. The dirlock
/// is already restored by [`establish_workspace_lock`] on every error variant,
/// so the caller only needs to roll back persistence (the agent dir). Only the
/// status code is chosen here; the message prose comes from the `Display` impl.
///
/// Note: the `ApiError::internal` arms log their message server-side and return
/// a generic `"internal error"` to the client (see `ApiError::internal`) -- the
/// prose there is for operators, not the HTTP body.
fn establish_lock_api_error(e: EstablishLockFailure) -> ApiError {
    use EstablishLockFailure::*;
    match &e {
        Busy { .. } => ApiError::conflict(e.to_string()),
        AcquireFailed(_) => ApiError::bad_request(e.to_string()),
        HandoffWithoutSupervisor | ForwardTransferFailed(_) => ApiError::internal(e.to_string()),
    }
}

/// Map an exec-gate WRITE failure (a carve-out refused because the supervisor
/// has an in-flight shell) to a client-facing conflict. The carve is retried by
/// the caller once the supervisor is quiescent.
fn exec_gate_failure(failure: kallip_runtime::ExecGateFailure) -> ApiError {
    use kallip_runtime::ExecGateFailure;
    let msg = match failure {
        ExecGateFailure::ForegroundExecInProgress => {
            "supervisor has an in-flight shell; wait for it to finish and retry".to_string()
        }
        ExecGateFailure::BgTasksRunning(n) => format!(
            "supervisor has {n} running background task(s); wait for them to finish and retry"
        ),
    };
    ApiError::conflict(msg)
}

/// Materialize one agent end-to-end from a fully-resolved config: workspace
/// disjoint check, agent dir + skills, workspace write-lock, task spawn, agent
/// cap, and registration. Owns the [`WorkspaceLockGuard`] and disarms it on
/// success so a Normal agent keeps its workspace lock for life.
///
/// This is the shared tail of every agent-creation path. The two callers build
/// the head themselves and hand off:
/// - [`create_agent`] resolves a *subagent* (supervisor validation, permission
///   ceiling, exec-policy inheritance, pre-reserved slot) and passes
///   `rollback_supervisor: Some(…)`.
/// - [`ensure_root_agent`] resolves the tagma singleton *root* (env-driven
///   config, default exec-policy) and passes `rollback_supervisor: None`.
///
/// `rollback_supervisor` doubles as the creation shape: `None` means this is the
/// root, registered via [`crate::state::AgentRegistry::register_root`]; `Some`
/// means a subagent, registered via `register_no_subagent_push` (slot
/// pre-reserved by the caller) and retracted from that supervisor on failure.
struct Materialize<'a> {
    state: &'a SharedState,
    id: AgentId,
    token: MintedToken,
    config: AgentConfig,
    exec_policy: ExecPolicy,
    rollback_supervisor: Option<AgentId>,
}

impl<'a> Materialize<'a> {
    async fn run(self) -> Result<AgentId, ApiError> {
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
        let tier = state.profiles.select_profile(depth).clone();

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

/// `POST /agents` — spawn a **subagent** under an existing supervisor.
///
/// The tagma's single root agent is tagma-managed (eagerly created at startup
/// by [`ensure_root_agent`]); it cannot be created over HTTP. A request with no
/// `created_by` is rejected with `409 Conflict`. Subagent creation requires
/// operator or direct-supervisor privilege and is gated by the per-supervisor
/// `max_subagents` limit.
pub async fn create_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // The root is tagma-managed; clients spawn subagents with `created_by` set.
    let Some(supervisor_id) = req.created_by else {
        return Err(ApiError::conflict(
            "root agent is managed by the tagma; spawn a subagent with 'created_by' set",
        ));
    };

    let id = AgentId::random();
    // Mint a fresh 256-bit `sk-agent-…` token. The plaintext goes into the agent
    // shell env (`KALLIP_AUTH_TOKEN`); only its SHA-256 is indexed for auth lookup.
    let token = MintedToken::generate(AGENT);

    let mut config = {
        let ws = std::path::PathBuf::from(&req.workspace_root);
        AgentConfig::load(req.prompt, req.skills, Some(ws))
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
    config.delegation_mode = req
        .delegation_mode
        .as_deref()
        .unwrap_or(kallip_common::protocol::DELEGATION_CARVE_OUT)
        .parse::<kallip_runtime::config::DelegationMode>()
        .map_err(ApiError::bad_request)?;
    // Fleet discipline: a subagent spawn must carry a non-empty role so a
    // superior can tell its subagents apart.
    if config.role.trim().is_empty() {
        return Err(ApiError::bad_request(
            "subagent requires a non-empty 'role'",
        ));
    }
    // Reject any workspace that overlaps the tagma data tree BEFORE reserving
    // the subagent slot, so a rejected workspace leaves no dangling slot.
    // (`validate_subagent_request` already confines a subagent's workspace within
    // its supervisor's, which is itself disjoint, so this is a backstop here; it
    // is load-bearing for the tagma-owned root, where it is checked in
    // `ensure_root_agent`.)
    persistence::ensure_workspace_disjoint(&config.workspace_root)
        .map_err(|e| ApiError::conflict(e.to_string()))?;

    // Parse the optional downgrade request once, before taking the lock, so a
    // bad spelling is a cheap client-side 400 rather than a held-write-lock
    // rejection. The tagma is the reference monitor: a value is accepted only
    // as a downgrade, clamped to the tier ceiling and supervisor class inside
    // validate_subagent_request.
    let requested_class = parse_requested_class(&req.permission_class)?;
    // Subagent head: validate supervisor + delegation constraints and pre-reserve
    // the slot under write lock to eliminate TOCTOU. The tagma-global preset
    // applies to every agent, so only the per-agent exec-policy is resolved here.
    let exec_policy = {
        let mut registry = state.registry.write().await;
        let (permissions, exec, permission_class) = validate_subagent_request(
            &registry,
            auth.identity(),
            &supervisor_id,
            &config.workspace_root,
            requested_class,
            config.delegation_mode,
        )?;
        // Check per-agent subagent limit and pre-reserve the slot.
        let supervisor = registry
            .get_mut(&supervisor_id)
            .ok_or_else(|| ApiError::not_found("supervisor not found"))?;
        if supervisor.subagent_ids().len() >= state.max_subagents {
            return Err(ApiError::unavailable(format!(
                "supervisor has {}/{max} subagents, cannot create more",
                supervisor.subagent_ids().len(),
                max = state.max_subagents
            )));
        }
        // Pre-reserve: push the new ID so concurrent requests see the updated count.
        supervisor.subagent_ids_mut().push(id.clone());
        config.created_by = Some(supervisor_id.clone());
        config.permissions = permissions;
        config.permissions_class = permission_class;
        exec
    };

    let id = Materialize {
        state: &state,
        id,
        token,
        config,
        exec_policy,
        rollback_supervisor: Some(supervisor_id),
    }
    .run()
    .await?;

    Ok((StatusCode::CREATED, Json(CreateAgentResponse { id })))
}

/// Ensure the tagma's single root agent exists. Called once at startup, after
/// [`restore_agents`](super::restore::restore_agents) and before the router
/// accepts connections, so the root is always present for clients. Idempotent:
/// if restore already brought the root back (the common case), this is a no-op.
///
/// The root's config is env-driven via [`AgentConfig::load`] — notably
/// `KALLIP_WORKSPACE_ROOT` and `KALLIP_MAX_TOOL_ROUNDS` — and its permission
/// class comes from `KALLIP_ROOT_AGENT_PERMISSION_CLASS`. The root is persisted
/// by the normal spawn path, so its id is stable across restarts.
pub async fn ensure_root_agent(state: &SharedState) -> anyhow::Result<()> {
    {
        let registry = state.registry.read().await;
        if registry.root_agent().is_some() {
            return Ok(());
        }
    }
    let id = AgentId::random();
    let token = MintedToken::generate(AGENT);
    let mut config = AgentConfig::load(None, Vec::new(), None)?;
    config.agent_id = Some(id.clone());
    config.permissions_class = permission_class_from_env();
    config.role = "root".to_string();
    config.description = "Tagma-owned root agent".to_string();
    // Reject any workspace that overlaps the tagma data tree (e.g. a tagma
    // launched from inside its own data dir). Surfaced as a startup error via
    // the `?` in `main.rs`.
    persistence::ensure_workspace_disjoint(&config.workspace_root)
        .map_err(|e| anyhow::anyhow!("root workspace overlaps the data tree: {e}"))?;
    let created = Materialize {
        state,
        id,
        token,
        config,
        exec_policy: ExecPolicy::default(),
        rollback_supervisor: None,
    }
    .run()
    .await?;
    info!(agent = %created, "created tagma root agent");
    Ok(())
}

/// `GET /agents/root` — return the tagma's single root agent. Always present
/// after startup (see [`ensure_root_agent`]); a missing root is a startup
/// invariant violation surfaced as `500`, never `404` (a 404 would invite
/// clients back into the check-then-create pattern this refactor removes).
pub async fn get_root_agent(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
) -> Result<Json<AgentSummary>, ApiError> {
    let conversation_id = state.external.get().and_then(|p| p.conversation_id());
    let registry = state.registry.read().await;
    let (id, entry) = registry
        .root_agent()
        .ok_or_else(|| ApiError::internal("root agent missing — startup invariant violated"))?;
    let mut summary = entry.summary(id);
    summary.conversation_id = conversation_id;
    Ok(Json(summary))
}

/// Verify the caller's bearer matches the named agent id: `204` on match,
/// `401` otherwise. Lets a trusted peer service (e.g. `kallip-cron`) confirm an
/// `(agent_id, token)` pair a client presented, without that service holding the
/// agent-token index itself. The bearer resolves via the standard
/// `crate::auth::AuthIdentity` path; an operator bearer does not match any agent
/// id.
pub async fn verify_agent(
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, ApiError> {
    match auth.identity() {
        crate::auth::Identity::Agent { id: aid } if *aid == id => Ok(StatusCode::NO_CONTENT),
        _ => Err(ApiError::unauthorized("agent id does not match token")),
    }
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
                .is_none_or(|sup| entry.identity().config.created_by.as_ref() == Some(sup))
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
        .identity()
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
        entry.identity_mut().config.role = role.clone();
    }
    if let Some(desc) = &body.description {
        entry.identity_mut().config.description = desc.clone();
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
    let live = entry
        .as_live()
        .ok_or_else(|| ApiError::conflict("agent is faulted; cannot report activity"))?;
    let mut activity = body.activity;
    truncate_chars(&mut activity, MAX_ACTIVITY_CHARS);
    *live
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
        // The tagma-owned singleton root is non-removable while *live* — clients
        // target subagents. A *faulted* root (e.g. a restore failure) is
        // removable so the operator can recover through the API: archiving it
        // frees the slot, and the next tagma restart re-creates a fresh root
        // (`ensure_root_agent`). Key on identity + liveness, not on
        // `created_by == None`, so faulted duplicate roots also stay removable.
        if registry
            .root_agent()
            .is_some_and(|(root_id, entry)| root_id == &id && entry.as_live().is_some())
        {
            return Err(ApiError::conflict(
                "root agent is tagma-managed and cannot be removed",
            ));
        }
        registry.require_superior(auth.identity(), &id)?;
        let Some(entry) = registry.get(&id) else {
            return Err(ApiError::not_found("agent not found"));
        };
        // Live agents must be idle and have no subagents. Faulted agents have no
        // task to be busy, so the idle check is skipped; the no-subagents check
        // still applies (remove children first).
        if let Some(live) = entry.as_live()
            && live.agent.get_state() != AgentState::Idle
        {
            return Err(ApiError::conflict("agent is busy, interrupt it first"));
        }
        if !entry.subagent_ids().is_empty() {
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

    // FullHandoff: return the workspace lock to the supervisor before the
    // child's locks are released. The transfer reassigns the ws entry from the
    // child to the supervisor; `release_all(child)` below then clears any OTHER
    // locks the child held (the ws entry is now the supervisor's, so it is
    // untouched). `NotOwner` (supervisor gone / lock already released) is
    // expected on racy removal -- logged, not fatal.
    if entry.identity().config.delegation_mode == DelegationMode::FullHandoff
        && let Some(supervisor_id) = entry.identity().config.created_by.as_ref()
    {
        match state.lock_manager.transfer(
            &id,
            supervisor_id,
            &entry.identity().config.workspace_root,
        ) {
            Ok(kallip_runtime::dirlock::TransferOutcome::Transferred) => {}
            Ok(kallip_runtime::dirlock::TransferOutcome::NotOwner) => {
                debug!(
                    child = %id,
                    supervisor = %supervisor_id,
                    "full-handoff transfer-back was NotOwner (lock already released or supervisor gone)"
                );
            }
            Err(e) => {
                warn!(child = %id, supervisor = %supervisor_id, "full-handoff transfer-back failed: {e}");
            }
        }
    }
    // Release all of this agent's directory write-locks (coupled to task death,
    // not registry removal — see DirLockManager invariants). A no-op for
    // faulted entries, which never acquired locks.
    state.lock_manager.release_all(&id);

    match entry {
        crate::state::RegistryEntry::Live(live) => {
            // Signal graceful cancellation; the agent persists on its way out.
            live.agent.cancel.cancel();

            // The agent is idle, so its tasks finish in milliseconds: the agent task
            // persists and returns (dropping its sender), and the bridge exits on
            // channel-close (see `crate::bridge::bridge_task`). Await real completion
            // under a bound; force-abort only if a task is stuck.
            let bound = Duration::from_secs(crate::shutdown::REMOVE_AGENT_SHUTDOWN_TIMEOUT_SECS);
            if !live.agent.shutdown(bound).await {
                error!(id = %id, "agent did not shut down in time, force-aborted");
            }
        }
        crate::state::RegistryEntry::Faulted(_) => {
            // No task to cancel or await -- go straight to archival so the
            // operator can clean up the orphaned data dir.
            info!(id = %id, "removing faulted agent (no task to shut down)");
        }
    }

    if let Err(e) = persistence::archive_agent_dir(&id) {
        warn!(id = %id, "agent dir archive failed: {e:#}");
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
        let live = entry
            .as_live()
            .ok_or_else(|| ApiError::conflict("agent is faulted; nothing to interrupt"))?;
        live.agent.round_cancel.clone()
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
/// Returns `(PermissionProfile, ExecPolicy, PermissionClass)` for the
/// new subagent if valid. The subagent inherits the supervisor's exec-policy
/// overrides (cloned), so monotonic strictness holds at creation. The classify
/// preset is tagma-global, so it is not part of the per-agent inheritance.
///
/// `requested_class` is the optional explicit downgrade from the spawn request
/// (already parsed from the wire string by the caller). When `None`, the child
/// is granted its model tier's ceiling (`ceiling_for_tier`); otherwise the
/// requested class is treated as a downgrade and is rejected with `forbidden` if
/// it exceeds the tier ceiling or the supervisor's own granted class. This is
/// the §2.3 ceiling invariant, enforced explicitly by the tagma as the trusted
/// reference monitor (depth monotonicity alone does NOT imply it — the tier 0/1
/// and 2/3 plateaus).
///
/// Lock ordering: `registry` RwLock is held when calling this function.
/// Inside, `exec_policy.read()` acquires the per-agent `std::sync::RwLock`.
fn validate_subagent_request(
    registry: &crate::state::AgentRegistry,
    identity: &crate::auth::Identity,
    supervisor_id: &AgentId,
    workspace_root: &std::path::Path,
    requested_class: Option<PermissionClass>,
    requested_mode: DelegationMode,
) -> Result<(PermissionProfile, ExecPolicy, PermissionClass), ApiError> {
    let supervisor_entry = registry.require_supervisor(identity, supervisor_id)?;
    // A faulted supervisor has no running task and no policy to inherit -- it
    // cannot host a new subagent.
    let supervisor = supervisor_entry
        .as_live()
        .ok_or_else(|| ApiError::conflict("supervisor is faulted; cannot spawn subagents"))?;

    // FullHandoff exclusivity: a full-handoff child takes the supervisor's
    // entire workspace write-lock, so it cannot coexist with any other child
    // (the carve topology and restore's concurrent sibling restore both require
    // the supervisor to have a single child while a full-handoff one lives).
    let existing = &supervisor.subagent_ids;
    if requested_mode == DelegationMode::FullHandoff && !existing.is_empty() {
        return Err(ApiError::conflict(
            "a full-handoff subagent requires the supervisor to have no other \
             subagents; remove them first",
        ));
    }
    for cid in existing {
        if let Some(child) = registry.get(cid)
            && child.identity().config.delegation_mode == DelegationMode::FullHandoff
        {
            return Err(ApiError::conflict(
                "supervisor already has a full-handoff subagent; remove it \
                 before spawning others",
            ));
        }
    }

    let supervisor_perms = &supervisor.identity.config.permissions;
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
    // FullHandoff transfers the supervisor's ENTIRE workspace write-lock to the
    // child via an exact-path `transfer(supervisor, child, ws)`. A proper
    // subdirectory would leave the supervisor holding its own root, so the
    // transfer is a `NotOwner` no-op and the child silently carves out the
    // subdirectory -- while the registry records FullHandoff and still enforces
    // its exclusivity, invisibly losing the handoff contract. Require identity.
    if requested_mode == DelegationMode::FullHandoff
        && subagent_ws != supervisor_perms.workspace_root
    {
        return Err(ApiError::bad_request(
            "full_handoff requires the subagent workspace to be the supervisor's \
             entire workspace root",
        ));
    }

    let permissions = PermissionProfile::subagent(subagent_ws, supervisor_perms.max_depth);

    // Ceiling invariant (`.draft/design/agent-sandbox.md` §2.3): the child's
    // granted permission class cannot exceed its model tier's ceiling, nor its
    // supervisor's granted class. The decision is delegated to
    // `resolve_granted_class`, a pure function unit-tested in isolation (the
    // depth monotonicity alone does NOT imply the ceiling monotonicity — tier
    // 0/1 share Normal, 2/3 share Guest — so these are explicit checks).
    let ceiling = PermissionClass::ceiling_for_tier(permissions.depth());
    let supervisor_class = supervisor.identity.config.permissions_class;
    let granted = resolve_granted_class(ceiling, supervisor_class, requested_class)?;

    // FullHandoff transfers the supervisor's workspace WRITE-lock to the child,
    // so the child must be Normal (a Guest is readonly: it skips the workspace
    // lock entirely, so a Guest FullHandoff would silently lose the lock on
    // reactivation -- release_all(child) clears it and the Guest never
    // re-acquires). Reject the combination up front.
    if requested_mode == DelegationMode::FullHandoff && granted != PermissionClass::Normal {
        return Err(ApiError::bad_request(
            "full-handoff requires permission_class normal (a Guest is readonly and \
             cannot hold the workspace write-lock)",
        ));
    }

    // The exec-policy is inherited from the supervisor (monotone: the tagma
    // validates the child stays at least as strict on the PUT path). The classify
    // preset is tagma-global, not per-agent, so it is not inherited here.
    let exec_policy = supervisor
        .agent
        .exec_policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    Ok((permissions, exec_policy, granted))
}

/// Parse the optional `permission_class` wire string (lowercase `"normal"` /
/// `"guest"`) into a typed class. A client spelling error is a `400 Bad
/// Request` here — distinct from the `403 Forbidden` the reference monitor
/// returns for a class that parses fine but exceeds the ceiling/supervisor.
fn parse_requested_class(raw: &Option<String>) -> Result<Option<PermissionClass>, ApiError> {
    use std::str::FromStr;
    match raw {
        None => Ok(None),
        Some(s) => Ok(Some(
            PermissionClass::from_str(s).map_err(|e| ApiError::bad_request(e.to_string()))?,
        )),
    }
}

/// Pure reference-monitor decision for the §2.3 ceiling invariant, separated
/// from `validate_subagent_request` so it can be unit-tested without building
/// a full `Agent`/registry. Returns the class to actually grant.
///
/// - `None` requested -> grant the tier `ceiling` (historical default).
/// - An explicit request is a **downgrade only**: anything above the ceiling or
///   the supervisor's own granted class is rejected with `forbidden`, never
///   silently clamped, so a caller mistake surfaces loudly. Because the gate
///   compares granted (not ceiling) classes, a supervisor that was itself
///   downgraded can no longer grant a child at its tier's default ceiling — the
///   intended "weak model can never escalate" property.
fn resolve_granted_class(
    ceiling: PermissionClass,
    supervisor_class: PermissionClass,
    requested: Option<PermissionClass>,
) -> Result<PermissionClass, ApiError> {
    let granted = requested.unwrap_or(ceiling);
    if granted > ceiling {
        return Err(ApiError::forbidden(format!(
            "requested permission class {granted} exceeds tier ceiling {ceiling}"
        )));
    }
    if granted > supervisor_class {
        return Err(ApiError::forbidden(format!(
            "requested permission class {granted} exceeds supervisor's {supervisor_class}"
        )));
    }
    Ok(granted)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::{
        AgentConfig, AgentId, DelegationMode, EstablishLockFailure, MAX_ACTIVITY_CHARS,
        PermissionClass, PermissionProfile, compose_system_prompt, establish_workspace_lock,
        inject_identity_env, interrupt_agent, list_agents, meta_skill_content, remove_agent,
        resolve_granted_class, resolve_root_agent, truncate_chars,
    };
    use crate::auth::{AuthIdentity, Identity};
    use crate::test_helpers::{
        add_faulted_root, add_faulted_sub, add_root, make_entry, make_state,
    };
    use axum::extract::{Path, Query, State};
    use kallip_common::protocol::ListAgentsQuery;

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

    // -- inject_identity_env (shared by fresh spawn + reactivation) --

    #[test]
    fn inject_identity_env_sets_root_always_and_supervisor_only_for_subagents() {
        let root = AgentId::from("root-1".to_owned());
        let sup = AgentId::from("sup-1".to_owned());

        // Root: supervisor unset, root set to self.
        let mut env = HashMap::new();
        inject_identity_env(&mut env, None, &root);
        assert_eq!(
            env.get("KALLIP_ROOT_AGENT_ID").map(String::as_str),
            Some("root-1")
        );
        assert!(
            !env.contains_key("KALLIP_SUPERVISOR_AGENT_ID"),
            "root must have no supervisor env (absent, not empty)"
        );

        // Subagent: both set.
        let mut env = HashMap::new();
        inject_identity_env(&mut env, Some(&sup), &root);
        assert_eq!(
            env.get("KALLIP_ROOT_AGENT_ID").map(String::as_str),
            Some("root-1")
        );
        assert_eq!(
            env.get("KALLIP_SUPERVISOR_AGENT_ID").map(String::as_str),
            Some("sup-1")
        );
    }

    #[test]
    fn inject_identity_env_clears_stale_supervisor_on_root() {
        // A reused env map may carry a stale `KALLIP_SUPERVISOR_AGENT_ID` from a
        // prior incarnation. The helper advertises "safe on a reused map", so
        // passing `None` (root) must REMOVE the stale key, not leave it.
        let root = AgentId::from("root-1".to_owned());
        let mut env = HashMap::new();
        env.insert("KALLIP_SUPERVISOR_AGENT_ID".into(), "stale-sup".into());
        inject_identity_env(&mut env, None, &root);
        assert!(
            !env.contains_key("KALLIP_SUPERVISOR_AGENT_ID"),
            "stale supervisor key must be removed on root, not left dangling: {env:?}"
        );
        assert_eq!(
            env.get("KALLIP_ROOT_AGENT_ID").map(String::as_str),
            Some("root-1")
        );
    }

    #[test]
    fn resolve_root_agent_returns_registry_root() {
        // The root is the tagma's single registered root, resolved
        // independently of any supervisor chain.
        let root = AgentId::from("root-1".to_owned());
        assert_eq!(resolve_root_agent(Some(&root)), root);
    }

    // -- compose_system_prompt (per-agent identity section) --

    /// Minimal config exercising only the fields `compose_system_prompt` reads.
    fn identity_config(created_by: Option<AgentId>, role: &str, description: &str) -> AgentConfig {
        AgentConfig {
            created_by,
            role: role.into(),
            description: description.into(),
            permissions_class: PermissionClass::Normal,
            // Synthetic base so this test exercises composition mechanics, not
            // the (separately guarded) content of DEFAULT_SYSTEM_PROMPT.
            system_prompt: "TEST BASE BODY".into(),
            ..AgentConfig::default()
        }
    }

    #[test]
    fn compose_system_prompt_root_has_no_unsubstituted_placeholder() {
        // The `.replace()` chain must consume every `{placeholder}`. A typo'd
        // name would leave a literal `{...}` in the production prompt; the
        // compiler can't catch it, so this generic check does.
        let cfg = identity_config(None, "root", "");
        let id = AgentId::from("root-1".to_owned());
        let prompt = compose_system_prompt(&cfg, id.clone(), id.clone());
        // The id value must actually render — guards against a `.replace` that
        // silently substitutes an empty/wrong value without leaving braces.
        assert!(prompt.contains("root-1"), "own id must render: {prompt}");
        assert!(
            !prompt.contains('{') && !prompt.contains('}'),
            "unsubstituted placeholder in root prompt: {prompt}"
        );
    }

    #[test]
    fn compose_system_prompt_subagent_has_no_unsubstituted_placeholder() {
        // Distinct placeholder set from the root template — exercise both.
        let cfg = identity_config(
            Some(AgentId::from("sup-1".to_owned())),
            "researcher",
            "gathers sources",
        );
        let id = AgentId::from("sub-1".to_owned());
        let root = AgentId::from("root-1".to_owned());
        let prompt = compose_system_prompt(&cfg, id.clone(), root.clone());
        // Values must actually render (not just "no braces left") — guards
        // against a `.replace` silently substituting empty/wrong values.
        assert!(prompt.contains("sub-1"), "own id must render: {prompt}");
        assert!(prompt.contains("root-1"), "root id must render: {prompt}");
        assert!(
            !prompt.contains('{') && !prompt.contains('}'),
            "unsubstituted placeholder in subagent prompt: {prompt}"
        );
    }

    #[test]
    fn compose_system_prompt_user_text_with_braces_is_not_rescanned() {
        // User-controlled `role`/`description` are substituted LAST; a value
        // containing a `{...}` fragment must survive as a literal and NOT be
        // re-scanned by an earlier placeholder's pass. We exercise this by
        // embedding the `{permission_class}` token (substituted earlier to
        // `Normal`) in the user text — the literal must appear in the prompt,
        // proving the role/description slot was not re-scanned.
        let root_cfg = identity_config(None, "{permission_class}", "");
        let root_id = AgentId::from("root-1".to_owned());
        let root_prompt = compose_system_prompt(&root_cfg, root_id.clone(), root_id.clone());
        // The real permission class renders in its own line...
        assert!(
            root_prompt.contains("- permission class:"),
            "permission-class line must render: {root_prompt}"
        );
        // ...and the literal token from the user-controlled role survives
        // unsubstituted (it was inserted only after the `{permission_class}` pass).
        assert!(
            root_prompt.contains("{permission_class}"),
            "user-text brace fragment must survive as a literal: {root_prompt}"
        );

        // Same contract for the subagent `description` slot.
        let sub_cfg = identity_config(
            Some(AgentId::from("sup-1".to_owned())),
            "researcher",
            "desc {permission_class} end",
        );
        let sub_prompt =
            compose_system_prompt(&sub_cfg, AgentId::from("sub-1".to_owned()), root_id.clone());
        assert!(
            sub_prompt.contains("desc {permission_class} end"),
            "user-text brace fragment must survive as a literal: {sub_prompt}"
        );
    }

    #[test]
    fn compose_system_prompt_static_tail_identical_across_variants() {
        // The static-shared tail (base + meta-skill) is the byte-identical,
        // cache-friendly suffix across every agent. Verify both variants end
        // with exactly that tail built from the same config base.
        let root_cfg = identity_config(None, "root", "");
        let sub_cfg = identity_config(Some(AgentId::from("sup-1".to_owned())), "researcher", "x");
        let root_id = AgentId::from("root-1".to_owned());
        let root_prompt = compose_system_prompt(&root_cfg, root_id.clone(), root_id.clone());
        let sub_prompt =
            compose_system_prompt(&sub_cfg, AgentId::from("sub-1".to_owned()), root_id.clone());
        let tail = format!("{}\n\n{}", root_cfg.system_prompt, meta_skill_content());
        assert!(
            root_prompt.ends_with(&tail),
            "root prompt must end with the shared static tail"
        );
        assert!(
            sub_prompt.ends_with(&tail),
            "subagent prompt must end with the shared static tail"
        );
    }

    // -- resolve_granted_class (the §2.3 reference-monitor decision, extracted) --

    #[test]
    fn granted_defaults_to_tier_ceiling_when_unrequested() {
        // No explicit request -> historical behavior: grant the ceiling.
        assert_eq!(
            resolve_granted_class(PermissionClass::Normal, PermissionClass::Normal, None).unwrap(),
            PermissionClass::Normal
        );
        assert_eq!(
            resolve_granted_class(PermissionClass::Guest, PermissionClass::Guest, None).unwrap(),
            PermissionClass::Guest
        );
    }

    #[test]
    fn granted_accepts_explicit_downgrade() {
        // A Normal-ceiling, Normal supervisor may actively grant Guest.
        assert_eq!(
            resolve_granted_class(
                PermissionClass::Normal,
                PermissionClass::Normal,
                Some(PermissionClass::Guest)
            )
            .unwrap(),
            PermissionClass::Guest
        );
        // Asking for exactly the ceiling is fine too.
        assert_eq!(
            resolve_granted_class(
                PermissionClass::Normal,
                PermissionClass::Normal,
                Some(PermissionClass::Normal)
            )
            .unwrap(),
            PermissionClass::Normal
        );
    }

    #[test]
    fn granted_rejects_request_above_tier_ceiling() {
        // depth-2 tier (ceiling Guest) cannot be bumped to Normal, even though the
        // supervisor is Normal.
        let err = resolve_granted_class(
            PermissionClass::Guest,
            PermissionClass::Normal,
            Some(PermissionClass::Normal),
        )
        .unwrap_err();
        assert!(err.to_string().contains("tier ceiling"), "{}", err);
    }

    #[test]
    fn granted_rejects_request_above_downgraded_supervisor() {
        // M1: a supervisor downgraded to Guest can no longer grant a child at its
        // tier's default Normal ceiling — the child's granted (Normal, the ceiling)
        // exceeds the supervisor's granted (Guest). Fail-closed: correct escalation
        // prevention, newly reachable once downgrade exists.
        let err = resolve_granted_class(
            PermissionClass::Normal,
            PermissionClass::Guest,
            None, // child asks for the default ceiling, which is now too high
        )
        .unwrap_err();
        assert!(err.to_string().contains("supervisor"), "{}", err);
    }

    // -- establish_workspace_lock (the shared carve: transfer + acquire + guards) --

    /// A Normal `AgentConfig` rooted at `ws`, reusing `make_entry`'s template so
    /// every field is populated.
    fn normal_config(ws: &std::path::Path) -> AgentConfig {
        let mut config = make_entry(None, String::new()).identity.config;
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
    async fn establish_workspace_lock_normal_root_acquires() {
        let state = make_state();
        let root = AgentId::from("root".to_owned());
        let ws = ws_dir("root");
        let cfg = normal_config(&ws);
        let established = establish_workspace_lock(&state, &root, &cfg, &[])
            .expect("Normal root acquires its workspace");
        // Lock is held while the guard lives and releases on drop.
        assert_eq!(state.lock_manager.holder(&ws).unwrap(), Some(root.clone()));
        drop(established);
        assert!(state.lock_manager.holder(&ws).unwrap().is_none());
    }

    #[tokio::test]
    async fn establish_workspace_lock_nested_child_no_longer_conflicts() {
        // The original bug: a Normal root holding /proj made any Normal nested
        // child's workspace acquire 409. With the chain, the child acquires.
        let state = make_state();
        let root = AgentId::from("root".to_owned());
        let root_ws = ws_dir("proj");
        let child_ws = root_ws.join("sub");
        std::fs::create_dir_all(&child_ws).unwrap();

        // Root holds /proj for the duration of the child acquire.
        let root_established =
            establish_workspace_lock(&state, &root, &normal_config(&root_ws), &[])
                .expect("root acquires");

        // Child's chain contains root → delegation, not conflict.
        let mut child_cfg = normal_config(&child_ws);
        child_cfg.created_by = Some(root.clone());
        let child = AgentId::from("child".to_owned());
        let child_established =
            establish_workspace_lock(&state, &child, &child_cfg, std::slice::from_ref(&root))
                .expect("nested child acquires via delegation chain");
        // Carve-out: the child's region appears read-only in the root's view.
        let ro = state.lock_manager.readonly_paths(&root).unwrap();
        assert_eq!(ro, vec![std::fs::canonicalize(&child_ws).unwrap()]);
        drop(child_established);
        drop(root_established);
    }

    #[tokio::test]
    async fn establish_workspace_lock_peer_without_chain_conflicts() {
        // Same topology, but the acquirer is NOT a delegation descendant
        // (empty chain) → Busy, the pre-fix behavior.
        let state = make_state();
        let root = AgentId::from("root".to_owned());
        let root_ws = ws_dir("proj2");
        let nested = root_ws.join("sub");
        std::fs::create_dir_all(&nested).unwrap();

        let _root_established =
            establish_workspace_lock(&state, &root, &normal_config(&root_ws), &[])
                .expect("root acquires");

        let peer = AgentId::from("peer".to_owned());
        let err = establish_workspace_lock(&state, &peer, &normal_config(&nested), &[])
            .err()
            .expect("peer without chain must conflict");
        assert!(matches!(err, EstablishLockFailure::Busy { .. }));
    }

    #[tokio::test]
    async fn establish_workspace_lock_guest_acquires_nothing() {
        let state = make_state();
        let id = AgentId::from("guest".to_owned());
        let ws = ws_dir("guest");
        let mut cfg = normal_config(&ws);
        cfg.permissions_class = PermissionClass::Guest;
        let established = establish_workspace_lock(&state, &id, &cfg, &[])
            .expect("guest establishes (acquires nothing)");
        assert!(established.workspace.is_none());
        assert!(state.lock_manager.holder(&ws).unwrap().is_none());
        drop(established);
    }

    #[tokio::test]
    async fn establish_workspace_lock_full_handoff_transfers_and_rolls_back() {
        // The drop-order invariant: on an unwind (drop without disarm) the
        // reverse transfer runs while writer==child, BEFORE the workspace guard's
        // release_all(child). A FullHandoff child must end up returning the lock
        // to the supervisor.
        let state = make_state();
        let supervisor = AgentId::from("sup".to_owned());
        let child = AgentId::from("child".to_owned());
        let ws = ws_dir("handoff");

        // Supervisor holds its workspace (the precondition for a real spawn:
        // validate guarantees a Live Normal supervisor owns its lock).
        let _sup_lock = state
            .lock_manager
            .acquire(&supervisor, &ws, &[])
            .expect("supervisor acquires");
        assert_eq!(
            state.lock_manager.holder(&ws).unwrap(),
            Some(supervisor.clone())
        );

        let mut cfg = normal_config(&ws);
        cfg.delegation_mode = DelegationMode::FullHandoff;
        cfg.created_by = Some(supervisor.clone());

        let established = establish_workspace_lock(&state, &child, &cfg, &[])
            .expect("full-handoff child establishes");
        // The forward transfer reassigned writer to the child.
        assert_eq!(state.lock_manager.holder(&ws).unwrap(), Some(child.clone()));
        // Simulate a spawn-failure unwind: drop WITHOUT disarm. EstablishedLock's
        // manual Drop runs the reverse transfer before the workspace guard releases,
        // so it runs while writer==child and the supervisor regains the lock.
        drop(established);
        assert_eq!(
            state.lock_manager.holder(&ws).unwrap(),
            Some(supervisor.clone())
        );
    }

    #[tokio::test]
    async fn establish_workspace_lock_rejects_handoff_without_supervisor() {
        // A corrupt meta.json with delegation_mode=full_handoff and no created_by
        // must fail gracefully (replaces the prior `.expect` that crashed restore).
        let state = make_state();
        let id = AgentId::from("orphan".to_owned());
        let ws = ws_dir("orphan");
        let mut cfg = normal_config(&ws);
        cfg.delegation_mode = DelegationMode::FullHandoff;
        cfg.created_by = None;
        let err = establish_workspace_lock(&state, &id, &cfg, &[])
            .err()
            .expect("full-handoff without supervisor is rejected");
        assert!(matches!(
            err,
            EstablishLockFailure::HandoffWithoutSupervisor
        ));
    }

    #[test]
    fn establish_lock_api_error_maps_status_codes() {
        // The HTTP-status selection lives here (not in the helper), so pin it.
        use kallip_common::protocol::ApiError;
        let busy = EstablishLockFailure::Busy {
            holder: AgentId::from("x".to_owned()),
            conflict: PathBuf::from("/p"),
        };
        assert_eq!(super::establish_lock_api_error(busy).status, 409);
        let other = EstablishLockFailure::AcquireFailed(std::io::Error::other("boom"));
        assert_eq!(
            super::establish_lock_api_error(other).status,
            ApiError::bad_request("").status
        );
    }

    // -- FullHandoff exclusivity (validate_subagent_request) --

    #[tokio::test]
    async fn validate_rejects_full_handoff_when_supervisor_has_a_child() {
        // Direction 1: a full-handoff child requires the supervisor to have NO
        // other children. Seed a supervisor with an existing child slot and the
        // request must be refused before any workspace/depth check runs.
        let state = make_state();
        let sup = AgentId::from("sup".to_owned());
        let sibling = AgentId::from("sibling".to_owned());
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &sup);
            reg.get_mut(&sup)
                .expect("supervisor registered")
                .subagent_ids_mut()
                .push(sibling.clone());
        }
        let reg = state.registry.read().await;
        let ws = PathBuf::from("/tmp");
        let err = super::validate_subagent_request(
            &reg,
            &Identity::Operator,
            &sup,
            &ws,
            None,
            DelegationMode::FullHandoff,
        )
        .expect_err("full-handoff with an existing child must be refused");
        assert_eq!(err.status, 409);
        // Direction-specific substring: this arm is the "supervisor has other
        // children" rejection. Asserting merely `.contains("full-handoff")`
        // would also pass against the other direction's message, hiding a swap.
        assert!(
            err.message.contains("no other subagents"),
            "should cite the no-other-subagents rule, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn validate_rejects_new_child_when_full_handoff_child_exists() {
        // Direction 2: once a full-handoff child lives, no other child (of any
        // mode) may be spawned under the same supervisor.
        let state = make_state();
        let sup = AgentId::from("sup".to_owned());
        let fh_child = AgentId::from("fh".to_owned());
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &sup);
            let mut entry = make_entry(Some(sup.clone()), format!("agent-{fh_child}"));
            entry.identity.config.delegation_mode = DelegationMode::FullHandoff;
            reg.register(fh_child.clone(), crate::state::RegistryEntry::Live(entry));
            reg.get_mut(&sup)
                .expect("supervisor registered")
                .subagent_ids_mut()
                .push(fh_child.clone());
        }
        let reg = state.registry.read().await;
        let ws = PathBuf::from("/tmp");
        let err = super::validate_subagent_request(
            &reg,
            &Identity::Operator,
            &sup,
            &ws,
            None,
            DelegationMode::CarveOut,
        )
        .expect_err("a new child while a full-handoff child lives must be refused");
        assert_eq!(err.status, 409);
        // Direction-specific substring: this arm is the "supervisor already has
        // a full-handoff child" rejection. Asserting merely
        // `.contains("full-handoff")` would also pass against the other
        // direction's message, hiding a swap.
        assert!(
            err.message.contains("already has"),
            "should cite the existing full-handoff child, got: {}",
            err.message
        );
    }

    /// On removal, a FullHandoff child's workspace lock is transferred back to
    /// the supervisor (the happy path; the drop-without-disarm unwind path is
    /// covered by `establish_workspace_lock_full_handoff_transfers_and_rolls_back`).
    #[tokio::test]
    async fn remove_agent_returns_full_handoff_lock_to_supervisor() {
        let state = make_state();
        let sup = AgentId::from("sup".to_owned());
        let child = AgentId::from("child".to_owned());
        let ws = ws_dir("fh-remove");

        // Register a Live FullHandoff child under `sup`. Set its workspace_root
        // to `ws`: remove_agent's transfer-back targets config.workspace_root.
        {
            let mut reg = state.registry.write().await;
            let mut entry = make_entry(Some(sup.clone()), format!("agent-{child}"));
            entry.identity.config.delegation_mode = DelegationMode::FullHandoff;
            entry.identity.config.permissions_class = PermissionClass::Normal;
            entry.identity.config.workspace_root = ws.clone();
            reg.register(child.clone(), crate::state::RegistryEntry::Live(entry));
        }
        // Simulate the spawn carve: sup held ws, then transferred it to the child.
        state.lock_manager.acquire(&sup, &ws, &[]).unwrap();
        state.lock_manager.transfer(&sup, &child, &ws).unwrap();
        assert_eq!(state.lock_manager.holder(&ws).unwrap(), Some(child.clone()));

        remove_agent(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            Path(child.clone()),
        )
        .await
        .expect("remove succeeds");

        // The transfer-back branch reassigned the workspace lock to the supervisor.
        assert_eq!(state.lock_manager.holder(&ws).unwrap(), Some(sup));
    }

    // -- Faulted agent manageability (the headline bug fix) --

    /// `subagent list` includes faulted agents, marking state and surfacing reason.
    #[tokio::test]
    async fn list_agents_includes_faulted() {
        let state = make_state();
        let live = AgentId::random();
        let faulted = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            // Two roots is intentionally invalid for a live tagma; this test
            // exercises list filtering, not the singleton invariant, so it uses
            // the raw `add_root`/`add_faulted_root` helpers (see their docs).
            add_root(&mut reg, &live);
            add_faulted_root(&mut reg, &faulted, "restore failed: boom");
        }
        let resp = list_agents(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Query(ListAgentsQuery { created_by: None }),
        )
        .await;
        let agents = resp.0.agents;
        let f = agents
            .iter()
            .find(|a| a.id == faulted)
            .expect("faulted agent listed");
        assert_eq!(f.state, super::AgentState::Faulted);
        assert_eq!(f.faulted_reason.as_deref(), Some("restore failed: boom"));
        assert!(agents.iter().any(|a| a.id == live));
    }

    /// Removing a faulted subagent succeeds (204) -- the bug was 403/404 because
    /// the agent was never registered. The fast path skips shutdown (no task);
    /// the archive is a best-effort no-op when the dir is absent.
    #[tokio::test]
    async fn remove_faulted_agent_succeeds() {
        let state = make_state();
        let root = AgentId::random();
        let faulted = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &root);
            add_faulted_sub(&mut reg, &faulted, &root, "broken");
        }
        let status = remove_agent(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            Path(faulted.clone()),
        )
        .await
        .expect("remove succeeds");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        // Entry is gone from the registry.
        assert!(!state.registry.read().await.contains_key(&faulted));
    }

    /// A *live* tagma root is non-removable (clients target subagents).
    #[tokio::test]
    async fn remove_live_root_returns_conflict() {
        let state = make_state();
        let root = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &root);
        }
        let err = remove_agent(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(root.clone()),
        )
        .await
        .expect_err("live root is non-removable");
        assert_eq!(err.status, 409);
    }

    /// A *faulted* root IS removable so an operator can recover from a restore
    /// failure through the API (the next tagma restart re-creates the root).
    /// `add_faulted_root` bypasses `register_root` to seed this single-root
    /// faulted state (test-only; see `add_root`'s doc).
    #[tokio::test]
    async fn remove_faulted_root_succeeds() {
        let state = make_state();
        let root = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_faulted_root(&mut reg, &root, "restore failed: boom");
        }
        let status = remove_agent(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            Path(root.clone()),
        )
        .await
        .expect("faulted root is removable");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        assert!(!state.registry.read().await.contains_key(&root));
    }

    /// Interrupting a faulted agent returns 409 (nothing to interrupt) instead
    /// of touching runtime fields that don't exist on a faulted entry.
    #[tokio::test]
    async fn interrupt_faulted_returns_conflict() {
        let state = make_state();
        let faulted = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_faulted_root(&mut reg, &faulted, "broken");
        }
        let err = interrupt_agent(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(faulted),
        )
        .await
        .expect_err("interrupt faulted is a conflict");
        assert_eq!(err.status, 409);
    }
}
