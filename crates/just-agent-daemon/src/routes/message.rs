use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_core::command::UserInput;
use tracing::{error, info};

use super::MessageRequest;
use crate::routes::agent::spawn_agent;
use crate::sse::sse_stream;
use crate::state::SharedState;

/// Any authenticated agent may send a message to any other agent.
/// This is intentional: inter-agent communication should not require a
/// supervisor relationship — agents cooperate as peers.
pub async fn send_message(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
    Json(req): Json<MessageRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Fast path: agent is alive, send directly.
    {
        let agents = state.agents.read().await;
        let entry = agents
            .iter()
            .find(|e| e.id == id)
            .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;
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
        .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;

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
    let saved_token = entry.agent.auth_token.clone();
    let saved_env = entry.agent.env.clone();
    entry.agent = spawn_agent(
        entry.agent.store.clone(),
        entry.agent.deferred.clone(),
        session_dir,
        entry.agent.config.clone(),
        Some(req.text),
        state.shutdown.clone(),
        entry.agent.events_tx.clone(),
        saved_token,
        saved_env,
    )
    .await
    .map_err(|e| {
        error!(id = %id, "reactivation failed: {e:#}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("reactivation failed: {e:#}"),
        )
    })?;
    Ok(StatusCode::ACCEPTED)
}

/// Any authenticated agent may subscribe to any other agent's event stream.
/// Mirrors the peer communication model of `send_message`.
pub async fn sse_events(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let agents = state.agents.read().await;
    let entry = agents
        .iter()
        .find(|e| e.id == id)
        .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;
    let rx = entry.agent.events_tx.subscribe();
    Ok(sse_stream(rx))
}
