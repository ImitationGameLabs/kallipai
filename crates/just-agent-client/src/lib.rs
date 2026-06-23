pub mod client;
pub mod types;

pub use client::{DaemonClient, DaemonClientBuilder};
pub use just_agent_common::agentid::AgentId;
pub use just_agent_common::approval::{ApprovalStatus, ToolCallContent};
pub use just_agent_common::policy::{ExecDecision, ExecPolicy, PolicyDecision, ToolPolicy};
pub use just_agent_common::protocol::{
    AgentPermissionsResponse, AgentStatusResponse, AgentSummary, ApiError, ApprovalDecisionBody,
    ApprovalEntry, CreateAgentRequest, CreateAgentResponse, ListAgentsResponse, ListApprovalsQuery,
    ListApprovalsResponse, ListSkillPromoteRecordsResponse, MessageResponse, PromoteDecision,
    SkillMeta, SkillPathsResponse, SkillPromoteDecisionBody, SkillPromoteShowResponse,
    SkillPromoteSubmitResponse, TokenBudgetResponse, TokenBudgetUpdateRequest,
    UpdateActivityRequest, UpdateAgentMetadataRequest,
};
pub use types::ListApprovalsParams;
