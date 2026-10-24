//! `POST /agents/{id}/lesche/messages` — the root agent's user-facing send
//! primitive: the bilateral 1:1 (default), a joined room (`room` set), or a
//! peer tagma's direct session (`tagma` set). This module also hosts the
//! room history read, the direct-session history read, and the unified
//! session list the `kallip lesche` subcommands drive.
//! The agent invokes `kallip lesche send` (a subcommand of the `kallip` CLI)
//! via `bash_exec`; it authenticates with its own per-agent token and POSTs
//! here. The tagma, holding the E2E key in-process, delivers the text as an
//! `AssistantContent` envelope over the relay.
//!
//! Root-only: the conversation with the user is owned by the single root
//! agent, so delivering a user-facing message is the root's job. A subagent
//! that tries is rejected (it must route outward communication through its
//! supervisor).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use kallipai_archeion_common::bytes::Ciphertext;
use kallipai_archeion_common::ids::{ChannelId, TagmaId, TraceId};
use kallipai_common::message::DeliveryResponse;
use kallipai_common::protocol::{ApiError, Modality};
use kallipai_lesche_common::direct::{
    DirectMessage, DirectMessageView, DirectSessionId, FileAttachment,
};
use kallipai_lesche_common::ids::check_segment;
use kallipai_lesche_common::message::{Envelope, Participant, RoomMessage, checked_mentions};
use kallipai_lesche_common::rooms::{MemberId, RoomId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::messaging::MentionView;
use crate::relay::RelayMessageError;
use crate::state::SharedState;
use kallipai_common::agentid::AgentId;

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

/// Self-only AND root-only guard shared by the lesche routes (the agent's
/// user-facing / room voice is owned by the single root agent; subagents route
/// outward communication through their supervisor, and the operator is not
/// allowed to forge the agent's voice).
async fn require_root_self(
    state: &SharedState,
    identity: &crate::auth::Identity,
    id: &AgentId,
) -> Result<(), ApiError> {
    let is_root = {
        let registry = state.registry.read().await;
        registry.require_self(identity, id)?;
        registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == id)
    };
    if !is_root {
        return Err(ApiError::forbidden(
            "delivering messages to the user requires the root agent; \
             subagents must route outward communication through their supervisor",
        ));
    }
    Ok(())
}

/// Read-side variant of [`require_root_self`] for the two lesche read
/// routes the console UI reaches through the daemon manage bridge (the
/// bridge authenticates as the operator): the operator may read the root
/// agent's lesche surfaces on the owner's behalf, but the write paths
/// stay [`require_root_self`]-guarded -- an operator posting as the
/// agent would forge the agent's voice.
async fn require_root_self_or_operator(
    state: &SharedState,
    identity: &crate::auth::Identity,
    id: &AgentId,
) -> Result<(), ApiError> {
    let is_root = {
        let registry = state.registry.read().await;
        registry.require_self_or_operator(identity, id)?;
        registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == id)
    };
    if !is_root {
        return Err(ApiError::forbidden(
            "reading lesche surfaces requires the root agent",
        ));
    }
    Ok(())
}

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

/// Refuse a position the log cannot have.
///
/// A position is an ordinal in a log, so a negative one is not a direction: the
/// lesche would read it as "from the start" and answer a page nobody asked for.
/// Both reads take the answer from here -- the room read on either cursor, the
/// direct read on the one cursor it has -- so the status and the sentence
/// cannot drift apart, and the refusal is this request's mistake rather than an
/// upstream fault (the lesche's own 400 would reach the caller as a 502).
fn refuse_negative_position(positions: &[Option<i64>]) -> Result<(), ApiError> {
    if positions.iter().flatten().any(|seq| *seq < 0) {
        return Err(ApiError::bad_request("a position cannot be negative"));
    }
    Ok(())
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

/// The text render of one room-history block: a bracketed header (this row's
/// `seq`, the sender's kind + id + sanitized handle, the row's timestamp) over
/// the body, one block per message.
///
/// The header is [`crate::messaging::block_header`]'s -- the same one the
/// direct-session render builds, so the two cannot drift. The sender is the
/// stored row's `Participant`; its advisory handle is sanitized there, which
/// keeps the rule the inbound prompt header applies at the render instead of
/// leaving it to each caller.
///
/// A row that answers another carries the answered position as a header segment
/// (`| quote=<seq>`): the same form and the same closing step as the inbound
/// prompt header (`messaging::close_header`), so a reader of `lesche read
/// --room` can tell which row a line replies to. The segment is the position
/// alone -- the quoted text stays where it is stored, and a reader that cannot
/// resolve the target renders absence rather than stale copy.
///
/// A row that names members carries the reader's own view of them as a further
/// segment (`| mentions=you`, `| mentions=<n>`, `| mentions=you+<n>`), behind
/// the quoted position: the same reading the live delivery renders, so the
/// agent sees one shape whether it watches a message arrive or reads it back.
fn render_room_block(
    seq: i64,
    sender: &Participant,
    at: OffsetDateTime,
    text: &str,
    quote: Option<i64>,
    mentions: Option<MentionView>,
) -> String {
    let header = crate::messaging::block_header(
        seq,
        sender.kind.as_str(),
        sender.id.as_ref(),
        &sender.handle,
        at,
        quote,
        mentions,
    );
    format!("{header}\n{text}\n\n")
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
async fn send_room_message(
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
async fn send_direct_message(
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

/// Query for `GET /agents/{id}/lesche/direct-sessions/{peer}/messages`: the
/// room history's paging fields plus `format=json` — the console UI's
/// structured variant. No `format` (any other value → 400) keeps the text
/// render, so the CLI contract does not move.
#[derive(Debug, Deserialize)]
pub(crate) struct DirectHistoryQuery {
    /// Return messages with `seq > after_seq` (exclusive). Default 0 = from
    /// the start.
    pub after_seq: Option<i64>,
    /// Max messages to return. Server-clamped by lesche.
    pub limit: Option<u64>,
    /// `json` switches the response from the bracketed text render to typed
    /// rows ([`DirectMessageJsonRow`]).
    pub format: Option<String>,
}

/// The json variant's default page (no `limit` query): small enough that a
/// full page fits the manage bridge's 256 KiB response cap with headroom,
/// large enough to spare a request per render. The text render keeps the
/// lesche's own default (the CLI contract does not move).
pub(crate) const DIRECT_JSON_DEFAULT_LIMIT: u64 = 25;

/// One row of the `format=json` direct history read (the console UI's
/// transcript contract): the decoded `DirectMessage` payload flattened to
/// the top level next to the row's envelope metadata. The handle is
/// advisory server-stamped data the client renders (and escapes); unlike
/// the text render there is no prompt-injection surface here.
#[derive(Debug, Serialize)]
struct DirectMessageJsonRow {
    pub seq: i64,
    pub sender: Participant,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<FileAttachment>,
    /// The position this row answers, in this session's own log; the field is
    /// absent when it answers nothing, and a present one is what the text
    /// render writes as its `| quote=` segment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<i64>,
    #[serde(with = "time::serde::iso8601")]
    pub created_at: OffsetDateTime,
}

/// `GET /agents/{id}/lesche/direct-sessions/{peer}/messages` — the
/// `kallip lesche read --tagma <peer>` path. The session id is derived
/// locally from (self, peer) with the lesche-common derivation, so the
/// agent never handles a session id: the peer is the tagma id that
/// `kallip lesche sessions` pairs with the session (each direct line reads
/// `direct <session-id> peer=<tagma-id> "<handle>"`), not the participant
/// id in the inbound header's parens. Served by the relay whose poll warmed
/// direct-session cache; a cold cache is indistinguishable from "no relay
/// knows this session" and surfaces as unavailable -- self-correcting on
/// the next poll, same as the room read.
pub async fn read_direct_session_messages(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, peer_str)): Path<(AgentId, String)>,
    Query(q): Query<DirectHistoryQuery>,
) -> Result<Response, ApiError> {
    require_root_self_or_operator(&state, auth.identity(), &id).await?;
    // The query's own shape is answered first: a malformed one is this
    // request's fault whether or not a relay happens to be installed, and a
    // missing relay would blame the wrong party.
    let json = direct_read_query(&q)?;
    // TagmaId parses leniently (an opaque id string); an unknown peer
    // surfaces from the lesche history read as a 404, not a 400.
    let peer = TagmaId::from(peer_str);
    let Some(first) = state.relay_handles().first().cloned() else {
        return Err(ApiError::unavailable(
            "no relay installed; cannot read a direct session",
        ));
    };
    let session = DirectSessionId::for_pair(first.tagma_id(), &peer);
    read_direct_session(&state, &session, q, json).await
}

/// `GET /agents/{id}/lesche/sessions/{session}/messages` -- the same read
/// addressed by the session id itself (`kallip lesche read --session <session>
/// --quote <seq>`).
///
/// The pair-addressed route derives the session from (self, peer); this one is
/// handed the id an inbound header already spells (`direct <session>`), which
/// is what a delivered message's read-back command names. Nothing is derived
/// and nothing is looked up, so that command cannot land on another
/// conversation.
pub async fn read_direct_session_messages_by_id(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, session)): Path<(AgentId, String)>,
    Query(q): Query<DirectHistoryQuery>,
) -> Result<Response, ApiError> {
    require_root_self_or_operator(&state, auth.identity(), &id).await?;
    // Same answer as the pair-addressed route: the query is the caller's own
    // to get wrong, before anything is looked up.
    let json = direct_read_query(&q)?;
    // The id was spelled by whoever wrote the command, and nothing here
    // derived it: a value that is not a canonical session id cannot name a
    // session, so it is refused as this request's mistake rather than carried
    // into a member-set lookup that could only say no to it.
    let Some(session) = DirectSessionId::parse(&session) else {
        return Err(ApiError::bad_request("not a canonical session id"));
    };
    read_direct_session(&state, &session, q, json).await
}

