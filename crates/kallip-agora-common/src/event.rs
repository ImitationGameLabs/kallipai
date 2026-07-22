//! Events on the app's multiplexed SSE stream (`GET /v1/me/events`), and the
//! tagma-facing event vocabulary carried inside E2EE envelopes.
//!
//! A single per-user connection carries envelope deliveries for all of the
//! user's conversations plus presence transitions, multiplexed by
//! `conversation_id` / `tagma_id`.
//!
//! Key exchange is NOT delivered here: it is a synchronous request/reply on
//! `POST /v1/conversations/{id}/key-exchange/init`, whose response body carries
//! the herald's signed key-exchange response directly.
//!
//! The presence variants (`TagmaOnline`, `TagmaOffline`) are emitted by the
//! data-plane relay (`kallip-lesche`) on the app event stream when a
//! herald tunnel connects/disconnects (and as a snapshot when the stream
//! opens). `AgentState` remains reserved for future per-agent lifecycle
//! surfacing.
//!
//! [`TagmaEvent`] is the *public, agent-free* event vocabulary the herald
//! produces (by mapping the tagma's internal `SseEvent` stream) and the app
//! consumes, inside the AEAD envelope. It is deliberately not a re-export of the
//! tagma's event type: the agora/herald public contract must not be coupled to
//! tagma-internal event shapes.

use crate::ids::TagmaId;
use crate::message::Envelope;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::AgentState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgoraEvent {
    /// An envelope was delivered for one of the user's conversations.
    Envelope { envelope: Envelope },
    /// A tagma came online (its herald established a live, key-verified tunnel).
    TagmaOnline { tagma_id: TagmaId },
    /// A tagma went offline (tunnel dropped, past the reconnect grace window).
    TagmaOffline { tagma_id: TagmaId },
    /// A surfaced agent's lifecycle state changed.
    #[allow(dead_code)]
    AgentState {
        tagma_id: TagmaId,
        agent_id: AgentId,
        state: AgentState,
    },
}

/// An event the tagma emits to the app, carried inside an E2EE envelope as a
/// [`crate::message::TagmaReply::Event`].
///
/// This is the agent-free, tagma-facing subset of the tagma's event stream.
/// The herald maps the tagma's `SseEvent` to this vocabulary, dropping
/// streaming-delta, tool, retry, and approval variants (they are outside the
/// app's capability set for the agora path). Approval-gated turns surface only
/// as `Busy` followed by silence until the operator resolves the approval
/// out-of-band.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TagmaEvent {
    /// A full assistant message (the non-streaming form; deltas are dropped).
    AssistantContent { content: String },
    /// The turn completed with this final assistant content.
    Finished { content: String },
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
