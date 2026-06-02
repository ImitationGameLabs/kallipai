use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use anyhow::Context as _;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_common::types::{AgentId, SseEvent};
use just_agent_runtime::config::{AgentConfig, PermissionProfile};
use just_agent_runtime::context::{AgenticContext, ContextStore, ContextSummarizer};
use just_agent_runtime::deferred::DeferredActionStore;
use just_agent_runtime::persistence;
use just_agent_runtime::policy::{AgentPolicy, AuthorizedToolExecutor};
use just_agent_runtime::provider::client_from_env;
use just_agent_runtime::session::{self, AgentContext};
use just_agent_runtime::tools::{build_tool_dispatch, ensure_meta_skill, load_skill};
use just_llm_client::types::chat::ChatMessage;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

use just_agent_common::types::{CreateAgentRequest, CreateAgentResponse};

use super::ListAgentsResponse;
use crate::bridge::bridge_task;
use crate::state::{Agent, AgentEntry, AgentState, AgentSummary, SharedState};

pub(crate) struct SpawnArgs {
    pub store: Arc<tokio::sync::Mutex<ContextStore>>,
    pub deferred: Arc<tokio::sync::Mutex<DeferredActionStore>>,
    pub session_dir: PathBuf,
    pub config: AgentConfig,
    pub initial_prompt: Option<String>,
    pub shutdown_cancel: CancellationToken,
    pub events_tx: broadcast::Sender<SseEvent>,
    pub auth_token: String,
    pub env: HashMap<String, String>,
    pub shared_state: SharedState,
}

/// Reconstruct runtime resources shared by create and restore.
pub(crate) async fn spawn_agent(args: SpawnArgs) -> anyhow::Result<Agent> {
    let cancel = args.shutdown_cancel.child_token();

    let client = {
        let meta = ensure_meta_skill()?;
        let mut sp = args.config.system_prompt.clone();
        sp.push_str("\n\n");
        sp.push_str(&meta);
        client_from_env(&sp)?
    };

    let dispatch = build_tool_dispatch(args.store.clone(), args.env.clone()).await?;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);
    let (prompt_tx, prompt_rx) = tokio::sync::mpsc::channel(16);

    let executor = AuthorizedToolExecutor::new(
        dispatch,
        AgentPolicy::new(args.config.workspace_root.clone()),
        args.deferred.clone(),
    );
    let tool_defs = executor.tool_definitions();
    args.store.lock().await.set_tool_definitions(tool_defs);
    let pinned_budget =
        (args.config.effective_budget() as f64 * args.config.pinned_budget_ratio) as usize;
    args.store.lock().await.set_pinned_budget(pinned_budget);
    let summarizer = ContextSummarizer::new(args.config.summary_max_tokens);

    let ctx = AgentContext {
        client,
        store: args.store.clone(),
        deferred: args.deferred.clone(),
        executor,
        summarizer,
        config: args.config.clone(),
        session_dir: Some(args.session_dir.clone()),
        cancel: cancel.clone(),
    };

    let agent_handle = tokio::spawn(session::agent_task(
        ctx,
        args.initial_prompt,
        prompt_rx,
        agent_tx,
    ));
    let state = Arc::new(AtomicU8::new(AgentState::IDLE));
    // TODO: extract agent_id from SpawnArgs as a required field so the type system
    // enforces this invariant instead of relying on callers to set config.agent_id.
    let agent_id = args
        .config
        .agent_id
        .clone()
        .expect("agent_id must be set before spawn");
    let bridge_handle = tokio::spawn(bridge_task(
        agent_id,
        agent_rx,
        args.events_tx.clone(),
        args.shutdown_cancel.clone(),
        state.clone(),
        args.shared_state.clone(),
    ));

    Ok(Agent {
        prompt_tx,
        events_tx: args.events_tx,
        deferred: args.deferred,
        config: args.config,
        agent_handle,
        bridge_handle,
        store: args.store,
        session_dir: Some(args.session_dir),
        cancel,
        state,
        auth_token: args.auth_token,
        env: args.env,
    })
}

