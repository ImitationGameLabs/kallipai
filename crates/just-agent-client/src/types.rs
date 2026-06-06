pub(crate) use just_agent_common::protocol::MessageRequest;

/// Re-export of the shared query type under the client-facing name.
pub type ListApprovalsParams = just_agent_common::protocol::ListApprovalsQuery;

/// Re-export of the message response with queue depth feedback.
pub type MessageResponse = just_agent_common::protocol::MessageResponse;
