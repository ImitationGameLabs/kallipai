//! The send face: one request, three targets.
//!
//! `post_message` reads the request's shape (exactly one target, a quote only
//! where a row can be named, mentions only into a room) and then hands off: the
//! bilateral 1:1 goes to the external projector, a room send to
//! [`send_room_message`], a peer tagma to [`send_direct_message`]. Each hand-off
//! carries the shared burst cap, and the two non-bilateral paths bypass the
//! bilateral store on purpose (lesche is the room's and the session's store of
//! record).

use axum::Json;
use axum::extract::{Path, State};
use kallipai_archeion_common::bytes::Ciphertext;
use kallipai_archeion_common::ids::{ChannelId, TagmaId, TraceId};
use kallipai_common::agentid::AgentId;
use kallipai_common::message::DeliveryResponse;
use kallipai_common::protocol::ApiError;
use kallipai_lesche_common::direct::{DirectMessage, DirectSessionId, FileAttachment};
use kallipai_lesche_common::ids::check_segment;
use kallipai_lesche_common::message::{Envelope, RoomMessage, checked_mentions};
use kallipai_lesche_common::rooms::{MemberId, RoomId};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::relay::RelayMessageError;
use crate::state::SharedState;

use super::shared::{map_lesche_error, require_root_self};

#[derive(Debug, Deserialize)]
pub(crate) struct LescheMessageRequest {
    pub text: String,
    /// Optional room id. Present when the agent is sending into a multi-member
    /// room (the tagma posts the plaintext to `/rooms/{room}/envelopes`);
    /// absent for the bilateral 1:1 send. A raw string parsed into a
    /// [`RoomId`] so this route owns the id-type boundary (the `kallip` CLI /
    /// `kallipai-client` stay archeion-id-free).
    pub room: Option<String>,
    /// Optional peer tagma id. Present when the agent is sending into the
    /// direct session with that tagma (create-or-get on first send, so
    /// re-sends are idempotent). Attachments in a direct session must live
    /// in a workspace the peer can read: a file record in a private area
    /// fails the peer's fetch with 403.
    pub tagma: Option<String>,
    /// Optional file reference for a direct-session send (the `tagma` branch
    /// only): `{record_id, name, size}` — the bytes stay in the files service
    /// and the peer fetches the record in its own authorized space (a
    /// private-area record fails the peer's fetch with 403, the same caveat
    /// the CLI's `--tagma` help documents). Any non-tagma branch carrying an
    /// attachment is a 400.
    #[serde(default)]
    pub attachment: Option<FileAttachment>,
    /// Optional position this message answers (the `kallip lesche send
    /// --quote <seq>` path): the per-conversation sequence number of the
    /// message being answered -- a room's `seq`, or, on a direct-session
    /// send, that session's own `seq`. Only the position crosses the
    /// wire: the quoted text is never copied, and `<= 0` reads as no quote
    /// at all. A bilateral 1:1 send carrying one is a 400: that
    /// conversation has no such log.
    #[serde(default)]
    pub quote: Option<i64>,
    /// The members this message names (the `kallip lesche send --mention`
    /// path). Room sends only: an id names a member of that room, so a
    /// bilateral or direct-session send carrying one is a 400. Absent, null,
    /// and empty all read the same: this send names nobody. Each id that does
    /// arrive is judged -- shape and the cap after repeats fold at this
    /// boundary, membership of the room once the roster can answer: a name it
    /// does not hold is refused, and one it cannot answer for goes out
    /// unjudged.
    ///
    /// The key is an `Option<Vec<String>>` rather than a bare `Vec` so an
    /// explicit `null` decodes: serde reads null into a `Vec` field as an
    /// error, and a null key here means this send names nobody: no bad
    /// request, and no membership judgement on the way out.
    #[serde(default)]
    pub mentions: Option<Vec<String>>,
}

