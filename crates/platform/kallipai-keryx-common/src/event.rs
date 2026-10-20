//! Events on the keryx app stream (`GET /me/events`): the non-room
//! vocabulary of the lesche stream, carried byte-identically.
//!
//! A single per-user connection carries the bilateral conversation
//! envelopes plus the tagma-lifecycle and plaintext metadata family:
//! aggregate status snapshots, runtime signals, and file-delivery
//! notifications, multiplexed by `conversation_id` / `tagma_id`.
//!
//! The variant set is the lesche stream's minus the room family (room
//! events stay on the lesche stream; the keryx never sees rooms). Serde
//! tags and field names match the lesche variants exactly, so a frame
//! serialized from one enum parses as its sibling and the wire bytes are
//! unchanged by the split.

use kallipai_archeion_common::ids::TagmaId;
use kallipai_lesche_common::message::Envelope;

use serde::{Deserialize, Serialize};

pub use kallipai_lesche_common::event::{AgentState, SignalEvent};

/// One event on the keryx `/me/events` stream. Internally tagged like the
/// lesche sibling (`type` + snake_case) with identical variant names and
/// field shapes: the split changes who emits the frames, not their bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KeryxEvent {
    /// An envelope was delivered for one of the user's conversations.
    Envelope { envelope: Envelope },
    /// A tagma came online (it established a live, key-verified tunnel).
    TagmaOnline { tagma_id: TagmaId },
    /// A tagma went offline (tunnel dropped, past the reconnect grace window).
    TagmaOffline { tagma_id: TagmaId },
    /// A file was delivered into this user's files space (the delivery side
    /// of the files service's `POST /files/{id}/send`), pushed by the
    /// files service via the keryx's internal surface. Plaintext operator
    /// metadata like the presence pair: where the file landed and who sent
    /// it are not conversation content. The recipient fetches the content
    /// itself via `GET /files/{record_id}`.
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
    /// A tagma's live aggregate runtime state (agent counts + token budget).
    /// Plaintext and user-scoped like the presence pair: agent state and
    /// token budget are operator metadata, not conversation content. Emitted
    /// on a periodic snapshot, so a dropped frame just means slightly-stale
    /// data. The root agent (the conversation peer) is reported separately
    /// from subagents (spawned helpers) so the UI can distinguish "root
    /// processing the user's turn" from "helpers doing background work".
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
    /// A tagma's per-event runtime signal (busy/idle presence, turn
    /// terminals, errors), pushed by the tagma and rebroadcast on the
    /// owner's app event stream. Plaintext like the status/presence
    /// variants: operator metadata, not conversation content (authored
    /// content rides the encrypted envelope). Not persisted in
    /// `chat_history` and not replayed -- a reconnect only replays
    /// authored messages; the tagma also writes each to its application
    /// log for observability.
    TagmaSignal {
        tagma_id: TagmaId,
        event: SignalEvent,
    },
    /// A 1:1 conversation's read position moved (or its unread count changed
    /// with a new reply). Plaintext and user-scoped like the status family:
    /// the relay learns *that* the user has read up to a row, never what the
    /// row says. Cached per tunnel and flushed to a connecting app stream, so
    /// a cold start badges every conversation without opening any of them.
    TagmaUnread {
        tagma_id: TagmaId,
        /// The peer partition (`ParticipantId`, or empty for the operator).
        peer: String,
        /// Newest `chat_history` row the user has read.
        cursor: i64,
        /// The agent's replies above the cursor.
        unread: u32,
    },
}

#[cfg(test)]
mod shape_tests {
    use super::*;
    use kallipai_archeion_common::bytes::Ciphertext;
    use kallipai_archeion_common::ids::{
        ChannelId, ParticipantId, ParticipantKind, TraceId, UserId,
    };
    use kallipai_lesche_common::message::Participant;

    /// A minimal wire-shaped envelope shared by the shape tests.
    fn test_envelope() -> Envelope {
        Envelope {
            channel_id: ChannelId::from("c1".to_string()),
            sender: Participant {
                id: ParticipantId::for_user(&UserId::from("u1".to_string())),
                kind: ParticipantKind::Human,
                handle: "Alice".into(),
                tagma_id: None,
            },
            sequence_n: 3,
            trace_id: TraceId::from("t1".to_string()),
            timestamp: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            ciphertext: Ciphertext(vec![1, 2, 3]),
        }
    }

    /// The keryx stream is a cross-crate wire contract (the keryx
    /// serializes, the UI parses), split from a stream clients already
    /// hold cursors on: pin the tag shape per variant family so neither
    /// a rename nor a retag can land silently.
    #[test]
    fn variants_keep_the_lesche_tag_shapes() {
        let cases: Vec<(KeryxEvent, &str)> = vec![
            (
                KeryxEvent::TagmaOnline {
                    tagma_id: TagmaId::from("tagma_x".to_string()),
                },
                "tagma_online",
            ),
            (
                KeryxEvent::TagmaOffline {
                    tagma_id: TagmaId::from("tagma_x".to_string()),
                },
                "tagma_offline",
            ),
            (
                KeryxEvent::TagmaStatus {
                    tagma_id: TagmaId::from("tagma_x".to_string()),
                    root_state: AgentState::Idle,
                    subagents_total: 0,
                    subagents_active: 0,
                    token_budget: 1,
                    token_consumed: 0,
                },
                "tagma_status",
            ),
            (
                KeryxEvent::TagmaSignal {
                    tagma_id: TagmaId::from("tagma_x".to_string()),
                    event: SignalEvent::Busy,
                },
                "tagma_signal",
            ),
        ];
        for (event, tag) in cases {
            let json = serde_json::to_value(&event).expect("serializes");
            assert_eq!(json["type"], tag);
            let back: KeryxEvent = serde_json::from_value(json).expect("parses back");
            assert!(std::mem::discriminant(&back) == std::mem::discriminant(&event));
        }
    }

