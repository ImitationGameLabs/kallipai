use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_core::context::{AgenticContext, ContextUsage};

use crate::state::SharedState;
use just_agent_core::retry::RetryRecord;
use serde::Serialize;

/// Combined status response: context usage + recent retry history.
#[derive(Serialize)]
pub struct AgentStatus {
    pub context: ContextUsage,
    pub recent_retries: Vec<RetryRecord>,
}

/// GET /agents/{id}/status — return context usage and retry history.
pub async fn agent_status(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let agents = state.agents.read().await;
    let entry = agents
        .iter()
        .find(|e| e.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let store = entry.agent.store.lock().await;
    let context = store.usage_snapshot();
    let recent_retries = store
        .retry_log
        .iter()
        .rev()
        .take(20)
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(AgentStatus { context, recent_retries }))
}
