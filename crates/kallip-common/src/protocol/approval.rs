//! Approval wire types for tagma-client communication.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::agentid::AgentId;
use crate::approval::{ApprovalStatus, ToolCallContent};

/// A single approval entry in API responses.
///
/// Deliberately does NOT carry the classifier's `defer_reason`: that reason is
/// agent-facing (it helps the agent rewrite a deferred command) and lives on the
/// runtime's `ApprovalInfo`. If a future change wants human approvers to see it
/// over HTTP/TUI, add the field here *intentionally* — it is a wire-contract
/// change, not a missing field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalEntry {
    pub id: String,
    pub requested_by: AgentId,
    pub content: ToolCallContent,
    /// Agent-provided justification for the tool call.
    pub commit_reason: Option<String>,
    pub status: ApprovalStatus,
    pub deny_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl ApprovalEntry {
    /// Construct an [`ApprovalEntry`] from an approval info snapshot and the owning agent id.
    ///
    /// Encapsulates the field-by-field mapping so callers don't need to
    /// repeat the construction at every call site.
    pub fn from_info(
        id: String,
        requested_by: AgentId,
        content: ToolCallContent,
        commit_reason: Option<String>,
        status: ApprovalStatus,
        deny_reason: Option<String>,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            requested_by,
            content,
            commit_reason,
            status,
            deny_reason,
            created_at,
        }
    }
}

/// Response for listing approvals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListApprovalsResponse {
    pub items: Vec<ApprovalEntry>,
    pub total: usize,
}

/// Request body for approving or denying an approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalDecisionBody {
    pub decision: String,
    pub reason: Option<String>,
}

/// Query parameters for listing approvals.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ListApprovalsQuery {
    pub offset: Option<u64>,
    /// Page size. Server clamps to [1, 20]; defaults to 5 when unset.
    pub limit: Option<u64>,
    pub requested_by: Option<AgentId>,
    pub status: Option<String>,
    pub order: Option<String>,
}
