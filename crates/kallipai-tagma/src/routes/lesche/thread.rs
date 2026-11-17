//! The thread-choice face: the caller's standing choice about one discussion in
//! one room (`GET`/`PUT`/`DELETE .../threads/{root}/subscription`).
//!
//! A room id is unique to the lesche that created it, so the relay that owns
//! the room serves the call (the same routing the room history read takes). The
//! member is the authenticated agent and the routes carry no member id, so the
//! only row a write can reach is the caller's own.
//!
//! `{root}` names the discussion by the row that heads it: the lesche answers
//! 404 for a seq that heads nothing in that room, and this hop leaves that
//! verdict to the lesche. Only the shape of the segment is judged here.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::ApiError;
use kallipai_lesche_client::LescheClient;
use kallipai_lesche_common::ids::check_segment;
use kallipai_lesche_common::rooms::{RoomId, ThreadChoice, ThreadSubscriptionView};

use crate::state::SharedState;

use super::shared::{map_lesche_error, require_root_self};

/// Body of `PUT /agents/{id}/lesche/rooms/{room}/threads/{root}/subscription`.
#[derive(serde::Deserialize)]
pub(crate) struct ThreadChoiceRequest {
    /// `following` or `muted`. Holding no choice is `DELETE`, not a value.
    pub state: String,
}

/// The calling room id, judged as the path segment it is: a dot segment
/// survives encoding, so it is refused here rather than surfacing as a missing
/// relay.
fn thread_room(raw: String) -> Result<RoomId, ApiError> {
    let room = RoomId::from(raw);
    check_segment(room.as_ref()).map_err(|err| ApiError::bad_request(format!("room id: {err}")))?;
    Ok(room)
}

/// The root segment as a position. The lesche decides whether it heads a
/// thread; a value that is no position at all is this request's own mistake.
fn thread_root(raw: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>()
        .ok()
        .filter(|root| *root >= 1)
        .ok_or_else(|| ApiError::bad_request("thread must be >= 1"))
}

/// The relay client that owns `room`. A cold joined-rooms cache (no poll has
/// warmed yet) is indistinguishable from "no relay online" and surfaces as
/// unavailable, the same answer the room history read gives.
async fn relay_for_room(state: &SharedState, room: &RoomId) -> Result<LescheClient, ApiError> {
    let Some(owner) = state.joined_rooms.owner_of(room).await else {
        return Err(ApiError::unavailable(
            "no online relay owns this room; cannot reach its threads",
        ));
    };
    let Some(handle) = state.relay(&owner) else {
        return Err(ApiError::unavailable(
            "relay not online; cannot reach its threads",
        ));
    };
    Ok(handle.lesche_client())
}

/// `GET /agents/{id}/lesche/rooms/{room}/threads/{root}/subscription` -- the
/// `kallip lesche thread` view of the caller's own standing choice: the choice
/// recorded (if any), the rule delivery will apply, and the room position the
/// choice took effect at.
pub async fn get_thread_subscription(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, room, root)): Path<(AgentId, String, String)>,
) -> Result<Json<ThreadSubscriptionView>, ApiError> {
    require_root_self(&state, auth.identity(), &id).await?;
    let room = thread_room(room)?;
    let root = thread_root(&root)?;
    let client = relay_for_room(&state, &room).await?;
    let view = client
        .thread_subscription(&room, root)
        .await
        .map_err(|e| map_lesche_error("thread subscription read failed", &e))?;
    Ok(Json(view))
}

/// `PUT .../threads/{root}/subscription` -- record the caller's choice. The
/// answer is the row read back, so the caller sees the position its choice took
/// effect at.
pub async fn put_thread_subscription(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, room, root)): Path<(AgentId, String, String)>,
    Json(req): Json<ThreadChoiceRequest>,
) -> Result<Json<ThreadSubscriptionView>, ApiError> {
    require_root_self(&state, auth.identity(), &id).await?;
    let choice = ThreadChoice::parse(req.state.trim())
        .ok_or_else(|| ApiError::bad_request("state must be following or muted"))?;
    let room = thread_room(room)?;
    let root = thread_root(&root)?;
    let client = relay_for_room(&state, &room).await?;
    let view = client
        .set_thread_subscription(&room, root, choice)
        .await
        .map_err(|e| map_lesche_error("thread subscription write failed", &e))?;
    Ok(Json(view))
}

/// `DELETE .../threads/{root}/subscription` -- drop the caller's choice so the
/// discussion goes back to the default. Idempotent, like the lesche route.
pub async fn delete_thread_subscription(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, room, root)): Path<(AgentId, String, String)>,
) -> Result<StatusCode, ApiError> {
    require_root_self(&state, auth.identity(), &id).await?;
    let room = thread_room(room)?;
    let root = thread_root(&root)?;
    let client = relay_for_room(&state, &room).await?;
    client
        .clear_thread_subscription(&room, root)
        .await
        .map_err(|e| map_lesche_error("thread subscription delete failed", &e))?;
    Ok(StatusCode::NO_CONTENT)
}
