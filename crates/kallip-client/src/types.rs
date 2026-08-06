pub(crate) use kallip_common::protocol::MessageRequest;

/// Re-export of the shared query type under the client-facing name.
pub type ListApprovalsParams = kallip_common::protocol::ListApprovalsQuery;

/// Re-export of the message response with queue depth feedback.
pub type MessageResponse = kallip_common::protocol::MessageResponse;

/// Request body for `POST /agents/{id}/lesche/messages` (the agent's
/// `kallip lesche send`). Named with the `Lesche` prefix to avoid colliding
/// with the agent→agent `MessageRequest` (the `/agents/{id}/message` route).
///
/// `room` is the optional room id: present when the agent is replying into a
/// multi-member room (the tagma posts the plaintext to
/// `/v1/rooms/{room}/envelopes`); absent for the bilateral 1:1 send. Kept
/// as a raw string so this client crate stays free of agora id-type coupling;
/// the tagma parses it into a `RoomId`.
#[derive(Debug, serde::Serialize)]
pub struct LescheMessageRequest {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
}

/// Re-export of the message-delivery response.
pub type LescheMessageResponse = kallip_common::message::DeliveryResponse;
