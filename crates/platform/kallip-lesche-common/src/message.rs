//! Envelope + E2E payload model.
//!
//! The agora sees only the [`Envelope`] (routing metadata + opaque ciphertext).
//! The [`TagmaRequest`] / [`TagmaReply`] inside is the E2E payload shared
//! between app and tagma; the agora never decrypts it.

use crate::event::AuthoredEvent;
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::ids::{ConversationId, TagmaId, TraceId, UserId};
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
/// only to its tagma, never to a tagma-internal agent id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Participant {
    User { user_id: UserId },
    Agent { tagma_id: TagmaId },
}

/// App -> tagma: one semantic operation against the tagma, encrypted inside an
/// envelope. The tagma owns the agent(s) that realize the op; the app never
/// names an agent. `req_id` correlates the op with its [`TagmaReply`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TagmaRequest {
    /// Send a user message to the tagma's root agent.
    SendMessage { req_id: u64, text: String },
    /// Interrupt the tagma's in-flight turn.
    Interrupt { req_id: u64 },
}

/// App -> tagma: a control operation that does NOT drive the agent (today: the
/// cursor-based history pull). Carried in the same encrypted envelope channel as
/// [`TagmaRequest`]; the relay dispatches by the `op` discriminant (which is
/// disjoint from `TagmaRequest`'s). Kept separate so `TagmaRequest` stays
/// "actions against the agent" and is not polluted by sync plumbing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TagmaControl {
    /// Pull a batch of chat history. The `(after, before)` pair selects the
    /// window:
    /// - `after=None,  before=None`  -> most recent `limit` rows (a first-time
    ///   device with an empty local cache).
    /// - `after=Some,  before=None`  -> rows with `id > after` (incremental
    ///   catch-up on reconnect; the app sends its rendered high-water mark).
    /// - `after=None,  before=Some`  -> rows with `id < before` (scroll-up lazy
    ///   load of older history).
    ///
    /// The relay responds with the matching rows (re-encrypted) followed by a
    /// [`TagmaReply::HistoryBatchEnd`] carrying the same `req_id`. `limit` is
    /// clamped server-side to `HISTORY_BATCH_MAX`.
    History {
        req_id: u64,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    },
}