/// Deliver a user-facing message: bilateral 1:1 (default) or into a room
/// (`room` set). Guarded by [`require_root_self`] -- only the root agent
/// has an attributed voice; the operator is not authorized to forge it and
/// subagents must route outward communication through their supervisor.
pub async fn post_message(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(req): Json<LescheMessageRequest>,
) -> Result<Json<DeliveryResponse>, ApiError> {
    require_root_self(&state, auth.identity(), &id).await?;
    // `<= 0` reads as no quote at all (see `RoomMessage::quote`), so normalize
    // the request's quote once, here at the boundary: the guard below then
    // sees a real position only, and no non-position can reach the wire.
    let quote = req.quote.filter(|q| *q > 0);
    // A quote names a position in a conversation's own log, so it rides a
    // room send or a direct-session send. The bilateral 1:1 has no such log,
    // so a quote there is a caller bug, not a position that points nowhere.
    if quote.is_some() && req.room.is_none() && req.tagma.is_none() {
        return Err(ApiError::bad_request(
            "quote rides a room or direct send: a seq names a row in that conversation's log",
        ));
    }
    // A mention names a member of the room the send enters, so it rides a
    // room send only -- unlike a quote, which also names a direct row -- and it
    // is judged in the same order: whether the key applies at all comes first,
    // so a roomless send hears the targeting refusal rather than a shape one.
    // The ids are then judged here (shape and cap, one judge shared with the
    // browser codec) and against the room's roster below, where the room is
    // known.
    let requested = req.mentions.unwrap_or_default();
    if !requested.is_empty() && req.room.is_none() {
        return Err(ApiError::bad_request(
            "mentions ride a room send: a participant id names a member of that room",
        ));
    }
    let mentions = checked_mentions(&requested)
        .map_err(|refusal| ApiError::bad_request(refusal.to_string()))?;
    // Exactly one target: the bilateral 1:1 (neither set), a joined room
    // (`room`), or a peer tagma's direct session (`tagma`). Both set is a
    // caller bug. Room and direct sends bypass the bilateral projector
    // entirely: no chat_history row, no bilateral frame, no bilateral emit --
    // the tagma posts the plaintext payload straight to the lesche envelope
    // route (relay surfaces and the 1:1 conversation are disjoint address
    // spaces, so the paths never share routing state).
    match (req.room, req.tagma) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "address exactly one target: room or tagma, not both",
            ));
        }
        (Some(room), None) => {
            if req.attachment.is_some() {
                return Err(ApiError::bad_request(
                    "attachments ride direct-session sends only; a room send is text",
                ));
            }
            return send_room_message(&state, room, req.text, quote, mentions)
                .await
                .map(|_| {
                    Json(DeliveryResponse {
                        ok: true,
                        error: None,
                    })
                });
        }
        (None, Some(peer)) => {
            return send_direct_message(&state, peer, req.text, req.attachment, quote)
                .await
                .map(|_| {
                    Json(DeliveryResponse {
                        ok: true,
                        error: None,
                    })
                });
        }
        (None, None) => {
            if req.attachment.is_some() {
                return Err(ApiError::bad_request(
                    "attachments ride direct-session sends only; the bilateral send is text",
                ));
            }
        }
    }
    // Persist once + publish via the single external projector. The projector
    // is the sole writer of chat content; whichever serving paths are active
    // (direct SSE and/or relay envelope) forward the published frame. Burst-
    // limited at the projector.
    let projector = state
        .external
        .get()
        .ok_or_else(|| ApiError::unavailable("external projector not initialized"))?;
    match projector.record_outbound(req.text).await {
        Ok(()) => Ok(Json(DeliveryResponse {
            ok: true,
            error: None,
        })),
        Err(RelayMessageError::BurstExceeded) => {
            Err(ApiError::too_many_requests("message burst cap exceeded"))
        }
        // The projector does not POST (the pumps do, and their failures are
        // logged there, not surfaced here); kept for exhaustiveness.
        Err(RelayMessageError::Delivery(e)) => {
            Err(ApiError::bad_gateway(format!("delivery failed: {e:#}")))
        }
    }
}

