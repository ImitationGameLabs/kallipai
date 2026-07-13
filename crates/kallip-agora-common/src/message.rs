//! Envelope + plaintext content model.
//!
//! The agora sees only the [`Envelope`] (routing metadata + opaque ciphertext).
//! The [`Plaintext`] inside is the E2E payload shared between app and herald;
//! the agora never decrypts it.

use crate::bytes::Ciphertext;
use crate::ids::{ConversationId, TraceId, UserId};
use kallip_common::agentid::AgentId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The unit the agora forwards between endpoints. Carries routing metadata +
/// AEAD ciphertext; the agora reads only the metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub conversation_id: ConversationId,
    pub sender: Participant,
    /// Per-conversation, per-sender monotonic counter from 0. Doubles as the
    /// AEAD nonce counter (direction-tagged) and as the agora's idempotency key.
    pub sequence_n: u64,
    pub trace_id: TraceId,
    #[serde(with = "time::serde::iso8601")]
    pub timestamp: OffsetDateTime,
    pub ciphertext: Ciphertext,
}

/// Who sent an envelope.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Participant {
    User {
        user_id: UserId,
    },
    Agent {
        team_id: crate::ids::TeamId,
        agent_id: AgentId,
    },
}

/// The E2E plaintext inside an envelope. Shared by app and herald; the agora
/// never sees this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Plaintext {
    Text {
        text: String,
        /// `sequence_n` of the message this replies to, for threading. `None` is
        /// a top-level message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<u64>,
    },
    Structured {
        /// Typed JSON payload (a proposal, a tool-call summary, a structured
        /// handoff). Opaque to the agora.
        json: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<u64>,
    },
    System {
        event: SystemEvent,
    },
}

/// A notice injected into a conversation by the system (agent lifecycle / turn
/// outcome). Delivered as a [`Plaintext::System`] message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SystemEvent {
    /// The bound agent started working on a turn.
    AgentBusy,
    /// The bound agent finished a turn and is waiting.
    AgentIdle,
    /// The bound agent could not be brought up (e.g. restore failure).
    AgentFaulted { reason: String },
    /// A turn ended without producing a reply (error / limits / cancellation).
    TurnError { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    #[test]
    fn plaintext_text_round_trips() {
        let p = Plaintext::Text {
            text: "hi".into(),
            parent: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"kind\":\"text\""));
        let back: Plaintext = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Plaintext::Text { text, parent: None } if text == "hi"));
    }

    #[test]
    fn participant_tag_is_snake_case() {
        let p = Participant::User {
            user_id: UserId::from("u1".to_string()),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "{\"kind\":\"user\",\"user_id\":\"u1\"}");
    }

    #[test]
    fn envelope_round_trips() {
        let env = Envelope {
            conversation_id: ConversationId::from("c1".to_string()),
            sender: Participant::User {
                user_id: UserId::from("u1".to_string()),
            },
            sequence_n: 3,
            trace_id: TraceId::from("t1".to_string()),
            timestamp: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            ciphertext: Ciphertext(vec![1, 2, 3]),
        };
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sequence_n, 3);
        assert_eq!(back.ciphertext.0, vec![1, 2, 3]);
    }
}
