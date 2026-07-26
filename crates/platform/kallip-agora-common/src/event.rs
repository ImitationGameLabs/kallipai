//! Events on the app's multiplexed SSE stream (`GET /v1/me/events`), and the
//! tagma-facing event vocabulary carried inside E2EE envelopes.
//!
//! A single per-user connection carries envelope deliveries for all of the
//! user's conversations plus presence transitions, multiplexed by
//! `conversation_id` / `tagma_id`.
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
//! by the lesche.
//!
//! [`TagmaEvent`] is the *public, agent-free* event vocabulary the tagma
//! produces (by mapping the tagma's internal `SseEvent` stream) and the app
//! consumes, inside the AEAD envelope. It is deliberately not a re-export of the
//! tagma's event type: the agora/tagma public contract must not be coupled to
//! tagma-internal event shapes.

use crate::ids::TagmaId;
use crate::message::Envelope;
use serde::{Deserialize, Serialize};

// Re-exported so downstream crates (e.g. `kallip-lesche-client`) can construct
// a [`TagmaStatusPayload`] without a direct `kallip_common` dependency. Also
// brought into scope for this module's own use.
pub use kallip_common::protocol::AgentState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgoraEvent {
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
}

/// `POST /v1/tagmata/{tagma_id}/status` request body — the tagma's periodic
/// runtime snapshot, rebroadcast by the lesche as an
/// [`AgoraEvent::TagmaStatus`] on the owner's app event stream.
///
/// `tagma_id` is intentionally absent: the path is authoritative, and the
/// lesche asserts it matches the authenticated tagma before rebroadcast
/// (mirroring `post_envelope`'s `conversation_id` check). Field names mirror
/// the [`AgoraEvent::TagmaStatus`] variant; keep them in sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaStatusPayload {
    pub root_state: AgentState,
    pub subagents_total: u32,
    pub subagents_active: u32,
    pub token_budget: u64,
    pub token_consumed: u64,
}

/// An event the tagma emits to the app, carried inside an E2EE envelope as a
/// [`crate::message::TagmaReply::Event`].
///
/// This is the agent-free, tagma-facing subset of the tagma's event stream.
/// The tagma maps its own `SseEvent` to this vocabulary, dropping
/// streaming-delta, tool, retry, and approval variants (they are outside the
/// app's capability set for the agora path). Approval-gated turns surface only
/// as `Busy` followed by silence until the operator resolves the approval
/// out-of-band.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TagmaEvent {
    /// A full assistant message (the non-streaming form; deltas are dropped).
    /// This is also the variant the `kallip lesche send` CLI's deliveries are
    /// mapped to — a message to the user is a deliberate act, surfaced as
    /// assistant content.
    AssistantContent { content: String },
    /// The agent yielded control (called `break`, or was force-idled). Content-less:
    /// a reply no longer rides the terminal event. The task parks, awaiting input.
    Idle,
    /// The tagma started working on a turn.
    Busy,
    /// A lifecycle/status notice.
    Status { message: String },
    /// The turn failed.
    Error { message: String },
    /// The in-flight turn was interrupted.
    Interrupted,
    /// The in-flight turn was cancelled.
    Cancelled,
    /// The tagma exhausted its token budget mid-turn.
    TokenBudgetExceeded { consumed: u64, budget: u64 },
    /// The tagma hit its max tool rounds mid-turn.
    MaxRoundsExceeded,
    /// The tagma's model failover chain is exhausted.
    FailoverChainExhausted {
        reason: FailoverChainExhaustion,
        detail: String,
    },
}

/// Why the failover chain ran out. Mirrors the tagma's
/// `FailoverChainExhaustion` but lives in the agent-free public contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FailoverChainExhaustion {
    NoFailoverConfigured,
    AllBackupsExhausted,
    AllCandidatesUnbuildable,
    AllCandidatesInfeasible,
}
