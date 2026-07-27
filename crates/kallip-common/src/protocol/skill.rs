//! Skill wire types.

use serde::{Deserialize, Serialize};

/// Response for GET /agents/{id}/skills/paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPathsResponse {
    /// Absolute path to the shared skill directory.
    pub shared: String,
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