/// Tagma -> app: either the result of a correlated op, or an unsolicited
/// event from the tagma's event pump. The agora never decrypts this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TagmaReply {
    /// `SendMessage` was accepted by the tagma. `history_id` is the
    /// `chat_history.id` of the **inbound** row the tagma appended for this
    /// user message (before running the op), so the app can stamp its
    /// optimistic local user line with a stable id and dedup it against a later
    /// history replay. `0` means no row was recorded and must not be used for
    /// dedup. The ack itself is live-only: it is never stored or replayed.
    MessageAccepted {
        req_id: u64,
        queue_depth: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
        #[serde(default)]
        history_id: i64,
        /// When this inbound row was appended (RFC 3339). Absent on acks with no
        /// durable row (`history_id == 0`) and on payloads serialized before the
        /// field existed. The authoritative send time for the user's line.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    /// `Interrupt` was delivered.
    Interrupted { req_id: u64 },
    /// An op failed. `status` mirrors the tagma/agora HTTP status where one
    /// applies (502 for an internal tagma panic).
    Error {
        req_id: u64,
        status: u16,
        message: String,
    },
    /// An unsolicited authored tagma event (an assistant message). Has no
    /// `req_id`: it is produced by the tagma's event pump, not in reply to any
    /// single op. `history_id` is the `chat_history.id` of the outbound row the
    /// tagma appended for this event before emitting it; the app uses it as a
    /// stable id to order/dedup frames across batch replay and live delivery.
    /// `0` means the row was not recorded (e.g. a relay running without chat
    /// history) and must not be used for dedup. Runtime signals (busy/idle,
    /// terminals, errors) do NOT ride this variant — they cross as plaintext
    /// `LescheEvent::TagmaSystem`, since they are operator metadata, not
    /// conversation content.
    Event {
        event: AuthoredEvent,
        #[serde(default)]
        history_id: i64,
        /// When the outbound row was appended (RFC 3339). Absent on frames with
        /// no durable row and on payloads serialized before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    /// Replay-only echo of a user-authored message (an inbound `chat_history`
    /// row), produced by `handle_history` so the app can reconstruct the user
    /// side of a past conversation. Never emitted on the live path: live user
    /// messages come from the app's own optimistic render + `MessageAccepted`.
    /// `history_id` is the inbound row id; the app dedups/orders by it.
    UserMessage {
        history_id: i64,
        text: String,
        /// When the inbound row was originally appended (RFC 3339). Absent on
        /// payloads serialized before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    /// The sole completion signal for a `TagmaControl::History` batch. The
    /// relay emits the batch's rows (each as `Event` or `UserMessage`) and then
    /// this marker carrying the same `req_id`. `count` is the rows sent;
    /// `more` being true means the window was truncated by `limit` and more
    /// rows remain pullable. If this marker never arrives, the app treats the
    /// sync as failed and retries on the next reconnect.
    HistoryBatchEnd { req_id: u64, count: u32, more: bool },
}

impl TagmaReply {
    /// Stamp a `chat_history` row id onto this reply. `Event` carries the id of
    /// its outbound row; `MessageAccepted` carries the id of the inbound row
    /// the send appended. The other variants are live-only (never stored or
    /// replayed) and are left untouched.
    pub fn set_history_id(&mut self, id: i64) {
        match self {
            TagmaReply::Event { history_id, .. }
            | TagmaReply::MessageAccepted { history_id, .. } => *history_id = id,
            _ => {}
        }
    }

    /// Stamp the row's `created_at` (Unix seconds) onto this reply as an RFC
    /// 3339 string. Covers the three durable variants: `Event` (outbound row),
    /// `MessageAccepted` (live inbound ack), `UserMessage` (replayed inbound).
    /// Unlike [`set_history_id`](Self::set_history_id) -- which skips
    /// `UserMessage` because its `history_id` is a required field set at
    /// construction -- this covers `UserMessage` too, since its `created_at`
    /// defaults to `None` and is stamped from the row by the replay path.
    pub fn set_created_at(&mut self, secs: i64) {
        let formatted = format_created_at(secs);
        match self {
            TagmaReply::Event { created_at, .. }
            | TagmaReply::MessageAccepted { created_at, .. }
            | TagmaReply::UserMessage { created_at, .. } => *created_at = Some(formatted),
            _ => {}
        }
    }
}

/// Format a Unix-seconds `created_at` as an RFC 3339 string. `from_unix_timestamp`
/// yields UTC, which `time`'s `Rfc3339` renders with a `Z` suffix
/// (`2026-07-26T12:34:56Z`). Falls back to the epoch for inputs outside the
/// representable range (never expected for a real `created_at`).
fn format_created_at(secs: i64) -> String {
    use time::format_description::well_known::Rfc3339;
    OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_default()
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
                history_id: 0,
                created_at: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::MessageAccepted {
                req_id: 1,
                queue_depth: 2,
                warning: Some("queue growing".into()),
                history_id: 7,
                created_at: None,
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
                event: AuthoredEvent::AssistantContent {
                    content: "hi".into(),
                },
                history_id: 0,
                created_at: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::Event {
                event: AuthoredEvent::AssistantContent {
                    content: "hi".into(),
                },
                history_id: 42,
                created_at: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::UserMessage {
                history_id: 11,
                text: "hi".into(),
                created_at: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::HistoryBatchEnd {
                req_id: 3,
                count: 10,
                more: true,
            })
            .unwrap(),
        ];
        for json in cases {
            let _: TagmaReply = serde_json::from_str(&json).unwrap();
        }
        // `warning` and `created_at` are omitted when None.
        let none_json = serde_json::to_string(&TagmaReply::MessageAccepted {
            req_id: 1,
            queue_depth: 0,
            warning: None,
            history_id: 0,
            created_at: None,
        })
        .unwrap();
        assert!(!none_json.contains("warning"));
        assert!(!none_json.contains("created_at"));
        // `history_id` / `created_at` default when absent, so an Event serialized
        // without them (a stored payload from before the stamp, or an older peer)
        // still parses.
        let unstamped = r#"{"kind":"event","event":{"type":"assistant_content","content":"hi"}}"#;
        let parsed: TagmaReply = serde_json::from_str(unstamped).unwrap();
        assert!(matches!(
            parsed,
            TagmaReply::Event {
                history_id: 0,
                created_at: None,
                ..
            }
        ));
        // `history_id` round-trips a non-zero value and the field name is pinned.
        let stamped = serde_json::to_string(&TagmaReply::Event {
            event: AuthoredEvent::AssistantContent {
                content: "hi".into(),
            },
            history_id: 42,
            created_at: None,
        })
        .unwrap();
        assert!(
            stamped.contains("\"history_id\":42"),
            "history_id field name pinned: {stamped}"
        );
        // UserMessage / HistoryBatchEnd field names are pinned (created_at is
        // omitted when None, so the UserMessage form is unchanged).
        let um = serde_json::to_string(&TagmaReply::UserMessage {
            history_id: 11,
            text: "hi".into(),
            created_at: None,
        })
        .unwrap();
        assert_eq!(um, r#"{"kind":"user_message","history_id":11,"text":"hi"}"#);
        let hbe = serde_json::to_string(&TagmaReply::HistoryBatchEnd {
            req_id: 3,
            count: 10,
            more: true,
        })
        .unwrap();
        assert_eq!(
            hbe,
            r#"{"kind":"history_batch_end","req_id":3,"count":10,"more":true}"#
        );
        // set_history_id stamps Event and MessageAccepted; leaves others alone.
        let mut ev = TagmaReply::Event {
            event: AuthoredEvent::AssistantContent {
                content: "hi".into(),
            },
            history_id: 0,
            created_at: None,
        };
        ev.set_history_id(99);
        assert!(matches!(ev, TagmaReply::Event { history_id: 99, .. }));
        let mut ack = TagmaReply::MessageAccepted {
            req_id: 1,
            queue_depth: 0,
            warning: None,
            history_id: 0,
            created_at: None,
        };
        ack.set_history_id(99);
        assert!(matches!(
            ack,
            TagmaReply::MessageAccepted { history_id: 99, .. }
        ));
        let mut interrupted = TagmaReply::Interrupted { req_id: 1 };
        interrupted.set_history_id(99);
        assert!(matches!(interrupted, TagmaReply::Interrupted { req_id: 1 }));
    }

    #[test]
    fn set_created_at_formats_and_covers_durable_variants() {
        // `time`'s Rfc3339 special-cases UTC to a `Z` suffix (not `+00:00`).
        assert_eq!(format_created_at(1_785_069_296), "2026-07-26T12:34:56Z");

        let mut ev = TagmaReply::Event {
            event: AuthoredEvent::AssistantContent {
                content: "hi".into(),
            },
            history_id: 0,
            created_at: None,
        };
        ev.set_created_at(1_785_069_296);
        assert!(
            matches!(ev, TagmaReply::Event { ref created_at, .. } if created_at.as_deref() == Some("2026-07-26T12:34:56Z"))
        );

        let mut ack = TagmaReply::MessageAccepted {
            req_id: 1,
            queue_depth: 0,
            warning: None,
            history_id: 0,
            created_at: None,
        };
        ack.set_created_at(1_785_069_296);
        assert!(matches!(
            ack,
            TagmaReply::MessageAccepted { ref created_at, .. }
                if created_at.as_deref() == Some("2026-07-26T12:34:56Z")
        ));

        let mut um = TagmaReply::UserMessage {
            history_id: 11,
            text: "hi".into(),
            created_at: None,
        };
        um.set_created_at(1_785_069_296);
        assert!(matches!(
            um,
            TagmaReply::UserMessage { ref created_at, .. }
                if created_at.as_deref() == Some("2026-07-26T12:34:56Z")
        ));

        // Non-durable variants are left untouched (no field to stamp).
        let mut interrupted = TagmaReply::Interrupted { req_id: 1 };
        interrupted.set_created_at(1_785_069_296);
        assert!(matches!(interrupted, TagmaReply::Interrupted { req_id: 1 }));
    }

    #[test]
    fn tagma_control_history_round_trips() {
        // latest mode
        let latest = serde_json::to_string(&TagmaControl::History {
            req_id: 1,
            after: None,
            before: None,
            limit: 50,
        })
        .unwrap();
        assert_eq!(
            latest,
            r#"{"op":"history","req_id":1,"after":null,"before":null,"limit":50}"#
        );
        let _: TagmaControl = serde_json::from_str(&latest).unwrap();
        // after mode
        let after = serde_json::to_string(&TagmaControl::History {
            req_id: 2,
            after: Some(10),
            before: None,
            limit: 50,
        })
        .unwrap();
        let parsed: TagmaControl = serde_json::from_str(&after).unwrap();
        assert!(matches!(
            parsed,
            TagmaControl::History {
                after: Some(10),
                before: None,
                limit: 50,
                ..
            }
        ));
        // before mode
        let before = serde_json::to_string(&TagmaControl::History {
            req_id: 3,
            after: None,
            before: Some(20),
            limit: 5,
        })
        .unwrap();
        let parsed: TagmaControl = serde_json::from_str(&before).unwrap();
        assert!(matches!(
            parsed,
            TagmaControl::History {
                after: None,
                before: Some(20),
                limit: 5,
                ..
            }
        ));
    }

    #[test]
    fn request_and_control_op_tags_are_disjoint() {
        // The relay dispatches by the `op` discriminant; the two enums must not
        // share a tag, and `history` must not parse as a TagmaRequest (and vice
        // versa for send_message/interrupt).
        let history = serde_json::to_string(&TagmaControl::History {
            req_id: 1,
            after: None,
            before: None,
            limit: 1,
        })
        .unwrap();
        assert!(
            serde_json::from_str::<TagmaRequest>(&history).is_err(),
            "history op must not parse as a TagmaRequest"
        );
        let send = serde_json::to_string(&TagmaRequest::SendMessage {
            req_id: 1,
            text: "x".into(),
        })
        .unwrap();
        assert!(
            serde_json::from_str::<TagmaControl>(&send).is_err(),
            "send_message op must not parse as a TagmaControl"
        );
    }

    #[test]
    fn tagma_request_op_discriminants_are_disjoint() {
        let req = serde_json::to_string(&TagmaRequest::SendMessage {
            req_id: 1,
            text: "hi".into(),
        })
        .unwrap();
        assert!(req.contains("\"op\":\"send_message\""));
        let req = serde_json::to_string(&TagmaRequest::Interrupt { req_id: 2 }).unwrap();
        assert!(req.contains("\"op\":\"interrupt\""));
    }
}
