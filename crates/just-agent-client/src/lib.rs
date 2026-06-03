pub mod client;
pub mod types;

pub use client::DaemonClient;
pub use just_agent_common::agentid::AgentId;
pub use just_agent_common::approval::{ApprovalStatus, ToolCallContent};
pub use just_agent_common::policy::{PolicyDecision, ToolPolicy};
pub use just_agent_common::protocol::{
    AgentPermissionsResponse, AgentStatusResponse, AgentSummary, ApprovalDecisionBody,
    ApprovalEntry, CreateAgentRequest, CreateAgentResponse, ListAgentsResponse, ListApprovalsQuery,
    ListApprovalsResponse,
};
pub use types::ListApprovalsParams;
