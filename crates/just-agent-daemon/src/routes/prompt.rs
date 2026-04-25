use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_core::command::UserInput;

use super::PromptRequest;
use crate::sse::sse_stream;
use crate::state::SharedState;

pub async fn send_prompt(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<PromptRequest>,
) -> Result<StatusCode, StatusCode> {
    let agents = state.agents.read().await;
    let entry = agents
        .iter()
        .find(|e| e.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    entry
        .agent
        .prompt_tx
        .send(UserInput::Prompt(req.text))
        .await
        .map_err(|_| StatusCode::GONE)?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn sse_events(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let agents = state.agents.read().await;
    let entry = agents
        .iter()
        .find(|e| e.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let rx = entry.agent.events_tx.subscribe();
    Ok(sse_stream(rx))
}