/// Whether the query asks for the json variant, and the refusals a query can
/// earn on its own.
///
/// Only `json` is accepted: a future format must not silently mean the text
/// render, and a negative position is refused by [`refuse_negative_position`].
/// Both entry points ask this before they resolve a relay, so a malformed query
/// is answered as such rather than as a relay that happens to be missing.
fn direct_read_query(q: &DirectHistoryQuery) -> Result<bool, ApiError> {
    refuse_negative_position(&[q.after_seq])?;
    match q.format.as_deref() {
        None => Ok(false),
        Some("json") => Ok(true),
        Some(other) => Err(ApiError::bad_request(format!(
            "unsupported format: {other} (only json)"
        ))),
    }
}

/// The shared body of both direct-session reads: the relay that owns the
/// session, one page from the lesche, and the render the query asks for.
async fn read_direct_session(
    state: &SharedState,
    session: &DirectSessionId,
    q: DirectHistoryQuery,
    json: bool,
) -> Result<Response, ApiError> {
    let client = {
        let Some(owner) = state.direct_sessions.owner_of(session).await else {
            return Err(ApiError::unavailable(
                "no online relay knows this direct session; cannot read its history",
            ));
        };
        let Some(handle) = state.relay(&owner) else {
            return Err(ApiError::unavailable(
                "relay not online; cannot read direct session history",
            ));
        };
        handle.lesche_client()
    };
    // The json variant pins a smaller default page (DIRECT_JSON_DEFAULT_LIMIT):
    // the console UI pulls conservatively because the manage bridge caps
    // responses at 256 KiB and drops an oversized page whole. An explicit
    // limit always wins.
    let limit = q
        .limit
        .or_else(|| json.then_some(DIRECT_JSON_DEFAULT_LIMIT));
    let rows = client
        .fetch_direct_messages(session, q.after_seq, limit)
        .await
        .map_err(|e| map_lesche_error("direct session history fetch failed", &e))?;
    if json {
        // Typed rows for the console UI; unparseable rows are skipped, never
        // fatal (same rule as the text render).
        let view: Vec<DirectMessageJsonRow> = rows
            .into_iter()
            .filter_map(|row| direct_json_row(session, row))
            .collect();
        Ok(Json(view).into_response())
    } else {
        Ok(render_direct_text(session, rows).into_response())
    }
}

/// The text render (no `format` query): the CLI/prompt contract, one
/// bracketed block per message, byte-compatible with the pre-json route.
///
/// The header is [`crate::messaging::block_header`]'s, with the answered
/// position when the row carries one -- the same `| quote=<seq>` segment a
/// room block renders. What differs from a room block is the body: a direct
/// row can carry an attachment, rendered as a pointer line under the text.
fn render_direct_text(session: &DirectSessionId, rows: Vec<DirectMessageView>) -> String {
    let mut out = String::new();
    for row in rows {
        let Some(DirectMessageJsonRow {
            seq,
            sender,
            text,
            attachment,
            quote,
            created_at,
        }) = direct_json_row(session, row)
        else {
            continue;
        };
        // An attachment renders as a bracketed pointer line under the
        // text: the pixels stay in the files service, the reader gets
        // the record id to drive the ingest with. The label follows the
        // declared modality; the pre-modality wire shape only carried
        // images.
        let text = match attachment {
            Some(att) => format!(
                "{text}\n[{} {}]",
                attachment_label(att.modality),
                att.record_id
            ),
            None => text,
        };
        let header = crate::messaging::block_header(
            seq,
            sender.kind.as_str(),
            sender.id.as_ref(),
            &sender.handle,
            created_at,
            quote,
            None,
        );
        out.push_str(&format!("{header}\n{text}\n\n"));
    }
    out
}

/// The bracketed-render label for an attachment: the declared modality in
/// lowercase. The pre-modality wire shape carried only images, so an
/// absent modality still renders as `image`.
fn attachment_label(modality: Option<Modality>) -> &'static str {
    match modality {
        Some(Modality::Image) | None => "image",
        Some(Modality::Audio) => "audio",
        Some(Modality::Video) => "video",
        Some(Modality::Text) => "text",
    }
}

#[cfg(test)]
mod render_label_tests {
    use super::*;
    use kallipai_archeion_common::ids::{ParticipantId, ParticipantKind, UserId};

    #[test]
    fn attachment_label_follows_the_declared_modality() {
        assert_eq!(attachment_label(Some(Modality::Image)), "image");
        assert_eq!(attachment_label(None), "image");
        assert_eq!(attachment_label(Some(Modality::Audio)), "audio");
        assert_eq!(attachment_label(Some(Modality::Video)), "video");
        assert_eq!(attachment_label(Some(Modality::Text)), "text");
    }

    /// A stored row's participant, as the read path hands it to the renderer.
    fn block_sender(kind: ParticipantKind, handle: &str) -> Participant {
        Participant {
            id: ParticipantId::for_user(&UserId::from("u-1".to_string())),
            kind,
            handle: handle.to_string(),
            tagma_id: None,
        }
    }

