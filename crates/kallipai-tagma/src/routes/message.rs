use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallipai_common::protocol::{ApiError, MessageResponse};
use serde::{Deserialize, Serialize};

use super::MessageRequest;
use crate::external::HistoryReadError;
use crate::sse::sse_stream;
use crate::state::SharedState;
use kallipai_common::agentid::AgentId;

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
    let attachment = req.attachment;
    let response = crate::delivery::deliver_message(
        &state,
        auth.identity().clone(),
        None,
        &id,
        &req.text,
        attachment,
        req.defer,
    )
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
    // Root check first, in its own short read-lock hold: the rejection must
    // not depend on, or race with, the subscribe/snapshot assembly below.
    {
        let registry = state.registry.read().await;
        let is_root = registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == &id);
        if !is_root {
            return Err(ApiError::not_found(
                "external event stream is root-only; the direct conversation is the root's",
            ));
        }
    }
    // Snapshot-then-live via the shared primitive: the three-topic bus
    // subscription runs BEFORE the connect-time status capture, so a status
    // published in the window between the two still lands in the live stream
    // (the snapshot can only repeat it; last-wins absorbs the repeat). The
    // old inline order — capture, then subscribe — lost exactly that frame,
    // leaving the header stale until the next pump tick. The capture remains
    // the on-demand query through the shared pure function; the bus
    // holds no latest-state.
    let (subscribed, initial) = kallipai_common::sse::open_snapshot_stream(
        || {
            (
                state
                    .bus
                    .subscribe::<crate::bus::AuthoredFrame>()
                    .expect("authored topic is registered"),
                state
                    .bus
                    .subscribe::<crate::bus::SignalFrame>()
                    .expect("signal topic is registered"),
                state
                    .bus
                    .subscribe::<crate::bus::StatusSnapshot>()
                    .expect("status topic is registered"),
            )
        },
        || async {
            let registry = state.registry.read().await;
            vec![crate::sse::DirectFrame::Status(
                crate::relay::status_pump::snapshot_status(&registry, &state.token_budget),
            )]
        },
    )
    .await;
    let (authored, signals, status) = subscribed;
    let frames = crate::sse::merge_direct_frames(authored, signals, status);
    // The connect snapshot yields exactly one Status frame; the assert
    // keeps a future multi-frame snapshot closure from silently dropping
    // every frame after the first (the consumer takes only the head).
    assert_eq!(
        initial.len(),
        1,
        "connect snapshot must yield exactly one frame"
    );
    Ok(crate::sse::direct_sse_stream(
        frames,
        initial.into_iter().next(),
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
/// The badge count does not ride this reply: it reaches the client on the
/// unread frames the tagma pushes, so a history pull never doubles as a count
/// read.
#[derive(Debug, Serialize)]
pub struct ExternalHistoryResponse {
    pub rows: Vec<kallipai_lesche_common::message::HistoryEntry>,
    pub more: bool,
    /// Rows the window held that the decoder could not read: they are not in
    /// `rows`, so a non-zero count means this page is a hole in the history
    /// rather than a complete window. This struct only serializes, so the
    /// field always goes out; the compatibility lives in the client, where
    /// `undecodable` is optional and the direct transport fills in zero.
    pub undecodable: u32,
}

pub async fn external_history(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Query(query): Query<ExternalHistoryQuery>,
) -> Result<Json<ExternalHistoryResponse>, ApiError> {
    require_root(&state, &id, "external history").await?;
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
    let page = match projector
        .read_history(None, query.after, query.before, limit)
        .await
    {
        Ok(page) => page,
        // A query naming two cursors asks for two windows at once: refusing it
        // keeps the "an empty page means an empty history" reading intact.
        Err(HistoryReadError::BothCursors { .. }) => {
            return Err(ApiError::bad_request(
                "history takes at most one cursor (after or before)",
            ));
        }
        // An unreadable store is not an empty conversation.
        Err(e) => {
            return Err(ApiError::unavailable(format!("history read failed: {e}")));
        }
    };
    Ok(Json(ExternalHistoryResponse {
        rows: page.entries,
        more: page.more,
        undecodable: page.undecodable,
    }))
}

/// The relay peer a manage-plane request targets: inserted as a request
/// extension by the manage router, which knows the peer from the authenticated
/// envelope. The plaintext frame surface carries no peer and is denied this
/// route, so every call that reaches the handler has one.
#[derive(Clone, Debug)]
pub struct ReadPeer(pub String);

/// Body for [`external_read_cursor`]: the highest `chat_history.id` the user has
/// read in that partition.
#[derive(Debug, Deserialize)]
pub struct ReadCursorBody {
    pub cursor: i64,
}

/// `PUT /agents/{id}/external/read-cursor` -- record how far the user has read
/// in one conversation partition. Root-only like the history read. The
/// partition is the relay peer the manage router authenticated (a request
/// extension); the plaintext frame surface carries no peer and is denied
/// this route.
///
/// Answers `204 No Content`, like the lesche read-cursor contract: the write is
/// monotonic and capped inside the store, and the resulting count reaches every
/// device of this user as an unread frame, so there is nothing to read back.
pub async fn external_read_cursor(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    peer: axum::extract::Extension<ReadPeer>,
    Json(body): Json<ReadCursorBody>,
) -> Result<StatusCode, ApiError> {
    require_root(&state, &id, "the read cursor").await?;
    let projector = state
        .external
        .get()
        .ok_or_else(|| ApiError::unavailable("external projector not initialized"))?;
    // Envelope-authenticated: the manage router supplies the peer it
    // authenticated, so the write is always scoped to a conversation.
    let user_id = Some(peer.0.0.as_str());
    projector
        .put_cursor(user_id, body.cursor)
        .await
        .ok_or_else(|| ApiError::unavailable("chat history store not initialized"))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reject any agent but the root: the direct (`/external/*`) surface belongs to
/// the root's own conversation.
async fn require_root(state: &SharedState, id: &AgentId, what: &str) -> Result<(), ApiError> {
    let registry = state.registry.read().await;
    let is_root = registry
        .root_agent()
        .is_some_and(|(root_id, _)| root_id == id);
    if is_root {
        Ok(())
    } else {
        Err(ApiError::not_found(format!(
            "{what} is root-only; the direct conversation is the root's"
        )))
    }
}

#[cfg(test)]
mod tests;
