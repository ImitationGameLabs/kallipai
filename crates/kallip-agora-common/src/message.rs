//! Envelope + E2E payload model.
//!
//! The agora sees only the [`Envelope`] (routing metadata + opaque ciphertext).
//! The [`TagmaRequest`] / [`TagmaReply`] inside is the E2E payload shared
//! between app and herald; the agora never decrypts it.

use crate::bytes::Ciphertext;
use crate::event::TagmaEvent;
use crate::ids::{ConversationId, TagmaId, TraceId, UserId};
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

/// Who sent an envelope. The agora is agent-free: an agent sender is attributed
/// only to its tagma, never to a daemon-internal agent id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Participant {
    User { user_id: UserId },
    Agent { tagma_id: TagmaId },
}

/// App -> herald: one semantic operation against the tagma, encrypted inside an
/// envelope. The herald owns the agent(s) that realize the op; the app never
/// names an agent. `req_id` correlates the op with its [`TagmaReply`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TagmaRequest {
    /// Send a user message to the tagma's root agent.
    SendMessage { req_id: u64, text: String },
    /// Interrupt the tagma's in-flight turn.
    Interrupt { req_id: u64 },
}

/// Herald -> app: either the result of a correlated op, or an unsolicited
/// event from the tagma's event pump. The agora never decrypts this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TagmaReply {
    /// `SendMessage` was accepted by the daemon.
    MessageAccepted {
        req_id: u64,
        queue_depth: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    /// `Interrupt` was delivered.
    Interrupted { req_id: u64 },
    /// An op failed. `status` mirrors the daemon/agora HTTP status where one
    /// applies (502 for an internal herald panic).
    Error {
        req_id: u64,
        status: u16,
        message: String,
    },
    /// An unsolicited tagma event. Has no `req_id`: it is produced by the
    /// herald's event pump, not in reply to any single op.
    Event { event: TagmaEvent },
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    #[test]
    fn participant_tag_is_snake_case() {
        let p = Participant::User {
            user_id: UserId::from("u1".to_string()),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "{\"kind\":\"user\",\"user_id\":\"u1\"}");

        let a = Participant::Agent {
            tagma_id: TagmaId::from("t1".to_string()),
        };
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, "{\"kind\":\"agent\",\"tagma_id\":\"t1\"}");
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

    #[test]
    fn tagma_request_round_trips() {
        let req = TagmaRequest::SendMessage {
            req_id: 7,
            text: "hi".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"op\":\"send_message\""));
        let back: TagmaRequest = serde_json::from_str(&json).unwrap();
        match back {
            TagmaRequest::SendMessage { req_id, text } => {
                assert_eq!(req_id, 7);
                assert_eq!(text, "hi");
            }
            _ => panic!("expected SendMessage"),
        }
    }

    #[test]
    fn tagma_reply_variants_round_trip() {
        let cases = vec![
            serde_json::to_string(&TagmaReply::MessageAccepted {
                req_id: 1,
                queue_depth: 0,
                warning: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::MessageAccepted {
                req_id: 1,
                queue_depth: 2,
                warning: Some("queue growing".into()),
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::Interrupted { req_id: 9 }).unwrap(),
            serde_json::to_string(&TagmaReply::Error {
                req_id: 5,
                status: 502,
                message: "boom".into(),
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::Event {
                event: TagmaEvent::Busy,
            })
            .unwrap(),
        ];
        for json in cases {
            let _: TagmaReply = serde_json::from_str(&json).unwrap();
        }
        // `warning` is omitted when None.
        let none_json = serde_json::to_string(&TagmaReply::MessageAccepted {
            req_id: 1,
            queue_depth: 0,
            warning: None,
        })
        .unwrap();
        assert!(!none_json.contains("warning"));
    }
}
