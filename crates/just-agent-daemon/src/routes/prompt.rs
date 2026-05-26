use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_core::command::UserInput;
use tracing::{error, info};

use super::PromptRequest;
use crate::routes::agent::spawn_agent;
use crate::sse::sse_stream;
use crate::state::SharedState;

pub async fn send_prompt(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<PromptRequest>,
) -> Result<StatusCode, StatusCode> {
    // Fast path: agent is alive, send directly.
    {
        let agents = state.agents.read().await;
        let entry = agents
            .iter()
            .find(|e| e.id == id)
            .ok_or(StatusCode::NOT_FOUND)?;
        if entry
            .agent
            .prompt_tx
            .send(UserInput::Prompt(req.text.clone()))
            .await
            .is_ok()
        {
            return Ok(StatusCode::ACCEPTED);
        }
    }

    // Slow path: agent is dead, reactivate.
    let mut agents = state.agents.write().await;
    let entry = agents
        .iter_mut()
        .find(|e| e.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // Double-check under write lock: another request may have reactivated.
    if entry
        .agent
        .prompt_tx
        .send(UserInput::Prompt(req.text.clone()))
        .await
        .is_ok()
    {
        return Ok(StatusCode::ACCEPTED);
    }

    info!(id = %id, "reactivating agent");
    entry.agent.agent_handle.abort();
    entry.agent.bridge_handle.abort();
    let session_dir = entry.agent.session_dir.clone().unwrap_or_default();
    entry.agent = spawn_agent(
        entry.agent.store.clone(),
        entry.agent.deferred.clone(),
        session_dir,
        entry.agent.config.clone(),
        Some(req.text),
        state.shutdown.clone(),
        entry.agent.events_tx.clone(),
    )
    .await
    .map_err(|e| {
        error!(id = %id, "reactivation failed: {e:#}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
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
