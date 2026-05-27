use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_core::config::{AgentConfig, PermissionProfile};
use just_agent_core::context::{AgenticContext, ContextStore, ContextSummarizer};
use just_agent_core::deferred::DeferredQueue;
use just_agent_core::persistence;
use just_agent_core::policy::{AgentPolicy, AuthorizedToolExecutor};
use just_agent_core::provider::client_from_env;
use just_agent_core::session::{self, AgentContext};
use just_agent_core::tools::{build_tool_dispatch, ensure_meta_skill, load_skill};
use just_agent_core::types::SseEvent;
use just_llm_client::types::chat::ChatMessage;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::{CreateAgentRequest, CreateAgentResponse, ListAgentsResponse};
use crate::bridge::bridge_task;
use crate::state::{Agent, AgentEntry, AgentState, AgentSummary, SharedState};

/// Reconstruct runtime resources shared by create and restore.
pub(crate) async fn spawn_agent(
    store: Arc<tokio::sync::Mutex<ContextStore>>,
    deferred: Arc<tokio::sync::Mutex<DeferredQueue>>,
    session_dir: std::path::PathBuf,
    config: AgentConfig,
    initial_prompt: Option<String>,
    shutdown_cancel: CancellationToken,
    events_tx: broadcast::Sender<SseEvent>,
    auth_token: String,
    env: HashMap<String, String>,
) -> anyhow::Result<Agent> {
    let cancel = shutdown_cancel.child_token();

    let client = {
        let meta = ensure_meta_skill()?;
        let mut sp = config.system_prompt.clone();
        sp.push_str("\n\n");
        sp.push_str(&meta);
        client_from_env(&sp)?
    };

    let dispatch = build_tool_dispatch(store.clone(), env.clone()).await?;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);
    let (prompt_tx, prompt_rx) = tokio::sync::mpsc::channel(16);

    let executor = AuthorizedToolExecutor::new(
        dispatch,
        AgentPolicy::new(config.workspace_root.clone()),
        deferred.clone(),
    );
    let tool_defs = executor.tool_definitions();
    store.lock().await.set_tool_definitions(tool_defs);
    let pinned_budget = (config.effective_budget() as f64 * config.pinned_budget_ratio) as usize;
    store.lock().await.set_pinned_budget(pinned_budget);
    let summarizer = ContextSummarizer::new(config.summary_max_tokens);

    let ctx = AgentContext {
        client,
        store: store.clone(),
        deferred: deferred.clone(),
        executor,
        summarizer,
        config: config.clone(),
        session_dir: Some(session_dir.clone()),
        cancel: cancel.clone(),
    };

    let agent_handle = tokio::spawn(session::agent_task(
        ctx,
        initial_prompt,
        prompt_rx,
        agent_tx,
    ));
    let state = Arc::new(AtomicU8::new(AgentState::IDLE));
    let bridge_handle = tokio::spawn(bridge_task(
        agent_rx,
        events_tx.clone(),
        shutdown_cancel.clone(),
        state.clone(),
    ));

    Ok(Agent {
        prompt_tx,
        events_tx,
        deferred,
        config,
        agent_handle,
        bridge_handle,
        store,
        session_dir: Some(session_dir),
        cancel,
        state,
        auth_token,
        env,
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

    let id = uuid::Uuid::new_v4().to_string();
    let auth_token = uuid::Uuid::new_v4().to_string();

    let mut config = {
        let ws = req.workspace_root.map(std::path::PathBuf::from);
        AgentConfig::load(req.prompt, req.skills, ws)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    };
    config.agent_id = Some(id.clone());
    let mut env = HashMap::new();
    env.insert("JUST_AGENT_ID".into(), id.clone());
    env.insert("JUST_AGENT_AUTH_TOKEN".into(), auth_token.clone());

    // Subagent: validate supervisor and delegation constraints.
    if let Some(ref supervisor_id) = req.created_by {
        let agents = state.agents.read().await;
        let supervisor = crate::auth::require_supervisor(&auth.0, &agents, supervisor_id)?;

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
    let deferred = Arc::new(tokio::sync::Mutex::new(DeferredQueue::new()));

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

    let session_dir = persistence::create_session(
        &id,
        &config.workspace_root,
        config.created_by.as_deref(),
        config.permissions.max_depth,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let prompt = config.prompt.take();
    let log_ws = config.workspace_root.display().to_string();
    let log_depth = config.permissions.max_depth;
    let (events_tx, _) = broadcast::channel(256);
    let agent = spawn_agent(
        store,
        deferred,
        session_dir,
        config,
        prompt,
        state.shutdown.clone(),
        events_tx,
        auth_token,
        env,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    {
        let mut agents = state.agents.write().await;
        // Re-verify supervisor was not deleted during agent spawn.
        if let Some(ref supervisor_id) = req.created_by
            && !agents.iter().any(|e| e.id == *supervisor_id)
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
        agents.push(AgentEntry { id: id.clone(), agent });
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
    let agents = state.agents.read().await;
    let summaries: Vec<AgentSummary> = agents
        .iter()
        .map(|entry| AgentSummary {
            id: entry.id.clone(),
            workspace_root: entry.agent.config.workspace_root.display().to_string(),
            state: entry.agent.get_state(),
        })
        .collect();
    Json(ListAgentsResponse { agents: summaries })
}

pub async fn delete_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let entry = {
        let mut agents = state.agents.write().await;
        crate::auth::require_superior(&auth.0, &agents, &id)?;
        let Some(idx) = agents.iter().position(|e| e.id == id) else {
            return Ok(StatusCode::NOT_FOUND);
        };
        // Agent must be idle and have no subagents.
        let entry = &agents[idx];
        if entry.agent.get_state() != AgentState::Idle {
            return Err((
                StatusCode::CONFLICT,
                "agent is busy, interrupt it first".into(),
            ));
        }
        let has_subagents = agents
            .iter()
            .any(|e| e.agent.config.created_by.as_deref() == Some(id.as_str()));
        if has_subagents {
            return Err((
                StatusCode::CONFLICT,
                "agent has active subagents, delete or interrupt them first".into(),
            ));
        }
        agents.remove(idx)
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
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let agents = state.agents.read().await;
    crate::auth::require_superior(&auth.0, &agents, &id)?;
    let Some(entry) = agents.iter().find(|e| e.id == id) else {
        return Ok(StatusCode::NOT_FOUND);
    };
    entry.agent.cancel.cancel();
    Ok(StatusCode::ACCEPTED)
}

/// Fire-and-forget: spawn one restore task per persisted session.
///
/// Returns immediately so the HTTP server can start accepting requests.
/// Each session restores concurrently; agents appear in the map once ready.
///
/// TODO: A tampered `meta.json` could set a subagent's workspace_root outside its
/// supervisor's boundary, bypassing the validation done at creation time. Consider
/// reading each supervisor's `meta.json` at restore and verifying the subagent's
/// workspace remains within the supervisor's workspace_root.
pub async fn restore_sessions(state: &SharedState) {
    let pending = persistence::scan_sessions();
    if pending.is_empty() {
        return;
    }

    info!(count = pending.len(), "restoring sessions");
    for p in pending {
        let state = state.clone();
        tokio::spawn(async move {
            let sess = match persistence::restore_session(&p.agent_id, &p.session_dir) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(id = %p.agent_id, "restore failed: {e:#}");
                    return;
                }
            };

            let mut config = match AgentConfig::load(None, vec![], Some(p.workspace_root.clone())) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(id = %p.agent_id, "restore config failed: {e:#}");
                    return;
                }
            };
            config.agent_id = Some(p.agent_id.clone());
            config.created_by = p.created_by.clone();
            if let Some(depth) = p.max_depth {
                config.permissions.max_depth =
                    depth.min(just_agent_core::config::DEFAULT_MAX_DEPTH);
            }

            let store = Arc::new(tokio::sync::Mutex::new(sess.store));
            let deferred = Arc::new(tokio::sync::Mutex::new(sess.deferred));
            let (events_tx, _) = broadcast::channel(256);

            let auth_token = uuid::Uuid::new_v4().to_string();
            let mut env = HashMap::new();
            env.insert("JUST_AGENT_ID".into(), p.agent_id.clone());
            env.insert("JUST_AGENT_AUTH_TOKEN".into(), auth_token.clone());

            match spawn_agent(
                store,
                deferred,
                sess.session_dir,
                config,
                None,
                state.shutdown.clone(),
                events_tx,
                auth_token,
                env,
            )
            .await
            {
                Ok(agent) => {
                    state
                        .agents
                        .write()
                        .await
                        .push(AgentEntry { id: sess.agent_id.clone(), agent });
                    info!(id = %sess.agent_id, "restored session");
                }
                Err(e) => {
                    tracing::error!(id = %sess.agent_id, "restore failed: {e:#}");
                }
            }
        });
    }
}
