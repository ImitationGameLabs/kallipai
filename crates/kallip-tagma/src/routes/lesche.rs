//! `POST /agents/{id}/lesche/messages` — the root agent's user-facing send
//! primitive: the bilateral 1:1 (default), a joined room (`room` set), or a
//! peer tagma's direct session (`tagma` set). This module also hosts the
//! room history read, the direct-session history read, and the unified
//! session list the `kallip lesche` subcommands drive.
//! The agent invokes `kallip lesche send` (a subcommand of the `kallip` CLI)
//! via `bash_exec`; it authenticates with its own per-agent token and POSTs
//! here. The tagma, holding the E2E key in-process, delivers the text as an
//! `AssistantContent` envelope over the relay. This replaces the former
//! standalone connector's unix-socket reply path.
//!
//! Root-only: the conversation with the user is owned by the single root
//! agent, so delivering a user-facing message is the root's job. A subagent
//! that tries is rejected (it must route outward communication through its
//! supervisor).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use kallip_archeion_common::bytes::Ciphertext;
use kallip_archeion_common::ids::{ChannelId, TagmaId, TraceId};
use kallip_common::message::DeliveryResponse;
use kallip_common::protocol::{ApiError, Modality};
use kallip_lesche_common::direct::{
    DirectMessage, DirectMessageView, DirectSessionId, FileAttachment,
};
use kallip_lesche_common::message::{Envelope, Participant, RoomMessage};
use kallip_lesche_common::rooms::RoomId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::relay::RelayMessageError;
use crate::state::SharedState;
use kallip_common::agentid::AgentId;

