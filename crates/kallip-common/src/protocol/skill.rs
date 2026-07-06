//! Skill and skill-promote wire types.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::agentid::AgentId;
use crate::promote::{SkillPromoteRecord, SkillPromoteStatus};

/// Response for GET /agents/{id}/skills/paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPathsResponse {
    /// Absolute path to the shared skill directory.
    pub shared: String,
    /// Absolute path to the agent-local skill directory, if available.
    pub local: Option<String>,
}

/// Skill metadata parsed from YAML frontmatter.
///
/// Also used as the response for GET /agents/{id}/skills/{name}/meta.
///
/// **Note:** `name` here is a display label from the frontmatter, not the
/// canonical skill identifier. The skill's unique identity is its path
/// relative to the skills root (e.g. `code/refactoring`), which determines
/// the on-disk location and is used for all lookups and routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Skill promote request wire types (review-based promote flow)
// ---------------------------------------------------------------------------

/// Response for POST /agents/{id}/skills/{name}/promote-request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPromoteSubmitResponse {
    pub request_id: String,
    pub skill_name: String,
    pub status: SkillPromoteStatus,
    /// Whether a shared skill already existed (old content was snapshotted).
    pub has_existing: bool,
}

/// Decision body for POST /skill-promote-requests/{id}.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPromoteDecisionBody {
    pub decision: PromoteDecision,
    pub reason: Option<String>,
}

/// Decision variants for promote-request review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromoteDecision {
    Approve,
    Deny,
}

/// A promote request entry in list/get API responses.
/// Does NOT include content — use the show endpoint for that.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPromoteRecordEntry {
    pub id: String,
    pub skill_name: String,
    /// Whether a shared skill already existed at submission time.
    pub has_existing: bool,
    pub requested_by: AgentId,
    pub status: SkillPromoteStatus,
    pub deny_reason: Option<String>,
    /// Skill description from frontmatter, for reviewer context.
    pub description: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub reviewed_at: Option<OffsetDateTime>,
}

impl SkillPromoteRecordEntry {
    /// Construct from a stored [`SkillPromoteRecord`], omitting content fields.
    pub fn from_record(r: &SkillPromoteRecord) -> Self {
        Self {
            id: r.id.clone(),
            skill_name: r.skill_name.clone(),
            has_existing: r.has_existing,
            requested_by: r.requested_by.clone(),
            status: r.status,
            deny_reason: r.deny_reason.clone(),
            description: r.description.clone(),
            created_at: r.created_at,
            reviewed_at: r.reviewed_at,
        }
    }
}

/// Response for GET /skill-promote-requests/{id} (show endpoint).
/// Includes old/new content for diff review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPromoteShowResponse {
    pub id: String,
    pub skill_name: String,
    /// Whether a shared skill already existed at submission time.
    pub has_existing: bool,
    pub requested_by: AgentId,
    pub status: SkillPromoteStatus,
    pub deny_reason: Option<String>,
    /// Skill description from frontmatter.
    pub description: Option<String>,
    pub old_content: Option<String>,
    pub new_content: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub reviewed_at: Option<OffsetDateTime>,
}

impl SkillPromoteShowResponse {
    /// Construct from a stored [`SkillPromoteRecord`], including content fields.
    pub fn from_record(r: &SkillPromoteRecord) -> Self {
        Self {
            id: r.id.clone(),
            skill_name: r.skill_name.clone(),
            has_existing: r.has_existing,
            requested_by: r.requested_by.clone(),
            status: r.status,
            deny_reason: r.deny_reason.clone(),
            description: r.description.clone(),
            old_content: r.old_content.clone(),
            new_content: r.new_content.clone(),
            created_at: r.created_at,
            reviewed_at: r.reviewed_at,
        }
    }
}

/// Response for GET /skill-promote-requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListSkillPromoteRecordsResponse {
    pub items: Vec<SkillPromoteRecordEntry>,
    pub total: usize,
}
