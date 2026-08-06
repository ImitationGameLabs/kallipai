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
use kallip_agora_common::ids::{MemberId, RoomId, TagmaId};
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
    /// A tagma's live runtime state, snapshotted by the tagma and rebroadcast
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
}

/// `POST /v1/tagmata/{tagma_id}/status` request body — the tagma's periodic
/// runtime snapshot, rebroadcast by the lesche as an
/// [`LescheEvent::TagmaStatus`] on the owner's app event stream.
///
/// `tagma_id` is intentionally absent: the path is authoritative, and the
/// lesche asserts it matches the authenticated tagma before rebroadcast
/// (mirroring `post_envelope`'s `conversation_id` check). Field names mirror
/// the [`LescheEvent::TagmaStatus`] variant; keep them in sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaStatusPayload {
    pub root_state: AgentState,
    pub subagents_total: u32,
    pub subagents_active: u32,
    pub token_budget: u64,
    pub token_consumed: u64,
}
