//! Envelope + E2E payload model.
//!
//! The agora sees only the [`Envelope`] (routing metadata + opaque ciphertext).
//! The [`TunnelFrame`] inside is the E2E payload shared between app and herald;
//! the agora never decrypts it.

use crate::bytes::B64;
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
        tagma_id: crate::ids::TagmaId,
        agent_id: AgentId,
    },
}

/// The E2E payload inside an envelope: one frame of the HTTP tunnel that
/// carries the daemon API over the relay. The app sends a single `Request`; the
/// herald replies with one `ResponseHead`, zero or more `ResponseBody` chunks
/// (streamed), then a `ResponseEnd` — except for a long-lived stream (e.g.
/// `GET /agents/{id}/events`) which stays open without a final `ResponseEnd`
/// until the daemon stream closes. Agnostic to HTTP/SSE semantics: the herald
/// forwards raw bytes, the endpoints frame them. The agora never decrypts this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum TunnelFrame {
    /// App -> herald: perform an HTTP request against the daemon.
    Request {
        req_id: u64,
        method: String,
        /// Daemon path only (must start with `/`, no scheme/authority).
        path: String,
        headers: Vec<(String, String)>,
        body: B64,
    },
    /// Herald -> app: the response status and headers, once.
    ResponseHead {
        req_id: u64,
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// Herald -> app: one chunk of the response body, in order. The herald caps
    /// each chunk so the enclosing envelope stays under the agora body limit.
    ResponseBody { req_id: u64, chunk: B64 },
    /// Herald -> app: the response body is complete (one-shot requests). Not
    /// sent for a long-lived stream that the app intentionally keeps open.
    /// `error` is set when the daemon stream broke mid-body (after a `200`
    /// head and some chunks already delivered): the app must treat the response
    /// as failed rather than silently truncating.
    ResponseEnd {
        req_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
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
    fn tunnel_request_round_trips() {
        let frame = TunnelFrame::Request {
            req_id: 42,
            method: "POST".into(),
            path: "/agents/a1/message".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: B64(vec![0xde, 0xad, 0xbe, 0xef]),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"frame\":\"request\""));
        // Body is base64, not a JSON number array.
        assert!(json.contains("\"body\":\"3q2+7w==\""));
        let back: TunnelFrame = serde_json::from_str(&json).unwrap();
        match back {
            TunnelFrame::Request {
                req_id,
                method,
                path,
                body,
                ..
            } => {
                assert_eq!(req_id, 42);
                assert_eq!(method, "POST");
                assert_eq!(path, "/agents/a1/message");
                assert_eq!(body.0, vec![0xde, 0xad, 0xbe, 0xef]);
            }
            _ => panic!("expected Request"),
        }
    }

    #[test]
    fn tunnel_response_frames_round_trip() {
        for frame in [
            TunnelFrame::ResponseHead {
                req_id: 1,
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
            },
            TunnelFrame::ResponseBody {
                req_id: 1,
                chunk: B64(b"data: hello\n\n".to_vec()),
            },
            TunnelFrame::ResponseEnd {
                req_id: 1,
                error: None,
            },
            TunnelFrame::ResponseEnd {
                req_id: 2,
                error: Some("stream broke".into()),
            },
        ] {
            let json = serde_json::to_string(&frame).unwrap();
            let back: TunnelFrame = serde_json::from_str(&json).unwrap();
            assert!(matches!(
                back,
                TunnelFrame::ResponseHead { .. }
                    | TunnelFrame::ResponseBody { .. }
                    | TunnelFrame::ResponseEnd { .. }
            ));
        }
        // `error` is omitted when None, present when Some.
        let none_json = serde_json::to_string(&TunnelFrame::ResponseEnd {
            req_id: 1,
            error: None,
        })
        .unwrap();
        assert!(!none_json.contains("error"));
        let some_json = serde_json::to_string(&TunnelFrame::ResponseEnd {
            req_id: 2,
            error: Some("boom".into()),
        })
        .unwrap();
        assert!(some_json.contains("\"error\":\"boom\""));
    }
}