pub async fn create_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Root agents require operator privilege.
    if req.created_by.is_none() {
        crate::auth::require_operator(&auth.0)?;
    }

    let id = AgentId::random();
    let auth_token = uuid::Uuid::new_v4().to_string();

    let mut config = {
        let ws = req.workspace_root.map(std::path::PathBuf::from);
        AgentConfig::load(req.prompt, req.skills, ws)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    };
    config.agent_id = Some(id.clone());
    let mut env = HashMap::new();
    env.insert("JUST_AGENT_ID".into(), id.to_string());
    env.insert("JUST_AGENT_AUTH_TOKEN".into(), auth_token.clone());

    // Subagent: validate supervisor and delegation constraints.
    if let Some(ref supervisor_id) = req.created_by {
        let registry = state.registry.read().await;
        let supervisor = registry.require_supervisor(&auth.0, supervisor_id)?;

        let supervisor_perms = &supervisor.agent.config.permissions;
        if supervisor_perms.max_depth == 0 {
            return Err((
                StatusCode::FORBIDDEN,
                "supervisor has no remaining delegation depth".into(),
            ));
        }
        let subagent_ws = config.workspace_root.canonicalize().map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid workspace_root: {e}"),
            )
        })?;
        if !subagent_ws.starts_with(&supervisor_perms.workspace_root) {
            return Err((
                StatusCode::FORBIDDEN,
                "workspace_root must be within supervisor's workspace".into(),
            ));
        }

        config.created_by = Some(supervisor_id.clone());
        config.permissions = PermissionProfile::subagent(subagent_ws, supervisor_perms.max_depth);
    }

    let store = Arc::new(tokio::sync::Mutex::new(ContextStore::new()));
    let deferred = Arc::new(tokio::sync::Mutex::new(DeferredActionStore::new()));

    for skill_name in &config.skills {
        let content =
            load_skill(skill_name).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        store
            .lock()
            .await
            .pin(
                &format!("skill:{skill_name}"),
                ChatMessage::user(format!("[skill: {skill_name}]\n{content}")),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        info!(skill = skill_name, "loaded skill");
    }

    let session_dir =
        persistence::create_session(&id, &config.workspace_root, config.created_by.as_ref())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let prompt = config.prompt.take();
    let log_ws = config.workspace_root.display().to_string();
    let log_depth = config.permissions.max_depth;
    let (events_tx, _) = broadcast::channel(256);
    let agent = spawn_agent(SpawnArgs {
        store,
        deferred,
        session_dir,
        config,
        initial_prompt: prompt,
        shutdown_cancel: state.shutdown.clone(),
        events_tx,
        auth_token: auth_token.clone(),
        env,
        shared_state: state.clone(),
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    {
        let mut registry = state.registry.write().await;
        // Re-verify supervisor was not deleted during agent spawn.
        if let Some(ref supervisor_id) = req.created_by
            && !registry.contains_key(supervisor_id)
        {
            // Supervisor gone — clean up the orphaned subagent.
            agent.agent_handle.abort();
            agent.bridge_handle.abort();
            if let Some(ref dir) = agent.session_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "supervisor agent was deleted during creation".into(),
            ));
        }
        registry.register(
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
) -> Result<StatusCode, (StatusCode, String)> {
    let entry = {
        let mut registry = state.registry.write().await;
        registry.require_superior(&auth.0, &id)?;
        let Some(entry) = registry.get(&id) else {
            return Ok(StatusCode::NOT_FOUND);
        };
        // Agent must be idle and have no subagents.
        if entry.agent.get_state() != AgentState::Idle {
            return Err((
                StatusCode::CONFLICT,
                "agent is busy, interrupt it first".into(),
            ));
        }
        if !entry.subagent_ids.is_empty() {
            return Err((
                StatusCode::CONFLICT,
                "agent has active subagents, delete or interrupt them first".into(),
            ));
        }
        // Unregister under the same write lock — should always succeed since
        // `get` above confirmed the agent exists. Defensive fallback in case
        // the invariant is violated by a future refactor.
        match registry.unregister(&id) {
            Some(e) => e,
            None => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "agent vanished during deletion".into(),
                ));
            }
        }
    };

    // Signal graceful cancellation.
    entry.agent.cancel.cancel();

    // Wait briefly for the agent to persist and exit.
    // Since JoinHandle is not Clone, we sleep and then abort.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    entry.agent.agent_handle.abort();
    entry.agent.bridge_handle.abort();

    if let Err(e) = persistence::cleanup_session(&id) {
        info!(id = %id, "session cleanup failed: {e:#}");
    }
    info!(id = %id, "deleted agent");
    Ok(StatusCode::NO_CONTENT)
}

