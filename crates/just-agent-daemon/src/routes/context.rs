use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_core::command::{SlashCommand, UserInput};
use just_agent_core::context::{AgenticContext, ContextUsage};

use super::SkillRequest;
use crate::state::SharedState;

/// GET /agents/{id}/status — return current context usage snapshot.
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
    let usage: ContextUsage = store.usage_snapshot();
    Ok(Json(usage))
}

/// POST /agents/{id}/compact — request context compaction.
pub async fn agent_compact(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let agents = state.agents.read().await;
    let entry = agents
        .iter()
        .find(|e| e.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    entry
        .agent
        .prompt_tx
        .send(UserInput::Command(SlashCommand::Compact))
        .await
        .map_err(|_| StatusCode::GONE)?;
    Ok(StatusCode::ACCEPTED)
}

/// POST /agents/{id}/skill — load a skill by name.
pub async fn agent_load_skill(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<SkillRequest>,
) -> Result<StatusCode, StatusCode> {
    let agents = state.agents.read().await;
    let entry = agents
        .iter()
        .find(|e| e.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    entry
        .agent
        .prompt_tx
        .send(UserInput::Command(SlashCommand::Skill { name: req.name }))
        .await
        .map_err(|_| StatusCode::GONE)?;
    Ok(StatusCode::ACCEPTED)
}
