//! HTTP/SSE wire types for daemon-client communication.

pub mod agent;
pub mod approval;
pub mod budget;
pub mod error;
pub mod skill;
pub mod sse;

// Re-export all public types for convenience.
// Downstream `use just_agent_common::protocol::*` continues to work unchanged.
pub use agent::{
    AgentPermissionsResponse, AgentState, AgentStatusResponse, AgentSummary, CreateAgentRequest,
    CreateAgentResponse, ListAgentsQuery, ListAgentsResponse, MaxToolRounds, MessageRequest,
    MessageResponse, UpdateActivityRequest, UpdateAgentMetadataRequest,
};
pub use approval::{
    ApprovalDecisionBody, ApprovalEntry, ListApprovalsQuery, ListApprovalsResponse,
};
pub use budget::{DEFAULT_TOKEN_BUDGET, TokenBudgetResponse, TokenBudgetUpdateRequest};
pub use error::ApiError;
pub use skill::{
    ListSkillPromoteRecordsResponse, PromoteDecision, SkillMeta, SkillPathsResponse,
    SkillPromoteDecisionBody, SkillPromoteRecordEntry, SkillPromoteShowResponse,
    SkillPromoteSubmitResponse,
};
pub use sse::{FailoverChainExhaustion, SseEvent};
