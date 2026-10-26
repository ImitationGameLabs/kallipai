//! Turn-history read routes: the paged log, one turn, and one call's text.
//!
//! Read-only over the agent's own history log, and this tagma's audit
//! surface: any authenticated identity may read any agent's turns — a faulted
//! agent keeps its directory after its process is gone, so a dead agent stays
//! auditable.

use std::path::{Path as FsPath, PathBuf};

use axum::Json;
use axum::extract::{Path, Query, State};
use kallipai_adk::history::view::{self, ViewError};
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::{
    ApiError, CallBody, CallBodyQuery, TurnDetail, TurnsPage, TurnsQuery,
};

use crate::state::SharedState;

/// The directory holding the named agent's history log.
async fn history_dir(state: &SharedState, id: &AgentId) -> Result<PathBuf, ApiError> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    entry
        .identity()
        .agent_dir
        .clone()
        .ok_or_else(|| ApiError::not_found("agent has no directory"))
}

/// Refuse an agent with no history log: an empty page would read as an
/// agent whose log is empty, which is a different statement.
fn require_history(dir: &FsPath) -> Result<(), ApiError> {
    if view::has_history(dir) {
        Ok(())
    } else {
        Err(ApiError::not_found("agent has no history log"))
    }
}

/// A malformed request is the caller's, an absent turn or call is the log's.
fn map_view_error(error: ViewError) -> ApiError {
    match error {
        ViewError::BadDay(_) => {
            // The caller's own text is not echoed back: a day is either a
            // shape this face accepts or it is not.
            ApiError::bad_request("day must be shaped YYYY-MM-DD")
        }
        ViewError::BadCursor(_) => ApiError::bad_request("unreadable cursor"),
        ViewError::NoSuchTurn(turn_id) => {
            ApiError::not_found(format!("no turn {turn_id} in this agent's turn history"))
        }
        ViewError::NoSuchCall(turn_id, call_index) => {
            ApiError::not_found(format!("no call {call_index} in turn {turn_id}"))
        }
    }
}

/// `GET /agents/{id}/turns` — one page of the agent's turn log, newest first.
///
/// `day` pins the walk to one UTC day file, `before` continues from a previous
/// page's `next_cursor`, and `limit` is clamped into the page bounds. Rows
/// carry no message text; the text of one turn is its detail read, and tool
/// output is a separate range read.
pub async fn list_turns(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Query(query): Query<TurnsQuery>,
) -> Result<Json<TurnsPage>, ApiError> {
    let dir = history_dir(&state, &id).await?;
    require_history(&dir)?;
    Ok(Json(
        view::read_turns_page(&dir, &query).map_err(map_view_error)?,
    ))
}

/// `GET /agents/{id}/turns/{turn_id}` — one turn, its message text included.
///
/// Every message carries its own text, shortened head-and-tail; a tool
/// message carries the result record the log stored for that call, cut the
/// same way, and a stream that was spilled reads as reclaimed rather than as
/// a path to follow. The full text is the call-body read below.
pub async fn get_turn(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path((id, turn_id)): Path<(AgentId, u64)>,
) -> Result<Json<TurnDetail>, ApiError> {
    let dir = history_dir(&state, &id).await?;
    require_history(&dir)?;
    Ok(Json(
        view::read_turn(&dir, turn_id).map_err(map_view_error)?,
    ))
}

/// `GET /agents/{id}/turns/{turn_id}/calls/{call_index}/body` — a byte range
/// of one call's text.
///
/// `offset` and `limit` walk the text the log holds for that call's result,
/// capped per request; `next_offset` continues where a capped range stopped.
pub async fn get_call_body(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path((id, turn_id, call_index)): Path<(AgentId, u64, u32)>,
    Query(query): Query<CallBodyQuery>,
) -> Result<Json<CallBody>, ApiError> {
    let dir = history_dir(&state, &id).await?;
    require_history(&dir)?;
    Ok(Json(
        view::read_call_body(&dir, turn_id, call_index, &query).map_err(map_view_error)?,
    ))
}

#[cfg(test)]
mod tests;