#[derive(Debug, Deserialize)]
pub(crate) struct LescheMessageRequest {
    pub text: String,
    /// Optional room id. Present when the agent is sending into a multi-member
    /// room (the tagma posts the plaintext to `/v1/rooms/{room}/envelopes`);
    /// absent for the bilateral 1:1 send. A raw string parsed into a
    /// [`RoomId`] so this route owns the id-type boundary (the `kallip` CLI /
    /// `kallip-client` stay archeion-id-free).
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
/// `[seq=<n> from=<kind>:<id>[ "<handle>"] at=<iso8601>]\n<text>\n\n` where
/// `kind` is `agent` or `human` (a room may have user-device members). Rows that
/// fail to parse are skipped, so one bad row never blanks the read.
pub async fn read_room_messages(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, room)): Path<(AgentId, String)>,
    Query(q): Query<LescheHistoryQuery>,
) -> Result<String, ApiError> {
    require_root_self(&state, auth.identity(), &id).await?;
    let room = RoomId::from(room);
    // Route via room ownership: each relay's poll keys its slice of the
    // joined-rooms cache by entry name, and a room id is unique to the lesche
    // that created it, so the owning relay's client serves the read. A cold
    // cache (no poll has warmed yet) is indistinguishable from "no relay
    // online" and surfaces as unavailable, same as before — self-correcting
    // on the next poll.
    let client = {
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
        handle.lesche_client()
    };
    let rows = client
        .fetch_room_messages(&room, q.after_seq, q.limit)
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
        let RoomMessage { text, .. } = request;
        let sender_id = row.sender.id.as_ref().to_string();
        // The advisory `Participant` carries the kind + handle. The kind is
        // relay-authenticated transitively (credential type); the handle is
        // spoofable + sanitized before interpolation -- same rule as the inbound
        // `format_room_incoming`. A user-device room member renders with its
        // kind label + sanitized handle (not skipped, as when rooms were
        // agent-to-agent only).
        let kind = row.sender.kind.as_str();
        let handle = crate::messaging::sanitize_handle(&row.sender.handle);
        let handle_part = if handle.is_empty() {
            String::new()
        } else {
            format!(" \"{handle}\"")
        };
        out.push_str(&format!(
            "[seq={} from={}:{}{} at={}]\n{text}\n\n",
            row.seq, kind, sender_id, handle_part, row.created_at
        ));
    }
    Ok(out)
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
            return send_room_message(&state, room, req.text).await.map(|_| {
                Json(DeliveryResponse {
                    ok: true,
                    error: None,
                })
            });
        }
        (None, Some(peer)) => {
            return send_direct_message(&state, peer, req.text, req.attachment)
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
/// posts the plaintext `RoomMessage` to `/v1/rooms/{room}/envelopes`. It
/// deliberately does NOT touch the bilateral projector -- no `chat_history` row
/// (lesche is the room's store of record), no bilateral frame, no `emit`.
/// (Named `send_` to distinguish it from the inbound
/// `deliver_inbound_room_message` in `routes/message.rs`, which carries a room
/// message INTO the agent's prompt channel.)
async fn send_room_message(
    state: &SharedState,
    room_str: String,
    text: String,
) -> Result<(), ApiError> {
    let room = RoomId::from(room_str);
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
    let (client, sender) = {
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
        (handle.lesche_client(), handle.agent_sender())
    };
    // Room plaintext = a `RoomMessage` (rooms and the bilateral 1:1 path are
    // disjoint address spaces; a room message is just text, no `req_id`/ack).
    // The lesche stores + relays it opaquely and member-gates the envelope
    // route, so the payload here is the plaintext itself.
    let plain = serde_json::to_vec(&RoomMessage {
        text,
        attachment: None,
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

/// Send a message into the direct session with a peer tagma (the
/// `kallip lesche send --tagma <tagma-id>` path). Shares the per-tagma burst
/// cap with the bilateral + room paths, then create-or-gets the session on
/// the serving relay's lesche (a warm direct-session cache entry names it;
/// a cold entry -- a first-ever send or a not-yet-ticked poll -- runs the
/// create-or-get on the first installed relay, which a single-relay
/// deployment makes identical) and posts the plaintext `DirectMessage`
/// envelope to `/v1/direct-sessions/{id}/messages`. Like the room path it
/// does NOT touch the bilateral projector -- no `chat_history` row (the
/// lesche is the session's store of record), no bilateral frame, no `emit`.
async fn send_direct_message(
    state: &SharedState,
    peer_str: String,
    text: String,
    attachment: Option<FileAttachment>,
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
    // a reference the peer resolves against the files service.
    let plain = serde_json::to_vec(&DirectMessage { text, attachment })
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
    #[serde(with = "time::serde::iso8601")]
    pub created_at: OffsetDateTime,
}

/// `GET /agents/{id}/lesche/direct-sessions/{peer}/messages` — the
/// `kallip lesche read --tagma <peer>` path. The session id is derived
/// locally from (self, peer) with the lesche-common derivation, so the
/// agent never handles a session id: the peer is the tagma id copied from
/// the inbound `[From: ... | direct <session>]` header's parens (or from
/// `kallip lesche sessions`). Served by the relay whose poll warmed the
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
    // Only `json` is accepted: a future format must not silently mean the
    // text render.
    let json = match q.format.as_deref() {
        None => false,
        Some("json") => true,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unsupported format: {other} (only json)"
            )));
        }
    };
    // TagmaId parses leniently (an opaque id string); an unknown peer
    // surfaces from the lesche history read as a 404, not a 400.
    let peer = TagmaId::from(peer_str);
    let Some(first) = state.relay_handles().first().cloned() else {
        return Err(ApiError::unavailable(
            "no relay installed; cannot read a direct session",
        ));
    };
    let session = DirectSessionId::for_pair(first.tagma_id(), &peer);
    let client = {
        let Some(owner) = state.direct_sessions.owner_of(&session).await else {
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
        .fetch_direct_messages(&session, q.after_seq, limit)
        .await
        .map_err(|e| map_lesche_error("direct session history fetch failed", &e))?;
    if json {
        // Typed rows for the console UI; unparseable rows are skipped, never
        // fatal (same rule as the text render).
        let view: Vec<DirectMessageJsonRow> = rows
            .into_iter()
            .filter_map(|row| direct_json_row(&session, row))
            .collect();
        Ok(Json(view).into_response())
    } else {
        Ok(render_direct_text(&session, rows).into_response())
    }
}

/// The text render (no `format` query): the CLI/prompt contract, one
/// bracketed block per message, byte-compatible with the pre-json route.
fn render_direct_text(session: &DirectSessionId, rows: Vec<DirectMessageView>) -> String {
    let mut out = String::new();
    for row in rows {
        let Some(DirectMessageJsonRow {
            seq,
            sender,
            text,
            attachment,
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
        let sender_id = sender.id.as_ref().to_string();
        let kind = sender.kind.as_str();
        let clean = crate::messaging::sanitize_handle(&sender.handle);
        let handle_part = if clean.is_empty() {
            String::new()
        } else {
            format!(" \"{clean}\"")
        };
        out.push_str(&format!(
            "[seq={} from={}:{}{} at={}]\n{text}\n\n",
            seq, kind, sender_id, handle_part, created_at
        ));
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

    #[test]
    fn attachment_label_follows_the_declared_modality() {
        assert_eq!(attachment_label(Some(Modality::Image)), "image");
        assert_eq!(attachment_label(None), "image");
        assert_eq!(attachment_label(Some(Modality::Audio)), "audio");
        assert_eq!(attachment_label(Some(Modality::Video)), "video");
        assert_eq!(attachment_label(Some(Modality::Text)), "text");
    }
}

/// Flatten one stored row into the json view: the decoded `DirectMessage`
/// payload at the top level (text + optional attachment) next to the row's
/// envelope metadata, so the browser never handles the ciphertext wrapper.
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
    let DirectMessage { text, attachment } = request;
    Some(DirectMessageJsonRow {
        seq: row.seq,
        sender: row.sender,
        text,
        attachment,
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
    if let Some(err) = e.downcast_ref::<kallip_lesche_client::LescheHttpError>()
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
    use kallip_common::agentid::AgentId;

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
            }),
        )
        .await
        .expect_err("max:0 budget -> 429");
        assert_eq!(err.status, 429, "expected 429, got {}", err.status);
    }
}
