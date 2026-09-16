use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::io::atomic_write;
use super::layout::agent_dir;
use kallip_common::AgentId;

/// Create agent directory and write initial meta.json.
pub fn create_agent_dir(agent_id: &AgentId, meta: AgentMeta) -> Result<PathBuf> {
    let dir = agent_dir(agent_id)?;
    std::fs::create_dir_all(&dir)?;

    atomic_write(
        &dir.join("meta.json"),
        &serde_json::to_string_pretty(&meta)?,
    )?;

    Ok(dir)
}

/// Read-modify-write `meta.json` to update `role`, `description`, and/or the
///
/// Used by `PUT /agents/{id}/metadata` and `PUT /agents/{id}/profile-set`.
/// Reads the current meta, applies the values, and atomically rewrites —
/// preserving whatever it did not touch. `None` leaves a field unchanged;
/// `Some(s)` sets it (for `profile_set`, the exact set name).
///
/// Callers hold the registry write-lock across the file I/O and the in-memory
/// update (persist-first, then `AgentConfig`) so disk and memory commit as
/// one unit. `check_meta` is startup-only, so the meta writers never run
/// concurrently.
pub fn rewrite_meta(
    dir: &Path,
    role: Option<&str>,
    description: Option<&str>,
    profile_set: Option<&str>,
    permissions_class: Option<crate::config::PermissionClass>,
) -> Result<()> {
    let path = dir.join("meta.json");
    let json = fs::read_to_string(&path).context("reading meta.json")?;
    let mut meta: AgentMeta = serde_json::from_str(&json).context("parsing meta.json")?;
    if let Some(r) = role {
        meta.role = r.to_owned();
    }
    if let Some(d) = description {
        meta.description = d.to_owned();
    }
    if let Some(s) = profile_set {
        meta.profile_set = Some(s.to_owned());
    }
    if let Some(c) = permissions_class {
        meta.permissions_class = c;
    }
    atomic_write(&path, &serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

/// Minimal metadata persisted per agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMeta {
    pub workspace_root: PathBuf,
    /// Supervisor agent ID (for subagents).
    #[serde(default, rename = "created_by")]
    pub created_by: Option<AgentId>,
    /// Short display label ("researcher"). Supervisor-owned; mirrored from
    /// [`AgentConfig`](crate::config::AgentConfig). Empty means unset.
    #[serde(default)]
    pub role: String,
    /// Longer prose ("gathers sources for the plan"). Supervisor-owned.
    #[serde(default)]
    pub description: String,
    /// FS-access permission class (Guest readonly / Normal home-rw). A safety
    /// invariant — persisted so restore can re-validate it against the
    /// supervisor chain (unlike `role`/`description`, which are display-only).
    /// Defaults to Normal for legacy `meta.json` files written before this
    /// field existed.
    #[serde(default)]
    pub permissions_class: crate::config::PermissionClass,
    /// Profile-set binding (by name), resolved at spawn and re-read on
    /// restore. `None` marks a record written before set binding existed;
    /// restore surfaces such agents as unspecified instead of guessing.
    #[serde(default)]
    pub profile_set: Option<String>,
    /// Workspace delegation mode (`carve_out` subdir vs `full_handoff`
    /// whole-workspace). Persisted so restore reproduces the supervisor lock
    /// transfer for a FullHandoff child. Defaults to CarveOut for legacy metas.
    #[serde(default)]
    pub delegation_mode: crate::config::DelegationMode,
}

/// Read an agent's meta.json without side effects.
/// Used for validating supervisor chains at restore time.
pub fn read_meta(agent_id: &AgentId) -> Result<AgentMeta> {
    let path = agent_dir(agent_id)?.join("meta.json");
    let json = fs::read_to_string(&path).context("reading meta.json")?;
    serde_json::from_str(&json).context("parsing meta.json")
}

/// Read an agent's meta.json directly from its directory.
/// Use when the directory path is already known (e.g., budget updates)
/// to avoid re-deriving the path from the agent ID.
pub fn read_meta_from_dir(dir: &Path) -> Result<AgentMeta> {
    let json = fs::read_to_string(dir.join("meta.json")).context("reading meta.json")?;
    serde_json::from_str(&json).context("parsing meta.json")
}

#[cfg(test)]
mod tests;