    /// The row timestamp the block tests render: 1_767_225_600 =
    /// 2026-01-01T00:00:00Z, in `time`'s `OffsetDateTime` Display form --
    /// the typed value the read paths pass, never a caller-built string.
    fn row_at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap()
    }

    #[test]
    fn room_block_carries_the_quoted_seq() {
        // The quoted position rides the header, after the timestamp, in the
        // same `| quote=<seq>` form the inbound prompt header uses.
        let sender = block_sender(ParticipantKind::Agent, "Alice");
        assert_eq!(
            render_room_block(3, &sender, row_at(), "hi", Some(7), None),
            format!(
                "[seq=3 from=agent:{} \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00 | quote=7]\nhi\n\n",
                sender.id.as_ref()
            )
        );
    }

    #[test]
    fn room_block_omits_an_absent_quote() {
        // Un-quoted rows keep the pre-quote block byte for byte: an absent or
        // non-positive position renders no segment (no room has a row 0).
        let sender = block_sender(ParticipantKind::Human, "");
        for quote in [None, Some(0), Some(-2)] {
            assert_eq!(
                render_room_block(3, &sender, row_at(), "hi", quote, None),
                format!(
                    "[seq=3 from=human:{} at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n",
                    sender.id.as_ref()
                )
            );
        }
    }

    #[test]
    fn room_block_states_the_readers_mentions_behind_the_quote() {
        // The history face of the reading the live header gives: named alone,
        // named with others, or not named at all.
        let sender = block_sender(ParticipantKind::Human, "Alice");
        let me = MemberId::for_tagma(&TagmaId::from("me-tagma".to_string()));
        let ada = MemberId::for_tagma(&TagmaId::from("ada-tagma".to_string()));
        let named_alone = MentionView::of(std::slice::from_ref(&me), &me);
        let named_with_ada = MentionView::of(&[me.clone(), ada], &me);
        let alone = render_room_block(3, &sender, row_at(), "hi", Some(7), named_alone);
        let with_ada = render_room_block(3, &sender, row_at(), "hi", Some(7), named_with_ada);
        let unnamed = render_room_block(3, &sender, row_at(), "hi", Some(7), None);
        assert!(
            alone.ends_with(" | quote=7 | mentions=you]\nhi\n\n"),
            "{alone}"
        );
        assert!(
            with_ada.ends_with(" | quote=7 | mentions=you+1]\nhi\n\n"),
            "{with_ada}"
        );
        assert!(unnamed.ends_with(" | quote=7]\nhi\n\n"), "{unnamed}");
    }

    #[test]
    fn room_block_keeps_the_handle_and_closes_without_a_segment() {
        // The other cell of the handle x quote grid: a handle present with
        // nothing answered still renders the wrapped handle, and the header
        // closes on the timestamp with no segment.
        let sender = block_sender(ParticipantKind::Human, "Alice");
        assert_eq!(
            render_room_block(3, &sender, row_at(), "hi", None, None),
            format!(
                "[seq=3 from=human:{} \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n",
                sender.id.as_ref()
            )
        );
    }

    /// One stored row, as the direct read path hands it to the renderer: the
    /// payload is the `DirectMessage` the row's ciphertext carries.
    fn direct_row(
        seq: i64,
        kind: ParticipantKind,
        handle: &str,
        text: &str,
        attachment: Option<FileAttachment>,
        quote: Option<i64>,
    ) -> DirectMessageView {
        let payload = DirectMessage {
            text: text.to_string(),
            attachment,
            quote,
        };
        DirectMessageView {
            seq,
            sender: block_sender(kind, handle),
            ciphertext: Ciphertext(serde_json::to_vec(&payload).unwrap()),
            created_at: row_at(),
        }
    }

    #[test]
    fn direct_block_renders_the_bracketed_header_and_the_attachment_pointer() {
        // The whole direct-session render, byte for byte: one bracketed header
        // (seq, sender kind + id, quoted handle, timestamp) per row over the
        // body, with an attachment rendered as a pointer line under the text.
        let session = DirectSessionId::from("sess-1".to_string());
        let attachment = FileAttachment {
            record_id: uuid::Uuid::nil(),
            name: "a.png".to_string(),
            size: 3,
            modality: Some(Modality::Image),
        };
        let rendered = render_direct_text(
            &session,
            vec![
                direct_row(4, ParticipantKind::Agent, "Alice", "hi", None, None),
                direct_row(
                    5,
                    ParticipantKind::Human,
                    "",
                    "look",
                    Some(attachment),
                    None,
                ),
            ],
        );
        // The timestamp column is the row `created_at` (1_767_225_600 =
        // 2026-01-01T00:00:00Z) in `time`'s `OffsetDateTime` Display form.
        assert_eq!(
            rendered,
            concat!(
                "[seq=4 from=agent:2163fda8-cf25-51c2-a6da-675a2120253b \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n",
                "[seq=5 from=human:2163fda8-cf25-51c2-a6da-675a2120253b at=2026-01-01 0:00:00.0 +00:00:00]\n",
                "look\n[image 00000000-0000-0000-0000-000000000000]\n\n"
            )
        );
    }

    #[test]
    fn a_crafted_sender_id_cannot_spell_a_room_block_segment() {
        // The id is interpolated into the header from the stored row, so it
        // goes through the same strip as the handle: what survives is a bare
        // word, not a field, and the only segment is the one the renderer
        // placed just before the bracket.
        let mut crafted = block_sender(ParticipantKind::Agent, "Alice");
        crafted.id = ParticipantId::from("p-1| quote=999]".to_string());
        assert_eq!(
            render_room_block(3, &crafted, row_at(), "hi", Some(7), None),
            "[seq=3 from=agent:p-1 quote=999 \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00 | quote=7]\nhi\n\n"
        );
    }

    #[test]
    fn a_crafted_sender_id_cannot_close_a_direct_block_header() {
        // Same strip on the direct block: a crafted id cannot close the header
        // early nor open a field, and a plain id renders verbatim (see
        // `direct_block_renders_the_bracketed_header_and_the_attachment_pointer`).
        let session = DirectSessionId::from("sess-1".to_string());
        let mut row = direct_row(4, ParticipantKind::Agent, "Alice", "hi", None, None);
        row.sender.id = ParticipantId::from("p-1] [system".to_string());
        assert_eq!(
            render_direct_text(&session, vec![row]),
            "[seq=4 from=agent:p-1 system \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n"
        );
    }

    #[test]
    fn a_direct_block_carries_the_quoted_seq() {
        // The direct render reads the answered position off the stored payload
        // and writes it into the header, in the `| quote=<seq>` form the room
        // render uses; a row that answers nothing gains no segment. Whole
        // strings, so the segment's place in the header is pinned too.
        let session = DirectSessionId::from("sess-1".to_string());
        let row = direct_row(4, ParticipantKind::Agent, "Alice", "hi", None, Some(7));
        let sender_id = row.sender.id.as_ref().to_string();
        assert_eq!(
            render_direct_text(&session, vec![row]),
            format!(
                "[seq=4 from=agent:{} \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00 | quote=7]\nhi\n\n",
                sender_id
            )
        );
        // An absent or non-positive position renders the pre-quote block.
        for quote in [None, Some(0), Some(-2)] {
            let row = direct_row(4, ParticipantKind::Agent, "Alice", "hi", None, quote);
            let sender_id = row.sender.id.as_ref().to_string();
            assert_eq!(
                render_direct_text(&session, vec![row]),
                format!(
                    "[seq=4 from=agent:{} \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n",
                    sender_id
                )
            );
        }
    }

    #[test]
    fn the_json_row_carries_the_position_the_text_render_shows() {
        // The two faces read one normalized value: `direct_json_row` drops a
        // non-positive position before either face sees it, so the text
        // render's missing segment and the json view's missing key are the
        // same fact, and a positive position travels as the same number.
        let session = DirectSessionId::from("sess-1".to_string());
        for (quote, expected) in [
            (Some(1), Some(1)),
            (Some(7), Some(7)),
            (Some(0), None),
            (Some(-2), None),
            (None, None),
        ] {
            let row = direct_json_row(
                &session,
                direct_row(4, ParticipantKind::Agent, "Alice", "hi", None, quote),
            )
            .expect("a readable row");
            let value = serde_json::to_value(&row).expect("json row");
            match expected {
                Some(seq) => assert_eq!(
                    value["quote"],
                    serde_json::json!(seq),
                    "{quote:?} keeps its position"
                ),
                None => assert!(value.get("quote").is_none(), "{quote:?} leaves the key out"),
            }
        }
    }
}

/// Flatten one stored row into the json view: the decoded `DirectMessage`
/// payload at the top level (text, the optional attachment, and the answered
/// position) next to the row's envelope metadata, so the browser never
/// handles the ciphertext wrapper.
/// `None` for a row whose payload no longer parses (skipped, never fatal).
fn direct_json_row(
    session: &DirectSessionId,
    row: DirectMessageView,
) -> Option<DirectMessageJsonRow> {
    let request: DirectMessage = match serde_json::from_slice(&row.ciphertext.0) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(session = %session, seq = row.seq, "direct history parse skipped: {e}");
            return None;
        }
    };
    let DirectMessage {
        text,
        attachment,
        quote,
    } = request;
    // A non-positive position is no position: the text render drops it, so
    // the json view normalizes it here and both faces read one value.
    let quote = quote.filter(|q| *q > 0);
    Some(DirectMessageJsonRow {
        seq: row.seq,
        sender: row.sender,
        text,
        attachment,
        quote,
        created_at: row.created_at,
    })
}

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