/// Send a message into a room (the `kallip lesche send --room <room>` path).
/// This is the outbound room counterpart of the bilateral `record_outbound`:
/// it shares the per-tagma burst cap (one agent voice = one rate limit), then
/// posts the plaintext `RoomMessage` to `/rooms/{room}/envelopes`. It
/// deliberately does NOT touch the bilateral projector -- no `chat_history` row
/// (lesche is the room's store of record), no bilateral frame, no `emit`.
/// (Named `send_` to distinguish it from the inbound
/// [`crate::delivery::deliver_inbound_relay_message`], which carries a room
/// message INTO the agent's prompt channel.)
///
/// `mentions` arrive canonical (the request guard ran the one judge); what is
/// left here is the room's own authority -- an id that names no member of
/// this room is a 400. The poll's roster answers first; anything short of a
/// confirmed member asks the lesche once (see [`judge_mentions`]).
pub(super) async fn send_room_message(
    state: &SharedState,
    room_str: String,
    text: String,
    quote: Option<i64>,
    mentions: Vec<MemberId>,
) -> Result<(), ApiError> {
    let room = RoomId::from(room_str);
    // Same rule as the read: the id reaches the lesche in a path segment, so
    // a dot segment is this request's own shape and answers 400.
    check_segment(room.as_ref()).map_err(|err| ApiError::bad_request(format!("room id: {err}")))?;
    // Shared burst cap with the bilateral path (the projector owns the limiter).
    let projector = state
        .external
        .get()
        .ok_or_else(|| ApiError::unavailable("external projector not initialized"))?;
    if !projector.check_outbound_burst().await {
        return Err(ApiError::too_many_requests("message burst cap exceeded"));
    }
    // Clone the relay client + agent sender via the room's owning relay (the
    // room id is unique to the lesche that created it, so ownership picks
    // the client; see the read route for the cold-cache caveat).
    let (client, sender, tagma_id, owner) = {
        let Some(owner) = state.joined_rooms.owner_of(&room).await else {
            return Err(ApiError::unavailable(
                "no online relay owns this room; cannot address a room",
            ));
        };
        let Some(handle) = state.relay(&owner) else {
            return Err(ApiError::unavailable(
                "relay not online; cannot address a room",
            ));
        };
        (
            handle.lesche_client(),
            handle.agent_sender(),
            handle.tagma_id().clone(),
            owner,
        )
    };
    // The room's own authority over its members, judged before anything is
    // posted: see `judge_mentions` for what a cached roster may be read as.
    judge_mentions(state, &client, &tagma_id, &owner, &room, &mentions).await?;
    // Room plaintext = a `RoomMessage` (rooms and the bilateral 1:1 path are
    // disjoint address spaces; a room message is text plus the optional
    // references below, no `req_id`/ack).
    // The lesche stores + relays it opaquely and member-gates the envelope
    // route, so the payload here is the plaintext itself.
    // The caller normalized `<= 0` to no quote (see `post_message`).
    let plain = serde_json::to_vec(&RoomMessage {
        text,
        attachment: None,
        quote,
        mentions,
    })
    .map_err(|e| ApiError::bad_gateway(format!("encode room message: {e:#}")))?;
    // post_room_envelope overwrites channel_id from `room`, so the value
    // set here is irrelevant; mirror the room id for clarity.
    let envelope = Envelope {
        channel_id: ChannelId::from(room.as_ref().to_string()),
        sender,
        // sequence_n is bilateral-replay-only (the AEAD nonce counter + the
        // archeion idempotency key); the rooms route does not consult it. A
        // constant 0 is harmless here.
        sequence_n: 0,
        trace_id: TraceId::random(),
        timestamp: OffsetDateTime::now_utc(),
        ciphertext: Ciphertext(plain),
    };
    client
        .post_room_envelope(&room, &envelope)
        .await
        .map_err(|e| map_lesche_error("room envelope post failed", &e))?;
    Ok(())
}

/// Judge a room send's mentions against the room's roster.
///
/// The roster is the poll's `list_my_rooms` snapshot -- the same answer that
/// told this route the room exists -- so confirming a member costs no call.
/// It is still a cache of the lesche's fact, not the fact: a slice can be
/// cold (the room absent from it) or behind a membership change the poll
/// has not seen yet. So every answer short of a confirmed member leaves
/// through one branch -- refresh once, then judge the roster that refresh
/// fetched -- so a refusal rests only on the answer the lesche handed back
/// in this request, never on a slice another writer may have replaced.
///
/// A refresh that leaves the roster unknown -- it failed, or its answer
/// does not cover the room -- refuses nobody: the lesche member-gates the
/// envelope route itself, and that is the authority this stands in for.
async fn judge_mentions(
    state: &SharedState,
    client: &kallipai_lesche_client::LescheClient,
    tagma_id: &TagmaId,
    relay: &str,
    room: &RoomId,
    mentions: &[MemberId],
) -> Result<(), ApiError> {
    if mentions.is_empty() {
        return Ok(());
    }
    if let Some(roster) = state.joined_rooms.members_of(relay, room).await
        && mentions.iter().all(|member| roster.contains(member))
    {
        return Ok(());
    }
    let Some(roster) = refresh_room_roster(state, client, tagma_id, relay, room).await else {
        return Ok(());
    };
    if let Some(missing) = mentions.iter().find(|member| !roster.contains(*member)) {
        return Err(ApiError::bad_request(format!(
            "mention {} is not a member of this room",
            missing.as_ref()
        )));
    }
    Ok(())
}

