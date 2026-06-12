use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_common::agentid::AgentId;
use just_agent_common::command::UserInput;
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
use just_agent_runtime::provider::client_from_env;
use just_agent_runtime::tools::{build_tool_dispatch, load_skill, meta_skill_content};
use just_llm_client::types::chat::ChatMessage;
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::info;

use just_agent_common::protocol::{CreateAgentRequest, CreateAgentResponse};

use super::ListAgentsResponse;
use crate::bridge::bridge_task;
use crate::state::{Agent, AgentEntry, AgentState, AgentSummary, SharedState};

/// Maximum time to wait for a single agent to persist before force-aborting on deletion.
const DELETE_AGENT_SHUTDOWN_TIMEOUT_SECS: u64 = 10;

pub(crate) struct SpawnArgs {
    pub agent_id: AgentId,
    pub store: Arc<tokio::sync::Mutex<ContextStore>>,
    pub approvals: Arc<tokio::sync::Mutex<ApprovalStore>>,
    pub agent_dir: PathBuf,
    pub config: AgentConfig,
    pub initial_prompt: Option<String>,
    pub shutdown_cancel: CancellationToken,
    pub events_tx: broadcast::Sender<SseEvent>,
    pub auth_token: String,
    pub env: HashMap<String, String>,
    pub shared_state: SharedState,
    pub tool_policy: Arc<std::sync::RwLock<ToolPolicy>>,
    pub prompt_queue_size: usize,
    /// Pre-created prompt channel for reactivation. When provided,
    /// `prompt_queue_size` is ignored and both ends are used as-is.
    /// The sender is already installed in the registry entry; spawn_agent
    /// only stores it in the Agent struct and passes the receiver to the
    /// agent task.
    pub prompt_channel: Option<(
        tokio::sync::mpsc::Sender<UserInput>,
        tokio::sync::mpsc::Receiver<UserInput>,
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
pub(crate) async fn spawn_agent(args: SpawnArgs) -> anyhow::Result<Agent> {
    let cancel = args.shutdown_cancel.child_token();
    let notify = Arc::new(Notify::new());

    let client = {
        let meta = meta_skill_content();
        let mut sp = args.config.system_prompt.clone();
        sp.push_str("\n\n");
        sp.push_str(meta);
        client_from_env(&sp)?
    };

    let dispatch = build_tool_dispatch(args.store.clone(), args.env.clone()).await?;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);
    let (prompt_tx, prompt_rx) = args
        .prompt_channel
        .unwrap_or_else(|| tokio::sync::mpsc::channel(args.prompt_queue_size));

    let executor = AuthorizedToolExecutor::new(
        dispatch,
        AgentPolicy::new(args.tool_policy.clone()),
        args.approvals.clone(),
    );
    let tool_defs = executor.tool_definitions();
    args.store.lock().await.set_tool_definitions(tool_defs);
    let pinned_budget =
        (args.config.effective_budget() as f64 * args.config.pinned_budget_ratio) as usize;
    args.store.lock().await.set_pinned_budget(pinned_budget);
    let summarizer = ContextSummarizer::new(args.config.summary_max_tokens);

    let token_budget = args.shared_state.token_budget.clone();

    let ctx = AgentContext {
        client,
        store: args.store.clone(),
        approvals: args.approvals.clone(),
        executor,
        summarizer,
        config: args.config.clone(),
        agent_dir: Some(args.agent_dir.clone()),
        history: Some(HistoryWriter::new(args.agent_dir.clone())),
        cancel: cancel.clone(),
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
        notify,
        state,
        auth_token: args.auth_token,
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
    let auth_token = uuid::Uuid::new_v4().to_string();

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
    let env = SpawnArgs::default_env(&id, &auth_token);

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

    let store = Arc::new(tokio::sync::Mutex::new(ContextStore::new()));
    let approvals = Arc::new(tokio::sync::Mutex::new(ApprovalStore::new()));

    // Create agent directory before loading skills so that agent-local
    // skills can be resolved from the agent dir.
    let agent_dir =
        persistence::create_agent_dir(&id, &config.workspace_root, config.created_by.as_ref())
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
        auth_token: auth_token.clone(),
        env,
        shared_state: state.clone(),
        tool_policy: tool_policy.clone(),
        prompt_queue_size: state.prompt_queue_size,
        prompt_channel: None,
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
                "agent limit reached ({}/{max}), delete agents to create new ones",
                registry.len(),
                max = state.max_agents
            )));
        }
        // Re-verify supervisor was not deleted during agent spawn.
        if let Some(ref supervisor_id) = req.created_by
            && !registry.contains_key(supervisor_id)
        {
            // Supervisor gone — the pre-reserved slot is already cleaned up
            // (unregistering the supervisor removes it from the map entirely).
            abort_agent(&agent);
            return Err(ApiError::internal(
                "supervisor agent was deleted during creation",
            ));
        }
        registry.register_no_subagent_push(
            id.clone(),
            auth_token,
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
pub async fn list_agents(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
) -> Json<ListAgentsResponse> {
    let registry = state.registry.read().await;
    let summaries: Vec<AgentSummary> = registry
        .iter()
        .map(|(id, entry)| AgentSummary {
            id: id.clone(),
            workspace_root: entry.agent.config.workspace_root.display().to_string(),
            state: entry.agent.get_state(),
            created_by: entry.agent.config.created_by.clone(),
        })
        .collect();
    Json(ListAgentsResponse { agents: summaries })
}

pub async fn delete_agent(
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
                "agent has active subagents, delete or interrupt them first",
            ));
        }
        // Unregister under the same write lock — should always succeed since
        // `get` above confirmed the agent exists. Defensive fallback in case
        // the invariant is violated by a future refactor.
        match registry.unregister(&id) {
            Some(e) => e,
            None => {
                return Err(ApiError::internal("agent vanished during deletion"));
            }
        }
    };

    // Signal graceful cancellation.
    entry.agent.cancel.cancel();

    // Wait for the agent to detect cancellation and persist.
    // Since JoinHandle is not Clone, we sleep and then abort.
    tokio::time::sleep(std::time::Duration::from_secs(
        DELETE_AGENT_SHUTDOWN_TIMEOUT_SECS,
    ))
    .await;
    entry.agent.agent_handle.abort();
    entry.agent.bridge_handle.abort();

    if let Err(e) = persistence::cleanup_agent_dir(&id) {
        info!(id = %id, "agent dir cleanup failed: {e:#}");
    }
    info!(id = %id, "deleted agent");
    Ok(StatusCode::NO_CONTENT)
}

/// Interrupt the current agent operation without deleting it.
pub async fn interrupt_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, ApiError> {
    let registry = state.registry.read().await;
    registry.require_superior(auth.identity(), &id)?;
    let Some(entry) = registry.get(&id) else {
        return Err(ApiError::not_found("agent not found"));
    };
    entry.agent.cancel.cancel();
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
