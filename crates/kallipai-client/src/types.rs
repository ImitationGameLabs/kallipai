pub(crate) use kallipai_common::protocol::MessageRequest;

/// Re-export of the shared query type under the client-facing name.
pub type ListApprovalsParams = kallipai_common::protocol::ListApprovalsQuery;

/// Re-export of the message response with queue depth feedback.
pub type MessageResponse = kallipai_common::protocol::MessageResponse;

/// Request body for `POST /agents/{id}/lesche/messages` (the agent's
/// `kallip lesche send`). Named with the `Lesche` prefix to avoid colliding
/// with the agent→agent `MessageRequest` (the `/agents/{id}/message` route).
///
/// `room` is the optional room id: present when the agent is replying into a
/// multi-member room; `tagma` the optional peer tagma id: present when the
/// agent is sending into that tagma's direct session. Exactly one may be
/// set; neither is the bilateral 1:1 send. Kept as raw strings so this
/// client crate stays free of archeion id-type coupling; the tagma parses them.
/// (A direct-session attachment must live in a workspace the peer can read:
/// a private-area record fails the peer's fetch with 403.)
#[derive(Debug, serde::Serialize)]
pub struct LescheMessageRequest {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagma: Option<String>,
    /// The position, in that conversation's own log, that this message
    /// answers: a room's `seq`, or that session's own `seq`. Absent for a
    /// send that answers nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<i64>,
    /// The participant ids this message names, as the sender wrote them. The
    /// tagma judges each one -- its shape, the cap, and whether it names a
    /// member of the room -- and answers 400 with the reason. A send that
    /// names nobody leaves the key off the wire, the way the CLI does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
}

/// One row of the unified session list (`GET /agents/{id}/lesche/sessions`,
/// the agent's `kallip lesche sessions`). Mirrors the tagma's
/// `SessionListEntry`; optional fields are absent for the kinds they do not
/// apply to.
#[derive(Debug, serde::Deserialize)]
pub struct LescheSessionEntry {
    /// `bilateral` (the fixed 1:1 with the operator), `room`, or `direct`.
    pub kind: String,
    /// The surface id: conversation id, room id, or derived session id.
    pub id: String,
    /// Room display name (rooms only).
    #[serde(default)]
    pub name: Option<String>,
    /// The peer's tagma id (direct sessions only).
    #[serde(default)]
    pub peer_tagma: Option<String>,
    /// The peer's server-stamped handle (direct sessions only).
    #[serde(default)]
    pub peer_handle: Option<String>,
}

/// Re-export of the message-delivery response.
pub type LescheMessageResponse = kallipai_common::message::DeliveryResponse;
