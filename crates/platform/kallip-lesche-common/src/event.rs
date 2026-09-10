//! Events on the app's multiplexed SSE stream (`GET /v1/me/events`), plus the
//! signal vocabulary the tagma pushes to the relay for plaintext rebroadcast.
//!
//! A single per-user connection carries envelope deliveries for all of the
//! user's conversations plus presence transitions, system signals, and
//! aggregate status snapshots, multiplexed by `conversation_id` / `tagma_id`.
//!
//! Key exchange is NOT delivered here: it is a synchronous request/reply on
//! `POST /v1/conversations/{id}/key-exchange/init`, whose response body carries
//! the tagma's signed key-exchange response directly.
//!
//! The presence variants (`TagmaOnline`, `TagmaOffline`) are emitted by the
//! data-plane relay (`kallip-lesche`) on the app event stream when a
//! tagma tunnel connects/disconnects (and as a snapshot when the stream
//! opens). `TagmaStatus` carries a tagma's live aggregate runtime state
//! (agent counts + token budget); like presence it is plaintext and
//! user-scoped, pushed by the tagma on a periodic snapshot and rebroadcast
//! by the lesche. `TagmaSystem` carries a tagma's per-event runtime signal
//! (busy/idle presence, turn terminals, errors) — plaintext like the others,
//! because these are operator metadata, not conversation content (authored
//! content rides the encrypted envelope as a `TagmaReply::Event`).
//!
//! [`AuthoredEvent`] and [`SignalEvent`] are the *public, agent-free* event
//! vocabulary the tagma produces (by projecting its internal `SseEvent` stream)
//! and the app consumes. They are re-exported from `kallip_common::protocol`
//! (the transport-neutral home); this crate re-exports them so downstream
//! crates can name them without a second `use` path.

use crate::message::Envelope;
use crate::rooms::{MemberId, RoomId};
use kallip_archeion_common::ids::TagmaId;
use serde::{Deserialize, Serialize};

// Re-exported so downstream crates (e.g. `kallip-lesche-client`) can name the
// external vocabulary through this module without a direct `kallip_common`
// path, and brought into scope for this module's own use.
pub use kallip_common::protocol::{AgentState, AuthoredEvent, SignalEvent};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LescheEvent {
    /// An envelope was delivered for one of the user's conversations.
    Envelope { envelope: Envelope },
    /// A tagma came online (it established a live, key-verified tunnel).
    TagmaOnline { tagma_id: TagmaId },
    /// A tagma went offline (tunnel dropped, past the reconnect grace window).
    TagmaOffline { tagma_id: TagmaId },
    /// A file was delivered into this user's files space (the delivery side
    /// of the files service's `POST /v1/files/{id}/send`), pushed by the
    /// files service via the lesche's internal surface. Plaintext operator
    /// metadata like the presence pair: where the file landed and who sent
    /// it are not conversation content. The recipient fetches the content
    /// itself via `GET /v1/files/{record_id}`.
    FileDelivered {
        /// The recipient's own record (the landing copy's id).
        record_id: uuid::Uuid,
        /// Where the copy landed in the recipient's space.
        path: String,
        /// The sender's principal string (user handle or tagma id).
        from: String,
        /// The file's display name (the source record's filename).
        name: String,
        /// Blob size in bytes.
        size: u64,
    },
    /// by the lesche. Plaintext and user-scoped like the presence pair (the
    /// lesche can read it): agent state and token budget are operator
    /// metadata, not conversation content. Emitted on a periodic snapshot, so
    /// a dropped frame just means slightly-stale data. The root agent (the
    /// conversation peer) is reported separately from subagents (spawned
    /// helpers) so the UI can distinguish "root processing the user's turn"
    /// from "helpers doing background work".
    TagmaStatus {
        tagma_id: TagmaId,
        /// The root agent's lifecycle state. `Faulted` is also the safe
        /// fallback reported when no root entry is registered (a
        /// production-unreachable state under normal startup ordering).
        root_state: AgentState,
        subagents_total: u32,
        subagents_active: u32,
        token_budget: u64,
        token_consumed: u64,
    },
    /// A tagma's per-event runtime signal (busy/idle presence, turn terminals,
    /// errors), pushed by the tagma and rebroadcast by the lesche on the
    /// owner's app event stream. Plaintext like the status/presence variants:
    /// operator metadata, not conversation content (authored content rides the
    /// encrypted envelope). Not persisted in `chat_history` and not replayed —
    /// a reconnect only replays authored messages; the tagma also writes each
    /// to its application log for observability.
    TagmaSignal {
        tagma_id: TagmaId,
        event: SignalEvent,
    },
    /// A room's membership changed (a member was added/removed). The user-device
    /// analog of the tagma-tunnel `Wake`: fanned to every live user member of
    /// the room so the frontend refreshes the roster (membership is
    /// server-authoritative). Transient -- not buffered, fanned to ALL live user
    /// members (no actor exclusion).
    RoomMembershipChanged { room_id: RoomId },
    /// A room member came online in `room_id` (it established a live tunnel, for
    /// an agent, or a live app stream, for a human). Fanned to every live HUMAN
    /// member of every room the participant belongs to, by the relay
    /// (`kallip-lesche`), on connect. Idempotent set-add -- clients MUST treat
    /// per-room presence as a set, not assume exactly-once (a participant
    /// connecting concurrently with the viewer's own snapshot may be delivered
    /// twice). The roster's `online` field is the fetch-time ground truth that
    /// resyncs this live layer.
    RoomMemberOnline {
        room_id: RoomId,
        /// The transitioning member's id; joins with `RoomMemberProfile.id` on
        /// the client.
        member_id: MemberId,
    },
    /// A room member went offline in `room_id`. Fanned on disconnect to every
    /// live HUMAN member of every room the participant belongs to (same audience
    /// and scope as the online variant). Transient (not buffered for offline
    /// viewers); the roster re-fetch resyncs. Idempotent like the online pair.
    RoomMemberOffline {
        room_id: RoomId,
        /// The transitioning member's id; joins with `RoomMemberProfile.id` on
        /// the client.
        member_id: MemberId,
    },

    /// A member's read cursor in a room advanced (the unread watermark).
    /// Fanned to the cursor owner's live app stream when their client PUTs the
    /// new `last_read_seq`, so the owner's OTHER live sessions (tabs, devices)
    /// converge their unread state in real time. No actor exclusion (the app
    /// stream is one broadcast channel per user): the initiating session also
    /// receives its own write and must treat the event as idempotent --
    /// advance the read watermark, re-derive the count between the known and
    /// read watermarks, never blindly zero (an out-of-order or own-echo event
    /// must not resurrect counted messages or lose unread ones). Transient --
    /// not buffered for offline viewers; the room list's `last_read_seq` field
    /// is the fetch-time ground truth that resyncs.
    RoomReadCursorChanged {
        room_id: RoomId,
        /// The new read watermark (`seq <= last_read_seq` is read).
        last_read_seq: i64,
    },
}

