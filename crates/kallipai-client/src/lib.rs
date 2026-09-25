pub mod client;
pub mod types;

pub use client::{TagmaClient, TagmaClientBuilder};
pub use kallipai_common::agentid::AgentId;
pub use kallipai_common::approval::{ApprovalStatus, ToolCallContent};
pub use kallipai_common::policy::{ExecDecision, ExecOverride, ExecPolicy, PolicyPreset};
pub use kallipai_common::protocol::{
    AgentPermissionsResponse, AgentStatusResponse, AgentSummary, ApiError, ApprovalDecisionBody,
    ApprovalEntry, CreateAgentRequest, CreateAgentResponse, ListAgentsResponse, ListApprovalsQuery,
    ListApprovalsResponse, MessageResponse, TokenBudgetResponse, TokenBudgetUpdateRequest,
    UpdateActivityRequest, UpdateAgentMetadataRequest,
};
pub use kallipai_common::protocol::{
    ClosedReason, TaskCloseRequest, TaskConfirmRequest, TaskCreateRequest, TaskExport,
    TaskForceRequest, TaskListQuery, TaskNoteRequest,
};
pub use kallipai_common::protocol::{InboxEntry, InboxSummary};
pub use types::ListApprovalsParams;