/// Map a lesche-client error to a precise [`ApiError`]: a member-gated 404
/// (unknown room/session / not a member) surfaces as `not_found` rather than
/// a misleading 502 bad-gateway; everything else stays a 502 (the lesche
/// itself is the transport in question).
fn map_lesche_error(context: &str, e: &anyhow::Error) -> ApiError {
    if let Some(err) = e.downcast_ref::<kallipai_lesche_client::LescheHttpError>()
        && err.status == reqwest::StatusCode::NOT_FOUND
    {
        return ApiError::not_found(format!("{context}: unknown target or not a member"));
    }
    ApiError::bad_gateway(format!("{context}: {e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthIdentity, Identity};
    use crate::external::ExternalProjector;
    use crate::relay::MessageLimits;
    use crate::state::RegistryEntry;
    use crate::test_helpers::{make_entry, make_state};
    use axum::Json;
    use axum::extract::{Path, State};
    use kallipai_common::agentid::AgentId;

    /// With neither the relay nor the direct serving path initialized (a state
    /// only constructed in tests — production always inits direct), the route
    /// returns 503 rather than touching either serving path. Authed as the
    /// agent itself (self-only) so the 403 check does not short-circuit.
    #[tokio::test]
    async fn message_unavailable_when_no_relay() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: None,
                tagma: None,
                attachment: None,
                quote: None,
                mentions: None,
            }),
        )
        .await
        .expect_err("no relay -> unavailable");
        assert_eq!(
            err.status, 503,
            "expected 503 unavailable, got {}",
            err.status
        );
    }

    /// A room send (`--room <room>`) takes the room branch, which needs the
    /// external projector (for the shared burst cap) -- absent here, so it
    /// short-circuits to 503. Proves the branch is taken and routes through
    /// `send_room_message` rather than the bilateral projector path.
    #[tokio::test]
    async fn room_send_unavailable_without_projector() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: Some("room-1".into()),
                tagma: None,
                attachment: None,
                quote: None,
                mentions: None,
            }),
        )
        .await
        .expect_err("no projector -> unavailable");
        assert_eq!(
            err.status, 503,
            "expected 503 unavailable, got {}",
            err.status
        );
    }

    /// `list_joined_rooms` with an empty joined-rooms cache returns an empty
    /// list rather than 503: the room list reads the cache directly and needs no
    /// other state. Pins that the route is wired through `require_root_self` and
    /// tolerates a cold cache.
    #[tokio::test]
    async fn list_joined_rooms_with_cold_cache_returns_empty() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let Json(rooms) = list_joined_rooms(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
        )
        .await
        .expect("cold cache -> empty list, not 503");
        assert!(rooms.is_empty());
    }

    /// `list_joined_rooms` rejects a subagent (the shared `require_root_self`
    /// guard). The guard now serves three routes; this pins that the extraction
    /// did not regress the 403 path for the room-discovery route.
    #[tokio::test]
    async fn list_joined_rooms_forbidden_for_subagent() {
        let state = make_state();
        let root = AgentId::random();
        let sub = AgentId::random();
        {
            let mut registry = state.registry.write().await;
            registry.register(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "root".into())),
            );
            registry.register(
                sub.clone(),
                RegistryEntry::Live(make_entry(Some(root), "sub".into())),
            );
        }

        let err = list_joined_rooms(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: sub.clone() }),
            Path(sub),
        )
        .await
        .expect_err("subagent -> forbidden");
        assert_eq!(err.status, 403);
    }

    /// The operator may NOT send as an agent — a message is something the end
    /// user attributes to the agent, so an operator posting one would forge the
    /// agent's voice. This is the deliberate narrowing from
    /// `require_self_or_operator` (used by non-impersonating self-writes) to
    /// `require_self`.
    #[tokio::test]
    async fn message_forbidden_for_operator() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(id),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: None,
                tagma: None,
                attachment: None,
                quote: None,
                mentions: None,
            }),
        )
        .await
        .expect_err("operator -> forbidden");
        assert_eq!(
            err.status, 403,
            "expected 403 forbidden, got {}",
            err.status
        );
    }

    /// A peer agent may not send for another agent either (self-only).
    #[tokio::test]
    async fn message_forbidden_for_other_agent() {
        let state = make_state();
        let a = AgentId::random();
        let b = AgentId::random();
        let entry_a = make_entry(None, "a".to_string());
        let entry_b = make_entry(Some(a.clone()), "b".to_string());
        {
            let mut registry = state.registry.write().await;
            registry.register(a.clone(), RegistryEntry::Live(entry_a));
            registry.register(b.clone(), RegistryEntry::Live(entry_b));
        }

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: a }),
            Path(b),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: None,
                tagma: None,
                attachment: None,
                quote: None,
                mentions: None,
            }),
        )
        .await
        .expect_err("peer agent -> forbidden");
        assert_eq!(
            err.status, 403,
            "expected 403 forbidden, got {}",
            err.status
        );
    }

    /// A subagent may not deliver a user-facing message even when posting as
    /// itself (self-only passes) — the conversation with the user is owned by
    /// the root, so a subagent must route outward communication through its
    /// supervisor.
    #[tokio::test]
    async fn message_forbidden_for_subagent() {
        let state = make_state();
        let root = AgentId::random();
        let sub = AgentId::random();
        {
            let mut registry = state.registry.write().await;
            registry.register(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "root".into())),
            );
            registry.register(
                sub.clone(),
                RegistryEntry::Live(make_entry(Some(root), "sub".into())),
            );
        }

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: sub.clone() }),
            Path(sub),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: None,
                tagma: None,
                attachment: None,
                quote: None,
                mentions: None,
            }),
        )
        .await
        .expect_err("subagent -> forbidden");
        assert_eq!(
            err.status, 403,
            "expected 403 forbidden, got {}",
            err.status
        );
    }

    /// A send naming BOTH targets (room + tagma) is a caller bug -> 400,
    /// before any projector/relay work. Pins the three-addressing dispatch
    /// invariant that the two optional targets are mutually exclusive.
    #[tokio::test]
    async fn direct_send_rejects_both_targets() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: Some("room-1".into()),
                tagma: Some("tagma-b".into()),
                attachment: None,
                quote: None,
                mentions: None,
            }),
        )
        .await
        .expect_err("both targets -> bad request");
        assert_eq!(err.status, 400, "expected 400, got {}", err.status);
    }

    /// A direct send takes the tagma branch before any relay work: without
    /// the external projector (the shared burst-cap owner) it short-circuits
    /// to 503, mirroring the room branch. Pins that `--tagma` routes through
    /// `send_direct_message` rather than the bilateral projector path.
    #[tokio::test]
    async fn direct_send_unavailable_without_projector() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: None,
                tagma: Some("tagma-b".into()),
                attachment: None,
                quote: None,
                mentions: None,
            }),
        )
        .await
        .expect_err("no projector -> unavailable");
        assert_eq!(
            err.status, 503,
            "expected 503 unavailable, got {}",
            err.status
        );
    }

    /// The direct-session history read with no relay installed is 503 -- the
    /// derivation needs the enrolled tagma id, which only a relay carries.
    #[tokio::test]
    async fn direct_read_unavailable_without_relay() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = read_direct_session_messages(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, "tagma-b".to_string())),
            Query(DirectHistoryQuery {
                after_seq: None,
                limit: None,
                format: None,
            }),
        )
        .await
        .expect_err("no relay -> unavailable");
        assert_eq!(
            err.status, 503,
            "expected 503 unavailable, got {}",
            err.status
        );
    }

    /// The format is the query's own to get wrong, so a malformed one is 400
    /// even with no relay installed: a 503 would blame a party that has not
    /// been reached yet.
    #[tokio::test]
    async fn a_bad_format_is_refused_before_the_relay_lookup() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = read_direct_session_messages(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, "tagma-b".to_string())),
            Query(DirectHistoryQuery {
                after_seq: None,
                limit: None,
                format: Some("xml".to_string()),
            }),
        )
        .await
        .expect_err("a format the route does not serve is 400");

        assert_eq!(
            err.status, 400,
            "a bad format is the request's fault, not a missing relay: {}",
            err.status
        );
    }

    /// The session-addressed route answers the same way: which route spelled
    /// the log does not change who got the format wrong.
    #[tokio::test]
    async fn a_bad_format_is_refused_on_the_session_addressed_read_too() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = read_direct_session_messages_by_id(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, "00000000-0000-0000-0000-0000000000a1".to_string())),
            Query(DirectHistoryQuery {
                after_seq: None,
                limit: None,
                format: Some("xml".to_string()),
            }),
        )
        .await
        .expect_err("a format the route does not serve is 400");

        assert_eq!(err.status, 400, "bad request, not a missing relay");
    }

    /// The session-addressed read is handed an id somebody spelled, so its
    /// shape is this request's own: a value that cannot name a session is
    /// refused as a bad request rather than carried to a member-set lookup
    /// that could only say no to it.
    #[tokio::test]
    async fn a_session_id_that_is_not_canonical_is_refused() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = read_direct_session_messages_by_id(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, "../../agents".to_string())),
            Query(DirectHistoryQuery {
                after_seq: None,
                limit: None,
                format: None,
            }),
        )
        .await
        .expect_err("a value that cannot name a session is this request's mistake");

        assert_eq!(err.status, 400, "bad request, not an unavailable relay");
    }

    /// A position is an ordinal in the log, so a negative one is no direction
    /// at all. Both reads answer it here, and the room query is refused on
    /// either cursor: the lesche would read a negative one as "from the start",
    /// and this hop would surface that answer as an upstream fault.
    #[tokio::test]
    async fn a_negative_position_is_refused_by_both_reads() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let direct = read_direct_session_messages(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id.clone(), "tagma-b".to_string())),
            Query(DirectHistoryQuery {
                after_seq: Some(-1),
                limit: None,
                format: None,
            }),
        )
        .await
        .expect_err("a negative position is not a page direction");
        assert_eq!(direct.status, 400, "bad request, not an unavailable relay");

        for query in [
            LescheHistoryQuery {
                after_seq: Some(-1),
                before_seq: None,
                limit: None,
            },
            LescheHistoryQuery {
                after_seq: None,
                before_seq: Some(-1),
                limit: None,
            },
        ] {
            let room = read_room_messages(
                State(state.clone()),
                AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
                Path((
                    id.clone(),
                    "00000000-0000-0000-0000-000000000aa1".to_string(),
                )),
                Query(query),
            )
            .await
            .expect_err("a negative position is not a page direction");
            assert_eq!(room.status, 400, "bad request, not an unavailable relay");
        }
    }

    /// The unified session list with no relays and no projector returns an
    /// empty list rather than 503: every member of the aggregate is
    /// best-effort, mirroring the cold-cache room list.
    #[tokio::test]
    async fn sessions_with_cold_state_returns_empty() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let Json(entries) = list_lesche_sessions(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
        )
        .await
        .expect("cold state -> empty list, not 503");
        assert!(entries.is_empty());
    }

    /// The console reads the session list through the manage bridge, which
    /// authenticates as the operator: the read routes accept the operator
    /// identity (read-side narrowing of `require_root_self`), so a cold
    /// state yields the same empty list it yields for the agent.
    #[tokio::test]
    async fn sessions_readable_by_operator() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let Json(entries) = list_lesche_sessions(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(id),
        )
        .await
        .expect("operator reads the session list");
        assert!(entries.is_empty());
    }

    /// The operator passes the direct-history read guard and reaches the
    /// relay-dependent leg: with no relay installed the route fails with
    /// 503 (unavailable), NOT 403 -- pinning that the guard relaxation
    /// covers the bridge identity the console actually presents.
    #[tokio::test]
    async fn direct_read_passes_guard_for_operator() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = read_direct_session_messages(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path((id, "tagma-b".to_string())),
            Query(DirectHistoryQuery {
                after_seq: None,
                limit: None,
                format: None,
            }),
        )
        .await
        .expect_err("no relay -> 503, guard passed");
        assert_eq!(
            err.status, 503,
            "expected 503 unavailable, got {}",
            err.status
        );
    }

    /// The direct-history read hands the data plane the caller's own cursor and
    /// page size, both of them: the route renders whatever the lesche answers,
    /// so a dropped `limit` would print a page nobody asked for.
    #[tokio::test]
    async fn direct_read_passes_its_cursor_and_page_size_on() {
        let (state, id, mock) = setup_lesche_send(Vec::new()).await;
        let own = TagmaId::from("t1".to_string());
        let peer = TagmaId::from("tagma-peer".to_string());
        state
            .direct_sessions
            .set_direct_sessions("test", [DirectSessionId::for_pair(&own, &peer)])
            .await;

        let _page = read_direct_session_messages(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, "tagma-peer".to_string())),
            axum::extract::Query(DirectHistoryQuery {
                after_seq: Some(41),
                limit: Some(1),
                format: None,
            }),
        )
        .await
        .expect("the read reaches the lesche");

        let reads = mock.direct_reads.lock().unwrap().clone();
        assert_eq!(reads.len(), 1, "one history GET");
        let query = reads[0].as_str();
        assert!(query.contains("after_seq=41"), "{query}");
        assert!(query.contains("limit=1"), "{query}");
    }

    /// The session-addressed read fetches the session the path names: no peer
    /// pair is derived, so the log a delivered message's read-back command
    /// spells is the log this read asks the lesche for.
    #[tokio::test]
    async fn a_session_addressed_read_fetches_that_session() {
        let (state, id, mock) = setup_lesche_send(Vec::new()).await;
        let own = TagmaId::from("t1".to_string());
        let peer = TagmaId::from("tagma-peer".to_string());
        let session = DirectSessionId::for_pair(&own, &peer);
        state
            .direct_sessions
            .set_direct_sessions("test", [session.clone()])
            .await;

        let _page = read_direct_session_messages_by_id(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, session.to_string())),
            axum::extract::Query(DirectHistoryQuery {
                after_seq: Some(6),
                limit: Some(1),
                format: None,
            }),
        )
        .await
        .expect("the read reaches the lesche");

        let reads = mock.direct_reads.lock().unwrap().clone();
        assert_eq!(reads.len(), 1, "one history GET");
        let read = reads[0].as_str();
        assert!(
            read.contains(&format!("/direct-sessions/{session}/messages")),
            "the path names the session itself: {read}"
        );
        assert!(
            read.contains("after_seq=6"),
            "the cursor rides along: {read}"
        );
        assert!(
            read.contains("limit=1"),
            "the page size rides along: {read}"
        );
    }

    /// The direct-send branch shares the per-tagma burst cap with the
    /// bilateral path: a projector configured with a zero-size budget makes
    /// the very first `--tagma` send a 429 before any relay is contacted
    /// (the create-or-get never runs). Pins the shared-cap wiring the
    /// direct branch relies on.
    #[tokio::test]
    async fn direct_send_hits_the_burst_cap_before_any_relay_contact() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));
        state
            .external
            .set(ExternalProjector::new(
                std::sync::Arc::downgrade(&state),
                None,
                None,
                None,
                None,
                MessageLimits {
                    max: 0,
                    window: std::time::Duration::from_secs(60),
                },
            ))
            .ok()
            .expect("projector set once");

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: None,
                tagma: Some("tagma-peer".into()),
                attachment: None,
                quote: None,
                mentions: None,
            }),
        )
        .await
        .expect_err("max:0 budget -> 429");
        assert_eq!(err.status, 429, "expected 429, got {}", err.status);
    }

    /// Every envelope body the stub lesche received, in arrival order.
    type PostedBodies = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

    /// The query string of every direct-history read the stub lesche answered.
    type ReadQueries = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// The room message inside one posted envelope: what actually crossed the
    /// wire, once the envelope's plaintext payload is decoded.
    fn wire_room_message(body: &[u8]) -> RoomMessage {
        let envelope: Envelope = serde_json::from_slice(body).expect("envelope json");
        serde_json::from_slice(&envelope.ciphertext.0).expect("room message json")
    }

    /// A room send reaches the lesche with the quote on the wire, and a
    /// non-positive quote reaches it with no quote field at all.
    #[tokio::test]
    async fn room_send_carries_the_quote_to_the_lesche() {
        let (state, id, mock) = setup_lesche_send(Vec::new()).await;
        state
            .joined_rooms
            .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
            .await;

        assert!(
            send_shaped(
                state.clone(),
                id.clone(),
                Some("room-1"),
                None,
                Vec::new(),
                Some(7),
            )
            .await
            .expect("quoted room send")
            .0
            .ok
        );
        assert!(
            send_shaped(state, id, Some("room-1"), None, Vec::new(), Some(0))
                .await
                .expect("zero-quote room send")
                .0
                .ok
        );

        let bodies = mock.posted.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "one envelope per send");
        let quoted = wire_room_message(&bodies[0]);
        assert_eq!(quoted.quote, Some(7), "the quote crosses the wire");
        assert_eq!(quoted.text, "ping");
        let zero = wire_room_message(&bodies[1]);
        assert_eq!(zero.quote, None, "a zero quote is no quote on the wire");
    }

    /// A direct-session send reaches the lesche with the quote on the wire, and
    /// an unquoted or non-positive one keeps the payload shape a pre-quote
    /// writer wrote.
    #[tokio::test]
    async fn direct_send_carries_the_quote_to_the_lesche() {
        let (state, id, mock) = setup_lesche_send(Vec::new()).await;

        assert!(
            send_shaped(
                state.clone(),
                id.clone(),
                None,
                Some("tagma-peer"),
                Vec::new(),
                Some(7),
            )
            .await
            .expect("quoted direct send")
            .0
            .ok
        );
        assert!(
            send_shaped(
                state.clone(),
                id.clone(),
                None,
                Some("tagma-peer"),
                Vec::new(),
                None
            )
            .await
            .expect("plain direct send")
            .0
            .ok
        );
        assert!(
            send_shaped(state, id, None, Some("tagma-peer"), Vec::new(), Some(0))
                .await
                .expect("zero-quote direct send")
                .0
                .ok
        );

        let bodies = mock.direct_posted.lock().unwrap().clone();
        assert_eq!(bodies.len(), 3, "one envelope per send");
        let quoted: Envelope = serde_json::from_slice(&bodies[0]).expect("envelope json");
        let payload: DirectMessage =
            serde_json::from_slice(&quoted.ciphertext.0).expect("direct message json");
        assert_eq!(payload.quote, Some(7), "the quote crosses the wire");
        assert_eq!(payload.text, "ping");
        let plain: Envelope = serde_json::from_slice(&bodies[1]).expect("envelope json");
        assert_eq!(
            plain.ciphertext.0, br#"{"text":"ping"}"#,
            "an unquoted direct payload keeps its pre-quote bytes"
        );
        let zero: Envelope = serde_json::from_slice(&bodies[2]).expect("envelope json");
        assert_eq!(
            zero.ciphertext.0, br#"{"text":"ping"}"#,
            "a zero quote is no quote on the wire"
        );
    }

    /// How many times the mock lesche was asked for the tagma's room list.
    type RosterPulls = std::sync::Arc<std::sync::atomic::AtomicUsize>;

    /// The mock lesche's shared state: the room and direct-session envelopes it
    /// received, the query each direct-history read carried, the room list it
    /// serves, how often it was asked for that list, and the status it answers
    /// that list with (zero means 200, so a test that never sets it gets a
    /// served roster; 500 makes the refresh fail).
    #[derive(Clone, Default)]
    struct LescheSendMock {
        posted: PostedBodies,
        direct_posted: PostedBodies,
        direct_reads: ReadQueries,
        pulls: RosterPulls,
        roster: std::sync::Arc<std::sync::Mutex<Vec<kallipai_lesche_common::rooms::TagmaRoomView>>>,
        roster_status: std::sync::Arc<std::sync::atomic::AtomicU16>,
    }

    /// A stand-in lesche serving the room-envelope route, the direct-session
    /// create-or-get and message routes, and the tagma's own room list: a send
    /// test can read what crossed the wire and tell a cached roster from a
    /// refreshed one.
    async fn spawn_lesche_send_stub(mock: LescheSendMock) -> String {
        use std::sync::atomic::Ordering;

        let app = axum::Router::new()
            .route(
                "/v1/lesche/rooms/{_room}/envelopes",
                axum::routing::post(
                    |State(mock): State<LescheSendMock>, body: axum::body::Bytes| async move {
                        mock.posted.lock().unwrap().push(body.to_vec());
                        axum::http::StatusCode::ACCEPTED
                    },
                ),
            )
            .route(
                "/v1/lesche/direct-sessions",
                axum::routing::post(|State(_mock): State<LescheSendMock>| async move {
                    Json(serde_json::json!({
                        "session_id": "00000000-0000-4000-8000-0000000000dd",
                        "peer": { "tagma_id": "tagma-peer", "handle": "peer@alice" },
                        "created_at": "2026-01-01T00:00:00Z",
                    }))
                }),
            )
            .route(
                "/v1/lesche/direct-sessions/{_session}/messages",
                axum::routing::get(
                    |State(mock): State<LescheSendMock>, uri: axum::http::Uri| async move {
                        // The path is recorded with the query: a read is
                        // addressed by the session in its path, so a test that
                        // only saw the query could not tell the two direct-read
                        // routes apart.
                        let recorded = uri
                            .path_and_query()
                            .map(|p| p.as_str().to_string())
                            .unwrap_or_default();
                        mock.direct_reads.lock().unwrap().push(recorded);
                        Json(Vec::<serde_json::Value>::new())
                    },
                )
                .post(
                    |State(mock): State<LescheSendMock>, body: axum::body::Bytes| async move {
                        mock.direct_posted.lock().unwrap().push(body.to_vec());
                        axum::http::StatusCode::ACCEPTED
                    },
                ),
            )
            .route(
                "/v1/lesche/tagmata/{_tagma}/rooms",
                axum::routing::get(|State(mock): State<LescheSendMock>| async move {
                    mock.pulls.fetch_add(1, Ordering::SeqCst);
                    let served = mock.roster_status.load(Ordering::SeqCst);
                    let code = axum::http::StatusCode::from_u16(served)
                        .unwrap_or(axum::http::StatusCode::OK);
                    let rooms = if code == axum::http::StatusCode::OK {
                        mock.roster.lock().unwrap().clone()
                    } else {
                        Vec::new()
                    };
                    (code, Json(rooms))
                }),
            )
            .with_state(mock);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/v1/lesche")
    }

    /// A state wired to one live relay and the mock lesche, with the calling
    /// agent registered (the send route is root-self). The room cache starts
    /// cold: a test seeds it with ids or with a snapshot as its case needs.
    async fn setup_lesche_send(
        roster: Vec<kallipai_lesche_common::rooms::TagmaRoomView>,
    ) -> (SharedState, AgentId, LescheSendMock) {
        let mock = LescheSendMock::default();
        *mock.roster.lock().unwrap() = roster;
        let lesche_url = spawn_lesche_send_stub(mock.clone()).await;
        let state = make_state();
        let id = AgentId::random();
        state.registry.write().await.register(
            id.clone(),
            RegistryEntry::Live(make_entry(None, "tok".into())),
        );
        let projector = ExternalProjector::new(
            std::sync::Arc::downgrade(&state),
            None,
            None,
            None,
            None,
            MessageLimits::default(),
        );
        assert!(state.external.set(projector).is_ok(), "projector installed");
        let client = kallipai_lesche_client::LescheClient::builder(&lesche_url, "tok")
            .build()
            .expect("lesche client");
        let handle = crate::relay::RelayHandle::new(
            client,
            "test".to_string(),
            TagmaId::from("t1".to_string()),
            "Tagma".into(),
            kallipai_e2ee::DeviceKey::generate(),
            id.clone(),
            std::sync::Arc::downgrade(&state),
        );
        state.set_relay("test", handle, tokio::spawn(async {}));
        (state, id, mock)
    }

    /// One send through the route, as the CLI's `lesche send` shapes it.
    async fn send_with(
        state: SharedState,
        id: AgentId,
        room: Option<&str>,
        tagma: Option<&str>,
        mentions: Vec<String>,
    ) -> Result<Json<DeliveryResponse>, ApiError> {
        send_shaped(state, id, room, tagma, mentions, None).await
    }

    /// One send with every field the route reads spelled out: `send_with` for
    /// the cases that vary only the target and the names, this one for a case
    /// that also carries a quote.
    async fn send_shaped(
        state: SharedState,
        id: AgentId,
        room: Option<&str>,
        tagma: Option<&str>,
        mentions: Vec<String>,
        quote: Option<i64>,
    ) -> Result<Json<DeliveryResponse>, ApiError> {
        post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
            Json(LescheMessageRequest {
                text: "ping".into(),
                room: room.map(str::to_owned),
                tagma: tagma.map(str::to_owned),
                attachment: None,
                quote,
                mentions: Some(mentions),
            }),
        )
        .await
    }

    fn member_id(name: &str) -> MemberId {
        MemberId::for_tagma(&kallipai_archeion_common::ids::TagmaId::from(
            name.to_string(),
        ))
    }

    fn room_view_with(
        room: &str,
        members: Vec<MemberId>,
    ) -> kallipai_lesche_common::rooms::TagmaRoomView {
        use kallipai_archeion_common::ids::ParticipantKind;
        use kallipai_lesche_common::rooms::{RoomMember, TagmaRoomView, Visibility};
        TagmaRoomView {
            room_id: RoomId::from(room.to_string()),
            members: members
                .into_iter()
                .map(|id| RoomMember {
                    id,
                    kind: ParticipantKind::Agent,
                })
                .collect(),
            membership_epoch: 1,
            is_creator: false,
            visibility: Visibility::Private,
            name: None,
        }
    }

    /// Mentions reach the lesche in the sender's order with repeats folded, an
    /// unnamed send carries none, and a cached roster answers from the cache.
    #[tokio::test]
    async fn room_send_carries_mentions_to_the_lesche() {
        use std::sync::atomic::Ordering;

        let ada = member_id("ada");
        let bo = member_id("bo");
        let view = room_view_with("room-1", vec![ada.clone(), bo.clone()]);
        let (state, id, mock) = setup_lesche_send(vec![view.clone()]).await;
        state.joined_rooms.set_room_snapshot("test", &[view]).await;

        let named = vec![
            ada.as_ref().to_owned(),
            bo.as_ref().to_owned(),
            ada.as_ref().to_owned(),
        ];
        assert!(
            send_with(state.clone(), id.clone(), Some("room-1"), None, named)
                .await
                .expect("named room send")
                .0
                .ok
        );
        assert!(
            send_with(state.clone(), id.clone(), Some("room-1"), None, Vec::new())
                .await
                .expect("unnamed room send")
                .0
                .ok
        );

        let bodies = mock.posted.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "one envelope per send");
        assert_eq!(
            wire_room_message(&bodies[0]).mentions,
            vec![ada.clone(), bo.clone()],
            "the members cross the wire in order, the repeat folded"
        );
        assert!(
            wire_room_message(&bodies[1]).mentions.is_empty(),
            "a send that named nobody names nobody on the wire"
        );
        assert_eq!(
            mock.pulls.load(Ordering::SeqCst),
            0,
            "a cached roster costs no call to the lesche"
        );
    }

    /// A mention that is not a participant id is refused with the text the
    /// caller typed, before the send reaches any room.
    #[tokio::test]
    async fn a_malformed_mention_is_refused() {
        let (state, id, _mock) = setup_lesche_send(Vec::new()).await;
        let err = send_with(
            state,
            id,
            Some("room-1"),
            None,
            vec!["not-an-id".to_string()],
        )
        .await
        .expect_err("a malformed id names nobody");
        assert_eq!(err.status, 400);
        assert_eq!(err.message, "not a participant id: not-an-id");
    }

    /// The cap counts members after repeats fold: seventeen distinct ids are
    /// refused, and the refusal states the cap.
    #[tokio::test]
    async fn naming_more_members_than_the_cap_is_refused() {
        let (state, id, _mock) = setup_lesche_send(Vec::new()).await;
        let named: Vec<String> = (0..17)
            .map(|n| member_id(&format!("member-{n}")).as_ref().to_owned())
            .collect();
        let err = send_with(state, id, Some("room-1"), None, named)
            .await
            .expect_err("seventeen members is one over the cap");
        assert_eq!(err.status, 400);
        assert_eq!(err.message, "a send names more than 16 members");
    }

    /// A mention names a member of the room the send enters, so a send at
    /// another target is refused rather than silently dropping the names.
    #[tokio::test]
    async fn a_mention_without_a_room_is_refused() {
        let (state, id, _mock) = setup_lesche_send(Vec::new()).await;
        let err = send_with(
            state,
            id,
            None,
            Some("tagma-peer"),
            vec![member_id("ada").as_ref().to_owned()],
        )
        .await
        .expect_err("a direct-session send names no room");
        assert_eq!(err.status, 400);
        assert_eq!(
            err.message,
            "mentions ride a room send: a participant id names a member of that room"
        );
    }

    /// A member the fresh roster does not name is refused, and the refusal
    /// names the id: the cached slice only sends the judge to the refresh, so
    /// a refusal never rests on what that slice said.
    #[tokio::test]
    async fn a_mention_of_a_non_member_is_refused() {
        use std::sync::atomic::Ordering;

        let ada = member_id("ada");
        let bo = member_id("bo");
        let view = room_view_with("room-1", vec![ada.clone()]);
        let (state, id, mock) = setup_lesche_send(vec![view.clone()]).await;
        state.joined_rooms.set_room_snapshot("test", &[view]).await;

        let err = send_with(
            state,
            id,
            Some("room-1"),
            None,
            vec![bo.as_ref().to_owned()],
        )
        .await
        .expect_err("bo is not in this room");
        assert_eq!(err.status, 400);
        assert_eq!(
            err.message,
            format!("mention {} is not a member of this room", bo.as_ref())
        );
        assert_eq!(
            mock.posted.lock().unwrap().len(),
            0,
            "a refused mention sends nothing"
        );
        assert_eq!(
            mock.pulls.load(Ordering::SeqCst),
            1,
            "a slice that lacks the id sends the judge to the refresh"
        );
    }

    /// A roster the poll has not cached is refreshed once, and the refreshed
    /// roster is what the next send is judged against: a member it names goes
    /// through, one it does not is refused.
    #[tokio::test]
    async fn a_cold_roster_is_refreshed_before_a_mention_is_judged() {
        use std::sync::atomic::Ordering;

        let ada = member_id("ada");
        let bo = member_id("bo");
        let (state, id, mock) =
            setup_lesche_send(vec![room_view_with("room-1", vec![ada.clone()])]).await;
        state
            .joined_rooms
            .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
            .await;

        assert!(
            send_with(
                state.clone(),
                id.clone(),
                Some("room-1"),
                None,
                vec![ada.as_ref().to_owned()],
            )
            .await
            .expect("the refreshed roster names the member")
            .0
            .ok
        );
        assert_eq!(
            mock.pulls.load(Ordering::SeqCst),
            1,
            "a cold roster is refreshed once"
        );

        assert!(
            send_with(
                state.clone(),
                id.clone(),
                Some("room-1"),
                None,
                vec![ada.as_ref().to_owned()],
            )
            .await
            .expect("the refreshed roster is cached")
            .0
            .ok
        );
        assert_eq!(
            mock.pulls.load(Ordering::SeqCst),
            1,
            "the refresh warmed the cache for the next send"
        );

        let err = send_with(
            state,
            id,
            Some("room-1"),
            None,
            vec![bo.as_ref().to_owned()],
        )
        .await
        .expect_err("the refreshed roster does not name bo");
        assert_eq!(err.status, 400);
        assert_eq!(
            err.message,
            format!("mention {} is not a member of this room", bo.as_ref())
        );
    }

    /// A cached roster the poll has not caught up with does not refuse a
    /// member the lesche names: the refresh answers, and the send goes through.
    #[tokio::test]
    async fn a_member_the_fresh_roster_names_is_let_through() {
        use std::sync::atomic::Ordering;

        let ada = member_id("ada");
        let bo = member_id("bo");
        let stale = room_view_with("room-1", vec![ada.clone()]);
        let fresh = room_view_with("room-1", vec![ada, bo.clone()]);
        let (state, id, mock) = setup_lesche_send(vec![fresh]).await;
        state.joined_rooms.set_room_snapshot("test", &[stale]).await;

        assert!(
            send_with(
                state,
                id,
                Some("room-1"),
                None,
                vec![bo.as_ref().to_owned()],
            )
            .await
            .expect("the fresh roster names the member")
            .0
            .ok
        );
        assert_eq!(
            mock.pulls.load(Ordering::SeqCst),
            1,
            "the cached slice sends the judge to the refresh"
        );
        assert_eq!(
            mock.posted.lock().unwrap().len(),
            1,
            "the send reached the lesche"
        );
    }

    /// A refresh that fails leaves the roster unknown, and an unknown roster
    /// refuses nobody: the send goes through and the lesche gates the envelope
    /// route itself. The entry below is built by the test seam -- a production
    /// reader meets this branch as a stale slice, the case next door.
    #[tokio::test]
    async fn a_failed_refresh_judges_no_mention() {
        use std::sync::atomic::Ordering;

        let (state, id, mock) = setup_lesche_send(Vec::new()).await;
        mock.roster_status.store(500, Ordering::SeqCst);
        state
            .joined_rooms
            .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
            .await;

        assert!(
            send_with(
                state,
                id,
                Some("room-1"),
                None,
                vec![member_id("ada").as_ref().to_owned()],
            )
            .await
            .expect("an unconfirmed roster refuses nobody")
            .0
            .ok
        );
        assert_eq!(
            mock.pulls.load(Ordering::SeqCst),
            1,
            "a rosterless cache asks the lesche once"
        );
        assert_eq!(
            mock.posted.lock().unwrap().len(),
            1,
            "the send reached the lesche"
        );
    }

    /// A stale slice and a failed refresh leave who is in the room
    /// unconfirmed, so the send goes through after that one refresh,
    /// never refused on a slice this call could not confirm.
    #[tokio::test]
    async fn a_stale_roster_and_a_failed_refresh_let_the_send_through() {
        use std::sync::atomic::Ordering;

        let ada = member_id("ada");
        let bo = member_id("bo");
        let (state, id, mock) =
            setup_lesche_send(vec![room_view_with("room-1", vec![ada.clone()])]).await;
        state
            .joined_rooms
            .set_room_snapshot("test", &[room_view_with("room-1", vec![ada])])
            .await;
        mock.roster_status.store(500, Ordering::SeqCst);

        assert!(
            send_with(
                state,
                id,
                Some("room-1"),
                None,
                vec![bo.as_ref().to_owned()],
            )
            .await
            .expect("an unconfirmed roster refuses nobody")
            .0
            .ok
        );
        assert_eq!(
            mock.pulls.load(Ordering::SeqCst),
            1,
            "a stale slice asks the lesche once"
        );
        assert_eq!(
            mock.posted.lock().unwrap().len(),
            1,
            "the send reached the lesche"
        );
    }

    /// A roomless send is judged for its target before its ids: the refusal
    /// says the key rides a room send, not that an id is malformed.
    #[tokio::test]
    async fn a_roomless_send_hears_the_targeting_refusal_first() {
        let (state, id, _mock) = setup_lesche_send(Vec::new()).await;
        let err = send_with(
            state,
            id,
            None,
            Some("tagma-peer"),
            vec!["not-an-id".to_string()],
        )
        .await
        .expect_err("a direct-session send names no room");
        assert_eq!(err.status, 400);
        assert_eq!(
            err.message,
            "mentions ride a room send: a participant id names a member of that room"
        );
    }

    /// The field's three absences read the same: an omitted key, an explicit
    /// null, and an empty list all mean this send names nobody.
    #[test]
    fn an_absent_null_or_empty_mention_list_names_nobody() {
        let parse = |body: &str| -> LescheMessageRequest {
            serde_json::from_str(body).expect("request json")
        };
        assert!(
            parse(r#"{"text":"ping","room":"room-1"}"#)
                .mentions
                .is_none()
        );
        assert!(
            parse(r#"{"text":"ping","room":"room-1","mentions":null}"#)
                .mentions
                .is_none(),
            "an explicit null names nobody"
        );
        assert_eq!(
            parse(r#"{"text":"ping","room":"room-1","mentions":[]}"#).mentions,
            Some(Vec::new())
        );
    }

    /// A stand-in lesche serving the room-history route: answers a fixed page
    /// and keeps each request's query string, so a test can read what was asked.
    type HistoryQueries = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    async fn spawn_room_history_stub() -> (String, HistoryQueries) {
        spawn_room_history_stub_serving(serde_json::json!([])).await
    }

    /// The same stub, serving a page of rows, so a test can drive the render
    /// path with stored rows instead of an empty page.
    async fn spawn_room_history_stub_serving(body: serde_json::Value) -> (String, HistoryQueries) {
        let seen: HistoryQueries = Default::default();
        let recorded = seen.clone();
        let app = axum::Router::new().route(
            "/v1/lesche/rooms/{_room}/messages",
            axum::routing::get(move |uri: axum::http::Uri| {
                let recorded = recorded.clone();
                let body = body.clone();
                async move {
                    recorded
                        .lock()
                        .unwrap()
                        .push(uri.query().unwrap_or("").to_string());
                    Json(body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/v1/lesche"), seen)
    }

    /// Read one stored room row through the route with this tagma as the
    /// reader, and hand back the rendered block body. The row is a human peer's
    /// (`Alice`), at a fixed position and timestamp, so a caller states only the
    /// payload it wants the header to read.
    async fn read_one_room_row(text: &str, quote: Option<i64>, mentions: Vec<MemberId>) -> String {
        use base64::Engine as _;
        let payload = serde_json::to_vec(&RoomMessage {
            text: text.into(),
            attachment: None,
            quote,
            mentions,
        })
        .expect("room message json");
        let page = serde_json::json!([{
            "seq": 3,
            "sender": {"id": "p-user-1", "kind": "human", "handle": "Alice"},
            "epoch": 1,
            "ciphertext": base64::engine::general_purpose::STANDARD.encode(&payload),
            "created_at": "2026-08-02T00:00:00Z",
        }]);
        let (lesche_url, _asked) = spawn_room_history_stub_serving(page).await;
        let state = make_state();
        let id = AgentId::random();
        state.registry.write().await.register(
            id.clone(),
            RegistryEntry::Live(make_entry(None, "tok".into())),
        );
        let client = kallipai_lesche_client::LescheClient::builder(&lesche_url, "tok")
            .build()
            .expect("lesche client");
        let handle = crate::relay::RelayHandle::new(
            client,
            "test".to_string(),
            TagmaId::from("t1".to_string()),
            "Tagma".into(),
            kallipai_e2ee::DeviceKey::generate(),
            id.clone(),
            std::sync::Arc::downgrade(&state),
        );
        state.set_relay("test", handle, tokio::spawn(async {}));
        state
            .joined_rooms
            .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
            .await;

        read_room_messages(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, "room-1".to_string())),
            Query(LescheHistoryQuery {
                after_seq: None,
                before_seq: None,
                limit: None,
            }),
        )
        .await
        .expect("a stored row renders")
    }

    /// Both cursors name a direction, so a request carrying both is rejected at
    /// this hop in the lesche's own words -- not forwarded and surfaced as an
    /// upstream 502, which reads as a retryable fault rather than a caller bug.
    #[tokio::test]
    async fn read_room_rejects_both_cursors_at_this_hop() {
        let state = make_state();
        let id = AgentId::random();
        state.registry.write().await.register(
            id.clone(),
            RegistryEntry::Live(make_entry(None, "tok".into())),
        );

        let err = read_room_messages(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, "room-1".to_string())),
            Query(LescheHistoryQuery {
                after_seq: Some(2),
                before_seq: Some(8),
                limit: None,
            }),
        )
        .await
        .expect_err("both cursors -> 400");
        assert_eq!(err.status, 400, "expected 400, got {}", err.status);
        assert_eq!(
            err.message,
            "after_seq and before_seq are mutually exclusive"
        );
    }

    /// The answer both room seams give a dot-segment id: the shape
    /// sentence a caller reads, kept in one place so the pins cannot
    /// drift.
    const DOT_SEGMENT_ANSWER: &str = "room id: an id cannot be a dot segment";

    /// A dot segment survives percent-encoding, so the room read
    /// refuses it at this hop: left to the client, the value would
    /// answer this hop's 503 (no relay owns it) rather than name the
    /// request's own shape.
    #[tokio::test]
    async fn read_room_rejects_a_dot_segment_at_this_hop() {
        let state = make_state();
        let id = AgentId::random();
        state.registry.write().await.register(
            id.clone(),
            RegistryEntry::Live(make_entry(None, "tok".into())),
        );

        let err = read_room_messages(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, "..".to_string())),
            Query(LescheHistoryQuery {
                after_seq: None,
                before_seq: None,
                limit: None,
            }),
        )
        .await
        .expect_err("a dot segment -> 400");
        assert_eq!(err.status, 400, "expected 400, got {}", err.status);
        assert_eq!(err.message, DOT_SEGMENT_ANSWER);
    }

    /// The room send judges the same value the read does, before any relay is
    /// resolved: a dot segment is this request's shape, not a backup path.
    #[tokio::test]
    async fn send_room_rejects_a_dot_segment_at_this_hop() {
        let state = make_state();

        let err = send_room_message(&state, "..".to_string(), "hello".into(), None, Vec::new())
            .await
            .expect_err("a dot segment -> 400");
        assert_eq!(err.status, 400, "expected 400, got {}", err.status);
        assert_eq!(err.message, DOT_SEGMENT_ANSWER);
    }

    /// The backward cursor crosses this hop: the lesche is asked for the page
    /// below it, one row deep, and for nothing else.
    #[tokio::test]
    async fn read_room_forwards_the_backward_cursor() {
        let (lesche_url, asked) = spawn_room_history_stub().await;
        let state = make_state();
        let id = AgentId::random();
        state.registry.write().await.register(
            id.clone(),
            RegistryEntry::Live(make_entry(None, "tok".into())),
        );
        let client = kallipai_lesche_client::LescheClient::builder(&lesche_url, "tok")
            .build()
            .expect("lesche client");
        let handle = crate::relay::RelayHandle::new(
            client,
            "test".to_string(),
            TagmaId::from("t1".to_string()),
            "Tagma".into(),
            kallipai_e2ee::DeviceKey::generate(),
            id.clone(),
            std::sync::Arc::downgrade(&state),
        );
        state.set_relay("test", handle, tokio::spawn(async {}));
        state
            .joined_rooms
            .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
            .await;

        let body = read_room_messages(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id, "room-1".to_string())),
            Query(LescheHistoryQuery {
                after_seq: None,
                before_seq: Some(8),
                limit: Some(1),
            }),
        )
        .await
        .expect("an empty page renders as nothing");
        assert!(body.is_empty(), "empty page: {body}");

        let asked = asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec!["before_seq=8&limit=1".to_string()],
            "one backward request, and no forward cursor on it"
        );
    }

    /// The quoted seq and the reader's own mention cross the whole read path:
    /// both sit inside the row's payload, and the rendered block puts them last
    /// in the header, the mention behind the quoted position, immediately before
    /// the closing bracket. The reader is the owning relay handle's tagma id,
    /// so a row naming it reads `you`.
    #[tokio::test]
    async fn read_room_renders_the_quoted_seq_and_the_readers_mention() {
        let body = read_one_room_row(
            "answering that",
            Some(7),
            vec![MemberId::for_tagma(&TagmaId::from("t1".to_string()))],
        )
        .await;

        // The rendered timestamp of the row the helper stores, spelled as the
        // block spells it.
        assert_eq!(
            body,
            "[seq=3 from=human:p-user-1 \"Alice\" at=2026-08-02 0:00:00.0 +00:00:00 | quote=7 | mentions=you]\nanswering that\n\n"
        );
    }

    /// A row that names other members and not this tagma reads as their count:
    /// the same viewer the live path takes, so the block states the reader's own
    /// view rather than anything the room's roster holds.
    #[tokio::test]
    async fn read_room_counts_the_other_members_a_stored_row_names() {
        let body = read_one_room_row(
            "for ada",
            None,
            vec![MemberId::for_tagma(&TagmaId::from("ada".to_string()))],
        )
        .await;

        assert_eq!(
            body,
            "[seq=3 from=human:p-user-1 \"Alice\" at=2026-08-02 0:00:00.0 +00:00:00 | mentions=1]\nfor ada\n\n"
        );
    }

    /// A quote names a position in a conversation's own log, so the bilateral
    /// 1:1 -- which has none -- answers a send carrying one with 400.
    #[tokio::test]
    async fn quote_on_a_bilateral_send_is_bad_request() {
        let state = make_state();
        let id = AgentId::random();
        state.registry.write().await.register(
            id.clone(),
            RegistryEntry::Live(make_entry(None, "tok".into())),
        );

        let err = post_message(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id.clone()),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: None,
                tagma: None,
                attachment: None,
                quote: Some(3),
                mentions: None,
            }),
        )
        .await
        .expect_err("a quote needs a target with a log");
        assert_eq!(err.status, 400, "expected 400, got {}", err.status);

        // The same send with a non-positive quote reads as no quote at all and
        // falls through to the bilateral path (unavailable in this harness).
        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room: None,
                tagma: None,
                attachment: None,
                quote: Some(0),
                mentions: None,
            }),
        )
        .await
        .expect_err("no projector in this harness");
        assert_eq!(err.status, 503, "expected 503, got {}", err.status);
    }
}