/// The tagma's periodic runtime snapshot — the `UpstreamEvent::Status`
/// element of the batched `POST /v1/tagmata/{tagma_id}/upstream` body,
/// rebroadcast as an [`LescheEvent::TagmaStatus`] on the owner's stream.
///
/// `tagma_id` is intentionally absent: the batch path is authoritative, and the
/// lesche asserts it matches the authenticated tagma before rebroadcast
/// (mirroring `post_envelope`'s `channel_id` check). Field names mirror
/// the [`LescheEvent::TagmaStatus`] variant; keep them in sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagmaStatusPayload {
    pub root_state: AgentState,
    pub subagents_total: u32,
    pub subagents_active: u32,
    pub token_budget: u64,
    pub token_consumed: u64,
    /// Whether the tagma's budget is unlimited (enforcement off,
    /// consumption still tracked). Absent (false) from older tagmas.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub token_budget_unlimited: bool,
}

/// `POST /v1/tagmata/{tagma_id}/upstream` request body element — one
/// plaintext metadata event on the single upstream channel. The batch body
/// is `Vec<UpstreamEvent>`; the lesche demultiplexes each element into the
/// same fan logic the three per-kind endpoints (status POST / signal POST /
/// state PUT) have always used, so the two paths cannot drift.
/// Adjacently tagged because the payload variants mix structs and an enum
/// (`SignalEvent` is itself internally tagged, which rules out an internal
/// tag here). Authored envelope traffic never joins this channel: envelopes
/// are encrypted request/reply pairs, not broadcast events, so the E2EE
/// boundary holds by construction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "event", rename_all = "snake_case")]
pub enum UpstreamEvent {
    /// Aggregate runtime snapshot; fans like the status POST (presence
    /// cache + `LescheEvent::TagmaStatus`).
    Status(TagmaStatusPayload),
    /// Runtime signal; fans like the signal POST
    /// (`LescheEvent::TagmaSignal` rebroadcast).
    Signal(SignalEvent),
    /// Full projection snapshot; fans like the state PUT (projection
    /// store accept plus dirty fan). Boxed: the batch enum is born at the
    /// flusher serialization boundary and dies at the lesche demux, so
    /// the variant-size spread is not worth carrying inline (clippy
    /// large_enum_variant).
    Projection(Box<crate::projection::ProjectionSnapshot>),
}

#[cfg(test)]
mod upstream_shape_tests {
    use super::*;

    /// The upstream channel is a cross-crate wire contract (tagma flusher
    /// serializes, lesche demultiplexes): pin the adjacent-tag shape per
    /// variant so neither side can rename or retag unilaterally.
    #[test]
    fn status_variant_adjacent_tag_shape() {
        let json = serde_json::json!({
            "type": "status",
            "event": {
                "root_state": "idle",
                "subagents_total": 0,
                "subagents_active": 0,
                "token_budget": 1,
                "token_consumed": 0,
            },
        });
        let event: UpstreamEvent = serde_json::from_value(json.clone()).expect("parses");
        assert!(matches!(event, UpstreamEvent::Status(_)));
        assert_eq!(serde_json::to_value(&event).expect("serializes"), json);
    }

    #[test]
    fn signal_variant_adjacent_tag_shape() {
        let json = serde_json::json!({ "type": "signal", "event": { "type": "busy" } });
        let event: UpstreamEvent = serde_json::from_value(json.clone()).expect("parses");
        assert!(matches!(event, UpstreamEvent::Signal(SignalEvent::Busy)));
        assert_eq!(serde_json::to_value(&event).expect("serializes"), json);
    }

    #[test]
    fn projection_variant_adjacent_tag_shape() {
        let json = serde_json::json!({
            "type": "projection",
            "event": {
                "agents": [],
                "status": {
                    "root_state": "idle",
                    "subagents_total": 0,
                    "subagents_active": 0,
                    "token_budget": 1,
                    "token_consumed": 0,
                },
                "push_seq": 7,
                "work_schedule": null,
            },
        });
        let event: UpstreamEvent = serde_json::from_value(json.clone()).expect("parses");
        assert!(matches!(event, UpstreamEvent::Projection(_)));
        assert_eq!(serde_json::to_value(&event).expect("serializes"), json);
    }
}
