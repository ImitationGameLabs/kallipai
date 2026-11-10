//! The session-list face: every conversation this tagma can read, in one list.
//!
//! The list is what `kallip lesche sessions` prints. It is a read path, so it
//! takes the operator-readable guard rather than the write-side one.

use axum::Json;
use axum::extract::{Path, State};
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::ApiError;
use serde::Serialize;

use crate::state::SharedState;

use super::shared::require_root_self_or_operator;

/// One row of the unified `GET /agents/{id}/lesche/sessions` list (the
/// `kallip lesche sessions` data): every surface the agent can address with
/// `kallip lesche send`, with its kind and target metadata.
#[derive(Debug, Serialize)]
pub(crate) struct SessionListEntry {
    /// `bilateral` (the fixed 1:1 with the operator), `room`, or `direct`.
    pub kind: &'static str,
    /// The surface id: conversation id, room id, or derived session id.
    pub id: String,
    /// Room display name (rooms only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The peer's tagma id (direct sessions only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_tagma: Option<String>,
    /// The peer's server-stamped handle (direct sessions only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_handle: Option<String>,
}

/// `GET /agents/{id}/lesche/sessions` — every addressable surface in one
/// list: the user bilateral conversation (the projector-owned conversation
/// id, the default send target), the joined rooms, and the direct sessions.
/// Aggregated from the online relays' `list_my_rooms` +
/// `list_direct_sessions` (the lesche exposes no aggregate endpoint);
/// best-effort per relay (a relay whose list fails is skipped and its
/// surfaces reappear when it returns), duplicates collapsed by (kind, id)
/// for two relays sharing one lesche.
pub async fn list_lesche_sessions(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<Json<Vec<SessionListEntry>>, ApiError> {
    require_root_self_or_operator(&state, auth.identity(), &id).await?;
    let mut out: Vec<SessionListEntry> = Vec::new();
    // The bilateral 1:1 is a fixed member of the list.
    if let Some(projector) = state.external.get()
        && let Some(conv) = projector.conversation_id()
    {
        out.push(SessionListEntry {
            kind: "bilateral",
            id: conv,
            name: None,
            peer_tagma: None,
            peer_handle: None,
        });
    }
    let mut seen: std::collections::HashSet<(bool, String)> = Default::default();
    for handle in state.relay_handles() {
        let client = handle.lesche_client();
        match client.list_my_rooms(handle.tagma_id()).await {
            Ok(rooms) => {
                for room in rooms {
                    if seen.insert((false, room.room_id.as_ref().to_owned())) {
                        out.push(SessionListEntry {
                            kind: "room",
                            id: room.room_id.as_ref().to_owned(),
                            name: room.name,
                            peer_tagma: None,
                            peer_handle: None,
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "sessions: room list failed; skipping relay");
            }
        }
        match client.list_direct_sessions().await {
            Ok(sessions) => {
                for view in sessions {
                    if seen.insert((true, view.session_id.as_ref().to_owned())) {
                        out.push(SessionListEntry {
                            kind: "direct",
                            id: view.session_id.as_ref().to_owned(),
                            name: None,
                            peer_tagma: Some(view.peer.tagma_id.as_ref().to_owned()),
                            peer_handle: Some(view.peer.handle),
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "sessions: direct list failed; skipping relay");
            }
        }
    }
    out.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    Ok(Json(out))
}
