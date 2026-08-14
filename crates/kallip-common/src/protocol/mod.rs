//! HTTP/SSE wire types for tagma-client communication.

pub mod agent;
pub mod approval;
pub mod budget;
pub mod error;
pub mod external;
pub mod inbox;
pub mod skill;
pub mod sse;

// Re-export all public types for convenience.
// Downstream `use kallip_common::protocol::*` continues to work unchanged.
pub use agent::{
    AgentPermissionsResponse, AgentState, AgentStatusResponse, AgentSummary, CreateAgentRequest, DutyStatus,
    CreateAgentResponse, DELEGATION_CARVE_OUT, DELEGATION_FULL_HANDOFF, ListAgentsQuery,
    ListAgentsResponse, MaxToolRounds, MessageRequest, MessageResponse, UpdateActivityRequest,
    UpdateAgentMetadataRequest,
};
pub use approval::{
    ApprovalDecisionBody, ApprovalEntry, ListApprovalsQuery, ListApprovalsResponse,
};
pub use budget::{DEFAULT_TOKEN_BUDGET, TokenBudgetResponse, TokenBudgetUpdateRequest};
pub use error::ApiError;
pub use external::{AuthoredEvent, SignalEvent};
pub use skill::{SkillMeta, parse_frontmatter, parse_frontmatter_description};
pub use sse::{FailoverChainExhaustion, SseEvent};
pub use inbox::{InboxEntry, InboxListQuery, InboxSummary};
