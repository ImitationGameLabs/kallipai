use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_core::config::AgentConfig;
use just_agent_core::context::{AgenticContext, ContextStore, SummarizeStrategy};
use just_agent_core::deferred::DeferredQueue;
use just_agent_core::policy::{AgentPolicy, AuthorizedToolExecutor};
use just_agent_core::provider::client_from_env;
use just_agent_core::session::{self, AgentContext};
use just_agent_core::tools::{build_tool_dispatch, ensure_meta_skill, load_skill};
use just_llm_client::types::chat::ChatMessage;
use tracing::info;

use super::{CreateAgentRequest, CreateAgentResponse, ListAgentsResponse};
use crate::bridge::bridge_task;
use crate::state::{Agent, AgentEntry, AgentSummary, SharedState};

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

    let client = {
        let meta =
            ensure_meta_skill().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut sp = config.system_prompt.clone();
        sp.push_str("\n\n");
        sp.push_str(&meta);
        client_from_env(&sp).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
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

    let dispatch = build_tool_dispatch(store.clone())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);
    let (prompt_tx, prompt_rx) = tokio::sync::mpsc::channel(16);

    let executor = AuthorizedToolExecutor::new(
        dispatch,
        AgentPolicy::new(config.workspace_root.clone()),
        deferred.clone(),
    );
    let tool_defs = executor.tool_definitions();
    store.lock().await.set_tool_definitions(tool_defs);
    let strategy: Box<dyn just_agent_core::context::CompactionStrategy> =
        Box::new(SummarizeStrategy::new(config.compact_max_tokens));

    let prompt = config.prompt.take();
    let ctx = AgentContext {
        client,
        store: store.clone(),
        deferred: deferred.clone(),
        executor,
        strategy,
        config: config.clone(),
    };

    let agent_handle = tokio::spawn(session::agent_task(ctx, prompt, prompt_rx, agent_tx));
    let (events_tx, _) = tokio::sync::broadcast::channel(256);
    let bridge_handle = tokio::spawn(bridge_task(agent_rx, events_tx.clone()));

    let agent = Agent {
        prompt_tx,
        events_tx,
        deferred,
        config,
        agent_handle,
        bridge_handle,
        store: store.clone(),
    };

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
            skills: entry.agent.config.skills.clone(),
        })
        .collect();
    Json(ListAgentsResponse { agents: summaries })
}

pub async fn delete_agent(State(state): State<SharedState>, Path(id): Path<String>) -> StatusCode {
    let mut agents = state.agents.write().await;
    if let Some(idx) = agents.iter().position(|e| e.id == id) {
        let entry = agents.remove(idx);
        entry.agent.agent_handle.abort();
        entry.agent.bridge_handle.abort();
        info!(id = %id, "deleted agent");
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