    /// The plaintext variants pin their field names, not only their
    /// tags: a field rename round-trips cleanly inside this crate
    /// while silently breaking every client cursor, so the JSON
    /// keys themselves are the contract being locked here.
    #[test]
    fn plaintext_variants_pin_field_names() {
        let tagma = TagmaId::from("tagma_x".to_string());
        let cases: Vec<(KeryxEvent, serde_json::Value)> = vec![
            (
                KeryxEvent::TagmaOnline {
                    tagma_id: tagma.clone(),
                },
                serde_json::json!({"type": "tagma_online", "tagma_id": "tagma_x"}),
            ),
            (
                KeryxEvent::TagmaOffline {
                    tagma_id: tagma.clone(),
                },
                serde_json::json!({"type": "tagma_offline", "tagma_id": "tagma_x"}),
            ),
            (
                KeryxEvent::TagmaStatus {
                    tagma_id: tagma.clone(),
                    root_state: AgentState::Idle,
                    subagents_total: 1,
                    subagents_active: 2,
                    token_budget: 3,
                    token_consumed: 4,
                },
                serde_json::json!({
                    "type": "tagma_status",
                    "tagma_id": "tagma_x",
                    "root_state": "idle",
                    "subagents_total": 1,
                    "subagents_active": 2,
                    "token_budget": 3,
                    "token_consumed": 4,
                }),
            ),
            (
                KeryxEvent::TagmaSignal {
                    tagma_id: tagma.clone(),
                    event: SignalEvent::Busy,
                },
                serde_json::json!({
                    "type": "tagma_signal",
                    "tagma_id": "tagma_x",
                    "event": {"type": "busy"},
                }),
            ),
        ];
        for (event, json) in cases {
            let ser = serde_json::to_value(&event).expect("serializes");
            assert_eq!(&ser, &json, "field names and values are the contract");
            let back: KeryxEvent = serde_json::from_value(ser).expect("parses");
            assert_eq!(serde_json::to_value(&back).expect("reserializes"), json);
        }
    }

    /// The payload-bearing variants pin to concrete JSON shapes (an
    /// envelope frame and a file-delivery frame), the exact bytes the
    /// lesche stream emitted for the same facts.
    #[test]
    fn envelope_variant_shape() {
        let event = KeryxEvent::Envelope {
            envelope: test_envelope(),
        };
        let json = serde_json::to_value(&event).expect("serializes");
        assert_eq!(json["type"], "envelope");
        assert_eq!(json["envelope"]["sequence_n"], 3);
        let back: KeryxEvent = serde_json::from_value(json).expect("parses back");
        assert!(matches!(back, KeryxEvent::Envelope { .. }));
    }

    #[test]
    fn file_delivered_variant_shape() {
        let json = serde_json::json!({
            "type": "file_delivered",
            "record_id": "00000000-0000-0000-0000-000000000001",
            "path": "inbox/a.bin",
            "from": "user_x",
            "name": "a.bin",
            "size": 3,
        });
        let event: KeryxEvent = serde_json::from_value(json.clone()).expect("parses");
        assert!(matches!(event, KeryxEvent::FileDelivered { .. }));
        assert_eq!(serde_json::to_value(&event).expect("serializes"), json);
    }

    /// The split's core promise is byte identity: every non-room
    /// frame serialized from one enum must parse as its sibling and
    /// serialize back to the same value, in both directions, so a
    /// serde edit on either side cannot drift silently.
    #[test]
    fn non_room_frames_cross_parse_between_siblings() {
        let tagma = TagmaId::from("tagma_x".to_string());
        let frames: Vec<serde_json::Value> = vec![
            serde_json::to_value(KeryxEvent::Envelope {
                envelope: test_envelope(),
            })
            .expect("serializes"),
            serde_json::to_value(KeryxEvent::TagmaOnline {
                tagma_id: tagma.clone(),
            })
            .expect("serializes"),
            serde_json::to_value(KeryxEvent::TagmaOffline {
                tagma_id: tagma.clone(),
            })
            .expect("serializes"),
            serde_json::to_value(KeryxEvent::FileDelivered {
                record_id: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                path: "inbox/a.bin".into(),
                from: "user_x".into(),
                name: "a.bin".into(),
                size: 3,
            })
            .expect("serializes"),
            serde_json::to_value(KeryxEvent::TagmaStatus {
                tagma_id: tagma.clone(),
                root_state: AgentState::Idle,
                subagents_total: 0,
                subagents_active: 0,
                token_budget: 1,
                token_consumed: 0,
            })
            .expect("serializes"),
            serde_json::to_value(KeryxEvent::TagmaSignal {
                tagma_id: tagma.clone(),
                event: SignalEvent::Busy,
            })
            .expect("serializes"),
        ];
        for frame in frames {
            let lesche: kallipai_lesche_common::event::LescheEvent =
                serde_json::from_value(frame.clone()).expect("lesche parses");
            assert_eq!(
                serde_json::to_value(&lesche).expect("lesche serializes"),
                frame
            );
            let keryx: KeryxEvent = serde_json::from_value(frame.clone()).expect("keryx parses");
            assert_eq!(
                serde_json::to_value(&keryx).expect("keryx serializes"),
                frame
            );
        }
    }
}
