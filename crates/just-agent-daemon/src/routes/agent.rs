use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_common::agentid::AgentId;
use just_agent_common::policy::ToolPolicy;
use just_agent_common::protocol::ApiError;
use just_agent_common::protocol::SseEvent;
use just_agent_runtime::agent_task::{self, AgentContext};
use just_agent_runtime::approval::ApprovalStore;
use just_agent_runtime::config::{AgentConfig, PermissionProfile, tool_policy_from_env};
use just_agent_runtime::context::{AgenticContext, ContextStore, ContextSummarizer};
use just_agent_runtime::history::HistoryWriter;
use just_agent_runtime::persistence;
use just_agent_runtime::policy::{AgentPolicy, AuthorizedToolExecutor};
use just_agent_runtime::tools::{build_tool_dispatch, load_skill, meta_skill_content};
use just_llm_client::types::chat::ChatMessage;
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use just_agent_common::protocol::{
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
    pub prompt_queue_size: usize,
    /// The resolved model tier (selected by the caller). The active profile is
    /// `tier.profiles[0]`; the rest form the within-tier failover chain. Owned so the
    /// runtime can carry the chain without re-touching the registry.
    pub tier: just_agent_runtime::profile::Tier,
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
        env.insert("JUST_AGENT_ID".into(), agent_id.to_string());
        env.insert("JUST_AGENT_AUTH_TOKEN".into(), auth_token.to_owned());
        env
    }
}

/// Reconstruct runtime resources shared by create and restore.
pub(crate) async fn spawn_agent(mut args: SpawnArgs) -> anyhow::Result<Agent> {
    let cancel = args.shutdown_cancel.child_token();
    let notify = Arc::new(Notify::new());
    // Round-scoped interrupt slot: `Some` only while a round runs. Shared with the agent
    // task so `interrupt_agent` can cancel the current round without terminating the task.
    let round_cancel: Arc<std::sync::Mutex<Option<just_agent_runtime::agent_task::RoundToken>>> =
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
        // implicit env profile derives it from JUST_AGENT_CONTEXT_WINDOW_TOKENS), then build the
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
    // the stateless backend's terminal-state observer). `try_send` drops silently
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

    let dispatch = build_tool_dispatch(args.store.clone(), args.env.clone(), notice_sink).await?;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);

    let executor = AuthorizedToolExecutor::new(
        dispatch,
        AgentPolicy::new(args.tool_policy.clone()),
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
        failover: just_agent_runtime::FailoverState::new(
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
    })
}

/// Abort agent/bridge handles and remove agent dir (best-effort).
/// Used when a spawned agent cannot be registered.
pub(crate) fn abort_agent(agent: &crate::state::Agent) {
    agent.agent_handle.abort();
    agent.bridge_handle.abort();
    if let Some(ref dir) = agent.agent_dir
        && let Err(e) = std::fs::remove_dir_all(dir)
    {
        tracing::warn!(path = %dir.display(), "failed to clean up agent dir: {e:#}");
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
    // (`JUST_AGENT_AUTH_TOKEN`); only its SHA-256 is indexed for auth lookup.
    let token = MintedToken::generate(TokenKind::Agent);

    let mut config = {
        let ws = req.workspace_root.map(std::path::PathBuf::from);
        AgentConfig::load(req.prompt, req.skills, ws)
            .map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    config.agent_id = Some(id.clone());
    if let Some(rounds) = req.max_tool_rounds {
        match rounds {
            just_agent_common::protocol::MaxToolRounds::Unlimited => {
                config.set_max_tool_rounds(usize::MAX);
            }
            just_agent_common::protocol::MaxToolRounds::Limited(n) => {
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
    let env = SpawnArgs::default_env(&id, token.secret());

    // Subagent: validate supervisor and delegation constraints, or use default policy.
    // Pre-reserve the subagent slot under write lock to eliminate TOCTOU.
    let tool_policy = if let Some(ref supervisor_id) = req.created_by {
        let mut registry = state.registry.write().await;
        let (permissions, policy) = validate_subagent_request(
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
        policy
    } else {
        tool_policy_from_env()
    };
    let tool_policy = Arc::new(std::sync::RwLock::new(tool_policy));

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

    let prompt = config.prompt.take();
    let log_ws = config.workspace_root.display().to_string();
    let log_depth = config.permissions.max_depth;
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
        prompt_queue_size: state.prompt_queue_size,
        prompt_channel: None,
        tier,
    })
    .await
    {
        Ok(a) => a,
        Err(e) => {
            // Rollback pre-reserved subagent slot.
            if let Some(ref supervisor_id) = req.created_by {
                let mut registry = state.registry.write().await;
                if let Some(supervisor) = registry.get_mut(supervisor_id) {
                    supervisor.subagent_ids.retain(|sid| sid != &id);
                }
            }
            // Clean up agent dir (created before spawn).
            if let Err(e) = std::fs::remove_dir_all(&agent_dir_clone) {
                tracing::warn!(path = %agent_dir_clone.display(), "failed to clean up agent dir: {e:#}");
            }
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
    info!(id = %id, supervisor = ?req.created_by, ws = %log_ws, depth = log_depth, "created agent");

    Ok((StatusCode::CREATED, Json(CreateAgentResponse { id })))
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
/// Returns `(PermissionProfile, ToolPolicy)` for the new subagent if valid.
///
/// Lock ordering: `registry` RwLock is held when calling this function.
/// Inside, `tool_policy.read()` acquires `std::sync::RwLock<ToolPolicy>`.
/// Never acquire these in reverse order.
fn validate_subagent_request(
    registry: &crate::state::AgentRegistry,
    identity: &crate::auth::Identity,
    supervisor_id: &AgentId,
    workspace_root: &std::path::Path,
) -> Result<(PermissionProfile, ToolPolicy), ApiError> {
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
    let tool_policy = supervisor
        .agent
        .tool_policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    Ok((permissions, tool_policy))
}

#[cfg(test)]
mod tests {
    use super::{MAX_ACTIVITY_CHARS, truncate_chars};

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
}
