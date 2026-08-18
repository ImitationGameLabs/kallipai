//! `POST /agents/{id}/lesche/messages` — the root agent's "speak to the user"
//! primitive.
//!
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
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::ids::{ChannelId, TraceId};
use kallip_common::message::DeliveryResponse;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::message::{Envelope, RoomMessage};
use kallip_lesche_common::rooms::RoomId;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::relay::RelayMessageError;
use crate::state::SharedState;
use kallip_common::agentid::AgentId;

#[derive(Debug, Deserialize)]
pub(super) struct LescheMessageRequest {
    pub text: String,
    /// Optional room id. Present when the agent is sending into a multi-member
    /// room (the tagma posts the plaintext to `/v1/rooms/{room}/envelopes`);
    /// absent for the bilateral 1:1 send. A raw string parsed into a
    /// [`RoomId`] so this route owns the id-type boundary (the `kallip` CLI /
    /// `kallip-client` stay agora-id-free).
    pub room: Option<String>,
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
pub(super) struct RoomHistoryQuery {
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
    Query(q): Query<RoomHistoryQuery>,
) -> Result<String, ApiError> {
    require_root_self(&state, auth.identity(), &id).await?;
    let room = RoomId::from(room);
    let client = {
        let relay = state.relay.lock().unwrap_or_else(|e| e.into_inner());
        let Some((handle, _)) = relay.as_ref() else {
            return Err(ApiError::unavailable(
                "relay not online; cannot read room history",
            ));
        };
        handle.lesche_client()
    };
    let rows = client
        .fetch_room_messages(&room, q.after_seq, q.limit)
        .await
        .map_err(|e| map_room_error("room history fetch failed", &e))?;
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
        let RoomMessage { text } = request;
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
    // A room send bypasses the bilateral projector entirely: no chat_history
    // row, no bilateral frame, no bilateral emit. The tagma posts the plaintext
    // `RoomMessage` straight to the room envelope route. Rooms and the 1:1
    // conversation are disjoint address spaces, so the two paths never share
    // routing state.
    if let Some(room) = req.room {
        return send_room_message(&state, room, req.text).await.map(|_| {
            Json(DeliveryResponse {
                ok: true,
                error: None,
            })
        });
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
    // Clone the relay client + agent sender out of the std Mutex and drop the
    // guard before awaiting (the lock is sync and must not span an await).
    let (client, sender) = {
        let relay = state.relay.lock().unwrap_or_else(|e| e.into_inner());
        let Some((handle, _)) = relay.as_ref() else {
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
    let plain = serde_json::to_vec(&RoomMessage { text })
        .map_err(|e| ApiError::bad_gateway(format!("encode room message: {e:#}")))?;
    // post_room_envelope overwrites channel_id from `room`, so the value
    // set here is irrelevant; mirror the room id for clarity.
    let envelope = Envelope {
        channel_id: ChannelId::from(room.as_ref().to_string()),
        sender,
        // sequence_n is bilateral-replay-only (the AEAD nonce counter + the
        // agora idempotency key); the rooms route does not consult it. A
        // constant 0 is harmless here.
        sequence_n: 0,
        trace_id: TraceId::random(),
        timestamp: OffsetDateTime::now_utc(),
        ciphertext: Ciphertext(plain),
    };
    client
        .post_room_envelope(&room, &envelope)
        .await
        .map_err(|e| map_room_error("room envelope post failed", &e))?;
    Ok(())
}

/// Map a lesche-client room-surface error to a precise [`ApiError`]: a
/// member-gated 404 (unknown room / not a member) surfaces as `not_found`
/// rather than a misleading 502 bad-gateway; everything else stays a 502 (the
/// lesche itself is the transport in question).
fn map_room_error(context: &str, e: &anyhow::Error) -> ApiError {
    if let Some(err) = e.downcast_ref::<kallip_lesche_client::LescheHttpError>()
        && err.status == reqwest::StatusCode::NOT_FOUND
    {
        return ApiError::not_found(format!("{context}: unknown room or not a member"));
    }
    ApiError::bad_gateway(format!("{context}: {e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthIdentity, Identity};
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
}