/// Ask the lesche for this tagma's rooms once, replace the addressed relay's
/// slice with the answer, and hand back the room's roster when the fresh
/// snapshot carried one. The one refresh helper [`judge_mentions`] calls.
async fn refresh_room_roster(
    state: &SharedState,
    client: &kallipai_lesche_client::LescheClient,
    tagma_id: &TagmaId,
    relay: &str,
    room: &RoomId,
) -> Option<std::collections::HashSet<MemberId>> {
    match client.list_my_rooms(tagma_id).await {
        Ok(rooms) => {
            // Judge with the answer in hand: reading it back out of the shared
            // slice would put an await boundary between the fetch and the judge,
            // where the poll or a second send can land a staler answer first.
            let fresh = rooms.iter().find(|view| &view.room_id == room).map(|view| {
                view.members
                    .iter()
                    .map(|member| member.id.clone())
                    .collect::<std::collections::HashSet<MemberId>>()
            });
            state.joined_rooms.set_room_snapshot(relay, &rooms).await;
            fresh
        }
        Err(e) => {
            tracing::warn!(
                room = %room, error = %e,
                "room send: roster refresh failed; judging nothing"
            );
            None
        }
    }
}

/// Send a message into the direct session with a peer tagma (the
/// `kallip lesche send --tagma <tagma-id>` path). Shares the per-tagma burst
/// cap with the bilateral + room paths, then create-or-gets the session on
/// the serving relay's lesche (a warm direct-session cache entry names it;
/// a cold entry -- a first-ever send or a not-yet-ticked poll -- runs the
/// create-or-get on the first installed relay, which a single-relay
/// deployment makes identical) and posts the plaintext `DirectMessage`
/// envelope to `/direct-sessions/{id}/messages`. Like the room path it
/// does NOT touch the bilateral projector -- no `chat_history` row (the
/// lesche is the session's store of record), no bilateral frame, no `emit`.
/// The quote, when the caller gave one, is a position in this session's own
/// log. A room send carries the same position as a `RoomMessage` field; the
/// `| quote=<seq>` segment a reader sees is the header's render of it.
pub(super) async fn send_direct_message(
    state: &SharedState,
    peer_str: String,
    text: String,
    attachment: Option<FileAttachment>,
    quote: Option<i64>,
) -> Result<(), ApiError> {
    // TagmaId parses leniently (an opaque id string); a peer the lesche does
    // not know fails the create-or-get with a member-gated 404, not a 400.
    let peer = TagmaId::from(peer_str);
    // Shared burst cap with the bilateral path (the projector owns the limiter).
    let projector = state
        .external
        .get()
        .ok_or_else(|| ApiError::unavailable("external projector not initialized"))?;
    if !projector.check_outbound_burst().await {
        return Err(ApiError::too_many_requests("message burst cap exceeded"));
    }
    // The session id is derived from this tagma's enrolled id + the peer
    // (the lesche-common derivation, the same bytes the lesche computes),
    // so both endpoints agree with zero lookup. Every installed relay
    // carries the same enrolled id, so any handle derives it.
    let handles = state.relay_handles();
    let Some(first) = handles.first() else {
        return Err(ApiError::unavailable(
            "no relay installed; cannot address a direct session",
        ));
    };
    let session = DirectSessionId::for_pair(first.tagma_id(), &peer);
    let handle = match state.direct_sessions.owner_of(&session).await {
        Some(owner) => state.relay(&owner).ok_or_else(|| {
            ApiError::unavailable("relay not online; cannot address a direct session")
        })?,
        None => first.clone(),
    };
    // create-or-get: idempotent on the lesche (the id derives from the
    // pair), so a double-initiation race is safe.
    handle
        .lesche_client()
        .create_direct_session(&peer)
        .await
        .map_err(|e| map_lesche_error("direct-session create failed", &e))?;
    // The direct payload is a `DirectMessage` (the lesche stores + relays
    // it opaquely and member-gates the route); the attachment, when set, is
    // a reference the peer resolves against the files service, and the
    // quote, when set, is a position in this session's own log.
    let plain = serde_json::to_vec(&DirectMessage {
        text,
        attachment,
        quote,
    })
    .map_err(|e| ApiError::bad_gateway(format!("encode direct message: {e:#}")))?;
    let envelope = Envelope {
        // post_direct_session_envelope overwrites channel_id from the
        // session id; mirror it for clarity, like the room send.
        channel_id: ChannelId::from(session.as_ref().to_string()),
        sender: handle.agent_sender(),
        sequence_n: 0,
        trace_id: TraceId::random(),
        timestamp: OffsetDateTime::now_utc(),
        ciphertext: Ciphertext(plain),
    };
    handle
        .lesche_client()
        .post_direct_session_envelope(&session, &envelope)
        .await
        .map_err(|e| map_lesche_error("direct-session envelope post failed", &e))?;
    Ok(())
}
