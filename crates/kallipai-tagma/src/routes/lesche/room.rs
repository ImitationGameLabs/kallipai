//! The room face: the rooms this tagma can address and one room's history.
//!
//! The list is the send path's address book and takes the write-side guard; the
//! history read is a read path and answers with [`render_room_block`]'s blocks.
//! Both are root-only.

use axum::Json;
use axum::extract::{Path, Query, State};
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::ApiError;
use kallipai_lesche_common::ids::check_segment;
use kallipai_lesche_common::message::RoomMessage;
use kallipai_lesche_common::rooms::{MemberId, RoomId};

use crate::messaging::MentionView;
use crate::state::SharedState;

use super::render::render_room_block;
use super::shared::{map_lesche_error, refuse_negative_position, require_root_self};

/// `GET /agents/{id}/lesche/rooms` -- the rooms this tagma can address with
/// `kallip lesche send --room <id>`, from the joined-rooms cache (maintained
/// by the relay's room-membership poll).
pub async fn list_joined_rooms(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<Json<Vec<String>>, ApiError> {
    require_root_self(&state, auth.identity(), &id).await?;
    let mut rooms: Vec<String> = state
        .joined_rooms
        .joined_rooms()
        .await
        .into_iter()
        .map(|r| r.as_ref().to_string())
        .collect();
    rooms.sort();
    Ok(Json(rooms))
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct LescheHistoryQuery {
    /// Return messages with `seq > after_seq` (exclusive). Default 0 = from the
    /// start.
    pub after_seq: Option<i64>,
    /// Return messages with `seq < before_seq` (exclusive): the page of OLDER
    /// rows ending below that cursor, rendered oldest-first. Mutually
    /// exclusive with `after_seq`: the pair is rejected on both hops. A single
    /// row lookup is `before_seq = seq + 1` with `limit = 1`.
    pub before_seq: Option<i64>,
    /// Max messages to return. Server-clamped by lesche.
    pub limit: Option<u64>,
}

/// `GET /agents/{id}/lesche/rooms/{room}/messages` — the `kallip lesche read
/// --room <room>` path. Fetches the room's history from lesche (payloads are
/// plaintext `RoomMessage` JSON; the lesche member-gates the read) and renders
/// a readable text block. This is the agent's only way to pull room history:
/// the bilateral reconnect-replay protocol does not cover rooms.
///
/// Format: one bracketed block per room message row, separated by a blank
/// line, multiline-safe:
/// `[seq=<n> from=<kind>:<id>[ "<handle>"] at=<timestamp>[ | quote=<seq>][ | mentions=<value>]]\n<text>\n\n`
/// where `kind` is `agent` or `human` (a room may have user-device members),
/// `quote=<seq>` names the row this one answers, and `mentions=<value>` states
/// this reader's own view of whom the row names: `you` when it names the reader
/// alone, `<n>` when it names `n` other members, `you+<n>` when it names both.
/// Both segments are absent when they have nothing to say, and rows that fail
/// to parse are skipped, so one bad row never blanks the read.
pub async fn read_room_messages(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, room)): Path<(AgentId, String)>,
    Query(q): Query<LescheHistoryQuery>,
) -> Result<String, ApiError> {
    require_root_self(&state, auth.identity(), &id).await?;
    // One page moves in one direction: reject the ambiguous pair here rather
    // than forwarding it. The lesche answers 400, which this hop would
    // otherwise surface as a 502 and read as a retryable upstream fault.
    if q.after_seq.is_some() && q.before_seq.is_some() {
        return Err(ApiError::bad_request(
            "after_seq and before_seq are mutually exclusive",
        ));
    }
    // Both cursors are this request's own shape, refused before any relay is
    // resolved for the reason above.
    refuse_negative_position(&[q.after_seq, q.before_seq])?;
    let room = RoomId::from(room);
    // The room id rides this read's path to the lesche, and a dot
    // segment survives encoding, so it is refused here: left to the
    // client, the value would answer this hop's 503 (no relay owns it)
    // and read as a missing relay.
    check_segment(room.as_ref()).map_err(|err| ApiError::bad_request(format!("room id: {err}")))?;
    // Route via room ownership: each relay's poll keys its slice of the
    // joined-rooms cache by entry name, and a room id is unique to the lesche
    // that created it, so the owning relay's client serves the read. A cold
    // cache (no poll has warmed yet) is indistinguishable from "no relay
    // online" and surfaces as unavailable, same as before — self-correcting
    // on the next poll.
    let (client, viewer) = {
        let Some(owner) = state.joined_rooms.owner_of(&room).await else {
            return Err(ApiError::unavailable(
                "no online relay owns this room; cannot read room history",
            ));
        };
        let Some(handle) = state.relay(&owner) else {
            return Err(ApiError::unavailable(
                "relay not online; cannot read room history",
            ));
        };
        // The same handle carries this tagma's id on that relay, which is the
        // reader the block headers speak to: the view of the mentions is its
        // own, not a roster lookup.
        (
            handle.lesche_client(),
            MemberId::for_tagma(handle.tagma_id()),
        )
    };
    let rows = client
        .fetch_room_messages(
            &room,
            kallipai_lesche_client::RoomHistoryQuery {
                after_seq: q.after_seq,
                before_seq: q.before_seq,
                limit: q.limit,
            },
        )
        .await
        .map_err(|e| map_lesche_error("room history fetch failed", &e))?;
    // Render each row. The payload IS the plaintext `RoomMessage` JSON; the
    // sender is the relay-authenticated envelope sender stamped on the stored
    // row. A row that fails to parse is skipped with a warn rather than failing
    // the whole read.
    let mut out = String::new();
    for row in rows {
        let request: RoomMessage = match serde_json::from_slice(&row.ciphertext.0) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(room = %room, seq = row.seq, "room history parse skipped: {e}");
                continue;
            }
        };
        let RoomMessage {
            text,
            quote,
            mentions,
            ..
        } = request;
        let mentions = MentionView::of(&mentions, &viewer);
        // The sender is the relay-authenticated envelope participant stamped on
        // the stored row: its id and kind are authenticated, its handle is
        // advisory and sanitized inside the renderer.
        out.push_str(&render_room_block(
            row.seq,
            &row.sender,
            row.created_at,
            &text,
            quote,
            mentions,
        ));
    }
    Ok(out)
}
