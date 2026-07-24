pub(crate) use kallip_common::protocol::MessageRequest;

/// Re-export of the shared query type under the client-facing name.
pub type ListApprovalsParams = kallip_common::protocol::ListApprovalsQuery;

/// Re-export of the message response with queue depth feedback.
pub type MessageResponse = kallip_common::protocol::MessageResponse;

/// Request body for `POST /agents/{id}/lesche/messages` (the agent's
/// `kallip lesche send`). Named with the `Lesche` prefix to avoid colliding
/// with the agent→agent `MessageRequest` (the `/agents/{id}/message` route).
#[derive(Debug, serde::Serialize)]
pub struct LescheMessageRequest {
    pub text: String,
}

/// Re-export of the message-delivery response.
pub type LescheMessageResponse = kallip_common::message::DeliveryResponse;
