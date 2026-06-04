//! Skill promote request types.
//!
//! Defines the domain types for the skill promote request system.
//! Agents submit promote requests to make their local skills available
//! in the shared directory. Root agents or operators review and decide.

use serde::{Deserialize, Serialize};

use crate::agentid::AgentId;

/// Placeholder text used when a promote request is denied without a reason.
/// Shared by the CLI display and daemon notification code so the wording
/// stays consistent. The stored `deny_reason` is always `None` in this case;
/// only renderers substitute this text.
pub const NO_REASON_PROVIDED: &str = "(no reason provided)";

/// Status of a skill promote request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPromoteStatus {
    Pending,
    Approved,
    Denied,
}

impl SkillPromoteStatus {
    /// Parse a status string (e.g. from a query parameter).
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "denied" => Some(Self::Denied),
            _ => None,
        }
    }
}

impl std::fmt::Display for SkillPromoteStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => f.write_str("pending"),
            Self::Approved => f.write_str("approved"),
            Self::Denied => f.write_str("denied"),
        }
    }
}

/// Input for creating a new promote request.
///
/// Self-documenting alternative to passing many positional parameters to
/// [`SkillPromoteStore::create`].
pub struct CreatePromoteRequest {
    /// Skill path relative to the skills root (e.g. `code/refactoring`).
    pub skill_name: String,
    /// Whether a shared skill already existed at submission time.
    pub has_existing: bool,
    /// Local skill content being promoted.
    pub new_content: String,
    /// Current shared skill content, if any (`None` = new skill).
    pub old_content: Option<String>,
    /// Description parsed from frontmatter. Convenience field for reviewers.
    pub description: Option<String>,
    /// Agent that submitted the request.
    pub requested_by: AgentId,
}

/// A skill promotion request record stored in `SkillPromoteStore`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillPromoteRecord {
    /// Unique ID, prefixed "spr_".
    pub id: String,
    /// Skill name being promoted.
    pub skill_name: String,
    /// Whether a shared skill already existed at submission time.
    pub has_existing: bool,
    /// New content: snapshotted from the agent's local skill file.
    pub new_content: String,
    /// Old content: snapshotted from the current shared skill file, if any.
    /// `None` means the shared skill did not exist at submission time.
    pub old_content: Option<String>,
    /// Skill description parsed from new content's frontmatter.
    /// Convenience field for reviewer context — derived from `new_content`
    /// at submission time and never re-derived.
    pub description: Option<String>,
    /// Agent that submitted the request.
    pub requested_by: AgentId,
    /// Status of the request.
    pub status: SkillPromoteStatus,
    /// Reason for denial. `None` when the request was denied without a
    /// reason. The display and notification layers substitute
    /// [`NO_REASON_PROVIDED`] when rendering; the stored value must remain
    /// `None` to preserve the semantic distinction.
    pub deny_reason: Option<String>,
    /// When the request was submitted.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// When the request was approved or denied.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub reviewed_at: Option<time::OffsetDateTime>,
}
