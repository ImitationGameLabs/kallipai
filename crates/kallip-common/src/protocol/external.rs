//! The external, transport-neutral event vocabulary a frontend consumes.
//!
//! This is the agent-free subset of the tagma's internal `SseEvent` stream,
//! split by destination channel so the encrypted/plaintext boundary is
//! type-enforced:
//!
//! - [`AuthoredEvent`] is **conversation content**: it rides the encrypted E2EE
//!   envelope channel and is persisted in `chat_history` (replayable on
//!   reconnect). Only deliberately authored messages live here.
//! - [`SignalEvent`] is **operator metadata**: it rides the plaintext signal
//!   channel (e.g. `LescheEvent`), is never persisted in `chat_history`, and is
//!   surfaced to the UI as a transient system notice plus a derived
//!   presence/state indicator. The tagma also writes each to its application
//!   log for observability.
//!
//! The split is the wire-level realization of "encrypt conversation content,
//! keep operator metadata plaintext": only [`AuthoredEvent`] ever enters an
//! AEAD envelope. The projector ([`crate::protocol`](super) consumer in
//! `kallip-tagma::projector`) maps the internal `SseEvent` to one or the other.

use serde::{Deserialize, Serialize};

use super::sse::FailoverChainExhaustion;

/// A deliberately authored message — conversation content that crosses the
/// E2EE boundary and is durably logged.
///
/// Today only the assistant's explicit message to the user lives here. User
/// messages are replayed via a separate `UserMessage` reply shape (synthesized
/// from `chat_history`), not as an event variant. Streaming deltas are
/// internal-only and never reach this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthoredEvent {
    /// A full assistant message (the non-streaming form; deltas are dropped at
    /// the projector). Also the variant the agent's `send` delivery is mapped
    /// to — addressing the user is a deliberate act, surfaced as assistant
    /// content.
    AssistantContent { content: String },
}

/// A runtime signal — operator metadata, not conversation content. Rides the
/// plaintext signal channel, is never persisted in `chat_history`, and is
/// surfaced to the UI as a transient system notice and/or a presence/state
/// transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalEvent {
    /// The tagma started working on a turn (presence: "working").
    Busy,
    /// The agent yielded control (presence: "idle"). Content-less: a reply no
    /// longer rides the terminal event. The task parks, awaiting input.
    Idle,
    /// The in-flight turn was interrupted.
    Interrupted,
    /// The in-flight turn was cancelled (terminal).
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
    /// The turn failed.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    //! Byte-level wire-stability guards for the external event vocabulary.
    //!
    //! Authored content crosses the E2EE envelope and is persisted in
    //! chat_history, so its JSON shape is a load-bearing contract: any drift
    //! breaks decryption on already-deployed peers and corrupts replay. These
    //! tests pin the exact serialized bytes so a future rename or serde-attribute
    //! change is a loud compile-time failure, not a silent wire break.

    use super::*;

    fn round_trip<T>(value: &T, expected: &str)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("serialize");
        assert_eq!(json, expected, "wire bytes drifted");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(&back, value, "round-trip lost data");
    }

    #[test]
    fn authored_assistant_content_bytes() {
        round_trip(
            &AuthoredEvent::AssistantContent {
                content: "hi".into(),
            },
            r#"{"type":"assistant_content","content":"hi"}"#,
        );
    }

    #[test]
    fn signal_unit_variants_bytes() {
        round_trip(&SignalEvent::Busy, r#"{"type":"busy"}"#);
        round_trip(&SignalEvent::Idle, r#"{"type":"idle"}"#);
        round_trip(&SignalEvent::Interrupted, r#"{"type":"interrupted"}"#);
        round_trip(&SignalEvent::Cancelled, r#"{"type":"cancelled"}"#);
        round_trip(
            &SignalEvent::MaxRoundsExceeded,
            r#"{"type":"max_rounds_exceeded"}"#,
        );
    }

    #[test]
    fn signal_fielded_variants_bytes() {
        round_trip(
            &SignalEvent::Error {
                message: "boom".into(),
            },
            r#"{"type":"error","message":"boom"}"#,
        );
        round_trip(
            &SignalEvent::TokenBudgetExceeded {
                consumed: 1,
                budget: 2,
            },
            r#"{"type":"token_budget_exceeded","consumed":1,"budget":2}"#,
        );
        // FailoverChainExhaustion serializes camelCase (it is the shared
        // kallip-common type with a Display impl).
        round_trip(
            &SignalEvent::FailoverChainExhausted {
                reason: FailoverChainExhaustion::NoFailoverConfigured,
                detail: "d".into(),
            },
            r#"{"type":"failover_chain_exhausted","reason":"noFailoverConfigured","detail":"d"}"#,
        );
    }

    #[test]
    fn legacy_non_assisted_event_does_not_parse_as_authored() {
        // A pre-split outbound row carried a busy payload as the old TagmaEvent.
        // AuthoredEvent must reject it (the read-time legacy filter drops such
        // rows from replay).
        let legacy = r#"{"type":"busy"}"#;
        assert!(serde_json::from_str::<AuthoredEvent>(legacy).is_err());
        let legacy = r#"{"type":"status","message":"m"}"#;
        assert!(serde_json::from_str::<AuthoredEvent>(legacy).is_err());
    }
}
