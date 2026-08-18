use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallip_common::protocol::{ApiError, MessageResponse};
use serde::{Deserialize, Serialize};

use super::MessageRequest;
use crate::sse::sse_stream;
use crate::state::SharedState;
use kallip_common::agentid::AgentId;

/// Any authenticated agent may send a message to any other agent.
/// This is intentional: inter-agent communication should not require a
/// supervisor relationship. Agents cooperate as peers.
///
/// Returns [`MessageResponse`] with queue depth feedback:
/// - `queue_depth == 0`: agent will process the message immediately.
/// - `queue_depth > 0`: message is queued behind existing messages (warning included).
/// - `503`: message queue is full, caller should retry later.
pub async fn send_message(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(req): Json<MessageRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), ApiError> {
    // Offline-direct path: there is no relay envelope and no authenticated
    // user, so the inbound is recorded under the operator partition (`NULL`) —
    // `deliver_message` receives `sender = None` for that. The relay path
    // passes the envelope `Participant` explicitly.
    let response =
        crate::delivery::deliver_message(&state, auth.identity().clone(), None, &id, &req.text)
            .await?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

/// Any authenticated agent may subscribe to any other agent's event stream.
/// Mirrors the peer communication model of `send_message`.
pub async fn sse_events(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    // Subscribe and clone the sender under one lock, then build the SSE stream
    // after releasing it. The sender outlives this call (held by the agent's
    // registry entry); the receiver-count transition logged by `sse_stream` is
    // observed against the same channel the receiver was subscribed to.
    let (rx, events_tx) = {
        let registry = state.registry.read().await;
        let entry = registry
            .get(&id)
            .ok_or_else(|| ApiError::not_found("agent not found"))?;
        let live = entry
            .as_live()
            .ok_or_else(|| ApiError::conflict("agent is faulted; no event stream"))?;
        let rx = live.agent.events_tx.subscribe();
        let events_tx = live.agent.events_tx.clone();
        (rx, events_tx)
    };
    Ok(sse_stream(id, events_tx, rx, state.shutdown.clone()))
}

/// The direct (local) external SSE: serves the projected external vocabulary
/// (authored messages, runtime signals, status snapshots) to a local frontend
/// client. Root-only — the direct stream is the root agent's conversation, and
/// the projector subscribes to the root regardless of the path id, so a non-root
/// id is rejected to keep the route honest. Any authenticated identity may
/// subscribe (the root owns the single conversation). The stream is one
/// multiplexed channel discriminated by the SSE `event:` name.
pub async fn external_events(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    // Hold the registry read-lock once for both the root check and the initial
    // status snapshot. The snapshot is prepended to the SSE so the chat header
    // renders at connect instead of after the next status-pump tick (~2 s).
    let initial = {
        let registry = state.registry.read().await;
        let is_root = registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == &id);
        if !is_root {
            return Err(ApiError::not_found(
                "external event stream is root-only; the direct conversation is the root's",
            ));
        }
        let payload = crate::relay::status_pump::snapshot_status(&registry, &state.token_budget);
        crate::direct::DirectFrame::Status(payload)
    };
    let direct = state
        .direct
        .get()
        .ok_or_else(|| ApiError::unavailable("direct serving not initialized"))?;
    let rx = direct.subscribe();
    Ok(crate::sse::direct_sse_stream(
        rx,
        Some(initial),
        state.shutdown.clone(),
    ))
}

/// Query params for [`external_history`]: `after` (incremental catch-up, rows
/// with id > after), `before` (scroll-up, id < before), or neither (the most
/// recent `limit`). Mirrors the relay `TagmaControl::History` modes.
#[derive(Debug, Default, Deserialize)]
pub struct ExternalHistoryQuery {
    #[serde(default)]
    pub after: Option<i64>,
    #[serde(default)]
    pub before: Option<i64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `GET /agents/{id}/external/history` -- the pull-based history window for the
/// direct (offline) path, cursor-driven by the frontend's high-water mark.
/// Root-only and projector-owned; returns decoded entries (sender + reply) and
/// a `more` flag. The direct SSE stays live-only; history is always a
/// frontend-initiated pull, symmetric with the relay's `TagmaControl::History`.
#[derive(Debug, Serialize)]
pub struct ExternalHistoryResponse {
    pub rows: Vec<kallip_lesche_common::message::HistoryEntry>,
    pub more: bool,
}

pub async fn external_history(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Query(query): Query<ExternalHistoryQuery>,
) -> Result<Json<ExternalHistoryResponse>, ApiError> {
    {
        let registry = state.registry.read().await;
        let is_root = registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == &id);
        if !is_root {
            return Err(ApiError::not_found(
                "external history is root-only; the direct conversation is the root's",
            ));
        }
    }
    let projector = state
        .external
        .get()
        .ok_or_else(|| ApiError::unavailable("external projector not initialized"))?;
    // Bound the page the same way the relay does (HISTORY_BATCH_MAX = 50); a
    // missing `limit` defaults to the cap.
    const DEFAULT_LIMIT: u32 = 50;
    const MAX_LIMIT: u32 = 50;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    // The direct endpoint serves the operator partition (`user_id IS NULL`).
    let (rows, more) = projector
        .read_history(None, query.after, query.before, limit)
        .await;
    Ok(Json(ExternalHistoryResponse { rows, more }))
}

#[cfg(test)]
mod tests;
