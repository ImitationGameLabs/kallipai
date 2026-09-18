//! HTTP/SSE wire types for tagma-client communication.

pub mod agent;
pub mod approval;
pub mod budget;
pub mod error;
pub mod external;
pub mod inbox;
pub mod skill;
pub mod sse;
pub mod task;
pub mod team;

// Re-export all public types for convenience.
// Downstream `use kallip_common::protocol::*` continues to work unchanged.
pub use agent::{
    ActiveProfile, AgentPermissionsResponse, AgentState, AgentStatusResponse, AgentSummary,
    AgentUsageStats, AttachmentIngestLocalResponse, AttachmentIngestRequest,
    AttachmentIngestResponse, CreateAgentRequest, CreateAgentResponse, DELEGATION_CARVE_OUT,
    DELEGATION_FULL_HANDOFF, DeleteSetResponse, DeliveryMode, DutyStatus, ListAgentsQuery,
    ListAgentsResponse, LockState, MaxToolRounds, MessageRequest, MessageResponse, Modality,
    ParkedReason, ProfileSetUpdateRequest, SetDefaultRequest, SetReference, TagmaUsageTotals,
    UpdateActivityRequest, UpdateAgentMetadataRequest,
};
pub use approval::{
    ApprovalDecisionBody, ApprovalEntry, ListApprovalsQuery, ListApprovalsResponse,
};
pub use budget::{TokenBudgetResponse, TokenBudgetUpdateRequest};
pub use error::ApiError;
pub use external::{AuthoredEvent, SignalEvent};
pub use inbox::{InboxEntry, InboxListQuery, InboxSummary};
pub use skill::{SkillMeta, parse_frontmatter, parse_frontmatter_description};
pub use sse::{FailoverChainExhaustion, SseEvent, TransientRetryInfo};
pub use task::{
    AssociationExport, ClosedReason, EventExport, TaskChainOpRequest, TaskCheckpointRequest,
    TaskCloseRequest, TaskCreateRequest, TaskDispatchRequest, TaskExport, TaskForceRequest,
    TaskListQuery, TaskNoteRequest, TaskStatus, TaskTimeAxis,
};
pub use team::{
    RoleDisposition, TeamAction, TeamActionResult, TeamConvergeOutcome, TeamConvergeRequest,
    TeamConvergeResponse, TeamLockEntry, TeamPlanRow, TeamRejection, TeamRejectionKind,
    TeamRoleStatus, TeamRowOutcome, TeamStatusQuery, TeamStatusResponse,
};
