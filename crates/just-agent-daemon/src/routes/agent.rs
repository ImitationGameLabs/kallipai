use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_core::config::AgentConfig;
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
    parent_cancel: CancellationToken,
    events_tx: broadcast::Sender<SseEvent>,
) -> anyhow::Result<Agent> {
    let cancel = parent_cancel.child_token();

    let client = {
        let meta = ensure_meta_skill()?;
        let mut sp = config.system_prompt.clone();
        sp.push_str("\n\n");
        sp.push_str(&meta);
        client_from_env(&sp)?
    };

    let dispatch = build_tool_dispatch(store.clone()).await?;

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
        parent_cancel.clone(),
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
    })
}

pub async fn create_agent(
    State(state): State<SharedState>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let id = uuid::Uuid::new_v4().to_string();

    let mut config = {
        let ws = req.workspace_root.map(std::path::PathBuf::from);
        AgentConfig::load(req.prompt, req.skills, ws)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    };

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

    let session_dir = persistence::create_session(&id, &config.workspace_root)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let prompt = config.prompt.take();
    let (events_tx, _) = broadcast::channel(256);
    let agent = spawn_agent(
        store,
        deferred,
        session_dir,
        config,
        prompt,
        state.shutdown.clone(),
        events_tx,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state
        .agents
        .write()
        .await
        .push(AgentEntry { id: id.clone(), agent });
    info!(id = %id, "created agent");
    Ok((StatusCode::CREATED, Json(CreateAgentResponse { id })))
}

pub async fn list_agents(State(state): State<SharedState>) -> Json<ListAgentsResponse> {
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

pub async fn delete_agent(State(state): State<SharedState>, Path(id): Path<String>) -> StatusCode {
    let entry = {
        let mut agents = state.agents.write().await;
        let Some(idx) = agents.iter().position(|e| e.id == id) else {
            return StatusCode::NOT_FOUND;
        };
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
    StatusCode::NO_CONTENT
}

/// Interrupt the current agent operation without deleting it.
pub async fn interrupt_agent(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> StatusCode {
    let agents = state.agents.read().await;
    let Some(entry) = agents.iter().find(|e| e.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    entry.agent.cancel.cancel();
    StatusCode::ACCEPTED
}

/// Fire-and-forget: spawn one restore task per persisted session.
///
/// Returns immediately so the HTTP server can start accepting requests.
/// Each session restores concurrently; agents appear in the map once ready.
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

            let config = match AgentConfig::load(None, vec![], Some(p.workspace_root.clone())) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(id = %p.agent_id, "restore config failed: {e:#}");
                    return;
                }
            };

            let store = Arc::new(tokio::sync::Mutex::new(sess.store));
            let deferred = Arc::new(tokio::sync::Mutex::new(sess.deferred));
            let (events_tx, _) = broadcast::channel(256);

            match spawn_agent(
                store,
                deferred,
                sess.session_dir,
                config,
                None,
                state.shutdown.clone(),
                events_tx,
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