/// Interrupt the current agent operation without deleting it.
pub async fn interrupt_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, (StatusCode, String)> {
    let registry = state.registry.read().await;
    registry.require_superior(&auth.0, &id)?;
    let Some(entry) = registry.get(&id) else {
        return Ok(StatusCode::NOT_FOUND);
    };
    entry.agent.cancel.cancel();
    Ok(StatusCode::ACCEPTED)
}

/// Validate supervisor chain and compute delegation depth for a subagent.
/// Returns the computed `max_depth` for the subagent's `PermissionProfile`.
fn validate_subagent_depth(
    workspace_root: &std::path::Path,
    supervisor_id: &AgentId,
) -> anyhow::Result<u8> {
    let supervisor_meta =
        persistence::read_meta(supervisor_id).context("supervisor meta missing")?;

    if !workspace_root.starts_with(&supervisor_meta.workspace_root) {
        anyhow::bail!("workspace outside supervisor boundary");
    }

    let mut levels: u8 = 1;
    let mut visited = HashSet::new();
    visited.insert(supervisor_id.clone());
    let mut current_meta = supervisor_meta;
    while let Some(ref parent_id) = current_meta.created_by {
        if !visited.insert(parent_id.clone()) {
            anyhow::bail!("circular supervisor chain detected");
        }
        levels = levels.saturating_add(1);
        current_meta = persistence::read_meta(parent_id).context("incomplete supervisor chain")?;
    }

    Ok(just_agent_runtime::config::DEFAULT_MAX_DEPTH.saturating_sub(levels))
}

/// Restore a single persisted session to a running agent.
async fn restore_one(
    p: persistence::PendingRestore,
    shutdown: CancellationToken,
    shared_state: SharedState,
) -> anyhow::Result<(AgentId, String, Agent)> {
    let sess = persistence::restore_session(&p.agent_id, &p.session_dir)?;

    let mut config = AgentConfig::load(None, vec![], Some(p.workspace_root.clone()))?;
    config.agent_id = Some(p.agent_id.clone());
    config.created_by = p.created_by.clone();

    if let Some(ref supervisor_id) = p.created_by {
        config.permissions.max_depth = validate_subagent_depth(&p.workspace_root, supervisor_id)?;
    }

    let store = Arc::new(tokio::sync::Mutex::new(sess.store));
    let deferred = Arc::new(tokio::sync::Mutex::new(sess.deferred));
    let (events_tx, _) = broadcast::channel(256);

    let auth_token = uuid::Uuid::new_v4().to_string();
    let mut env = HashMap::new();
    env.insert("JUST_AGENT_ID".into(), p.agent_id.to_string());
    env.insert("JUST_AGENT_AUTH_TOKEN".into(), auth_token.clone());

    let agent = spawn_agent(SpawnArgs {
        store,
        deferred,
        session_dir: sess.session_dir,
        config,
        initial_prompt: None,
        shutdown_cancel: shutdown,
        events_tx,
        auth_token: auth_token.clone(),
        env,
        shared_state,
    })
    .await?;

    Ok((sess.agent_id, auth_token, agent))
}

/// Restore persisted sessions sequentially, then rebuild `subagent_ids`
/// from `created_by` relationships.
pub async fn restore_sessions(state: &SharedState) {
    let pending = persistence::scan_sessions();
    if pending.is_empty() {
        return;
    }

    // Use "agents" in logs to avoid confusing users with the internal "session" concept.
    info!(count = pending.len(), "restoring agents");

    for p in pending {
        let agent_id = p.agent_id.clone();
        match restore_one(p, state.shutdown.clone(), state.clone()).await {
            Ok((id, auth_token, agent)) => {
                let mut registry = state.registry.write().await;
                registry.register(
                    id,
                    auth_token,
                    AgentEntry {
                        agent,
                        subagent_ids: vec![],
                    },
                );
                info!(id = %agent_id, "restored agent");
            }
            Err(e) => {
                tracing::error!(id = %agent_id, "restore failed: {e:#}");
            }
        }
    }

    state.registry.write().await.rebuild_subagent_ids();
}
