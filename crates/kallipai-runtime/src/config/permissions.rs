//! FS-access and delegation permissions for the agent sandbox.
//!
//! Owns the static sandbox axes: [`PermissionClass`] (FS-access baseline,
//! lowercase wire spelling), [`DelegationMode`] (how a subagent relates to
//! the supervisor's workspace write-lock), and [`PermissionProfile`]
//! (delegation depth and workspace boundary, seeded from [`DEFAULT_MAX_DEPTH`]).
//! Re-exported by `crate::config`, so the `config::` paths stay stable.

use std::path::PathBuf;

/// Hard-coded maximum delegation depth for top-level agents.
///
/// Not configurable — hard-coding avoids the complexity of persisting and
/// re-validating a dynamic value across restarts. The depth is recomputed
/// from the `created_by` chain on restore (depth = Self - chain length),
/// eliminating any attack surface from tampered `meta.json`. A future
/// increase to this constant will cover all reasonable delegation needs
/// once the chain-walking restore path is sufficiently tested.
pub const DEFAULT_MAX_DEPTH: u8 = 3;

/// FS-access permission class — the static baseline axis of the agent sandbox.
///
/// Granted explicitly at spawn (never above the supervisor's own class) and
/// re-validated against the supervisor chain on restore. `Ord` is derived
/// (`Guest < Normal`) so the invariant `class(child) <= class(supervisor)`
/// is a plain comparison. Persisted on `AgentMeta` (a safety invariant,
/// unlike display fields).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum PermissionClass {
    /// Guest: readonly — workspace RO, secret zero-access, no home write.
    Guest,
    /// Normal: home broad-write + workspace write. Default for root agents.
    #[default]
    Normal,
}

/// How a subagent relates to its supervisor's workspace write-lock.
///
/// Serialized `snake_case` for both the wire (`CreateAgentRequest`) and the
/// persisted (`AgentMeta`) form. This intentionally diverges from
/// [`PermissionClass`], which keeps a PascalCase persisted form distinct from its
/// lowercase wire/env spelling: `DelegationMode` is newer and has no env-var
/// spelling, so one shared lowercase form is simpler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationMode {
    /// The subagent scopes into a proper subdirectory of the supervisor's
    /// workspace (the default). The supervisor keeps its root write-lock; the
    /// subdirectory becomes a readonly hole to the supervisor via the delegation
    /// carve-out.
    #[default]
    CarveOut,
    /// The subagent takes the supervisor's *entire* workspace: the supervisor's
    /// root write-lock is transferred to the child at spawn and transferred back
    /// on removal, so the supervisor's next shell loses workspace write until the
    /// child is gone.
    ///
    /// Exclusive: a supervisor with a `FullHandoff` child may have no other
    /// child (CarveOut or FullHandoff). Enforced at spawn; a legacy/corrupt
    /// on-disk tree that violates it may either fault on restore (one
    /// interleaving) or silently overlap (the other) -- normal operation never
    /// produces such a tree, so this is a defense note, not a live concern.
    ///
    /// Reactivation: while a `FullHandoff` child is Live the workspace write-lock
    /// is reassigned to the child, so `release_all(supervisor)` is a no-op and
    /// the supervisor's reactivation `try_acquire_workspace_lock` returns
    /// `Busy { holder: child }`. The supervisor therefore cannot reactivate
    /// (restart its task) until the child is removed -- the supervisor genuinely
    /// cannot write its workspace while the child holds the lock.
    FullHandoff,
}

impl std::str::FromStr for DelegationMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            kallipai_common::protocol::DELEGATION_CARVE_OUT => Ok(Self::CarveOut),
            kallipai_common::protocol::DELEGATION_FULL_HANDOFF => Ok(Self::FullHandoff),
            other => Err(format!(
                "unknown delegation_mode '{other}' (expected '{}' or '{}')",
                kallipai_common::protocol::DELEGATION_CARVE_OUT,
                kallipai_common::protocol::DELEGATION_FULL_HANDOFF
            )),
        }
    }
}

impl std::fmt::Display for DelegationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CarveOut => f.write_str(kallipai_common::protocol::DELEGATION_CARVE_OUT),
            Self::FullHandoff => f.write_str(kallipai_common::protocol::DELEGATION_FULL_HANDOFF),
        }
    }
}

/// Error returned when a [`PermissionClass`] cannot be parsed from its lowercase
/// wire/env spelling. Surfaced by the tagma as a `400 Bad Request` body, so the
/// message stays client-readable and stable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePermissionClassError(pub String);

impl std::fmt::Display for ParsePermissionClassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid permission class '{}' (expected \"normal\" or \"guest\")",
            self.0
        )
    }
}

impl std::error::Error for ParsePermissionClassError {}

/// Lowercase wire/env spelling: `"normal"` / `"guest"`. This is the inverse of
/// [`PermissionClass`]'s [`std::fmt::Display`] and matches the
/// `KALLIPAI_ROOT_AGENT_PERMISSION_CLASS` env-var convention — distinct from the
/// PascalCase serde form persisted in `meta.json`. Parsing is intentionally
/// trim-free; callers decide whether to trim surrounding whitespace.
impl std::str::FromStr for PermissionClass {
    type Err = ParsePermissionClassError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "normal" => Ok(PermissionClass::Normal),
            "guest" => Ok(PermissionClass::Guest),
            other => Err(ParsePermissionClassError(other.to_owned())),
        }
    }
}

/// Lowercase wire/env spelling (`"normal"` / `"guest"`), the inverse of
/// [`std::str::FromStr`]. Used by the permissions endpoint and by client-facing
/// error messages so they stay consistent with the wire form (rather than the
/// PascalCase `Debug`/serde form).
impl std::fmt::Display for PermissionClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PermissionClass::Guest => f.write_str("guest"),
            PermissionClass::Normal => f.write_str("normal"),
        }
    }
}

/// Permission profile controlling agent delegation capabilities.
#[derive(Clone, Debug)]
pub struct PermissionProfile {
    /// Remaining delegation levels. Decremented for each subagent.
    pub max_depth: u8,
    /// Workspace boundary. Subagents must operate within their supervisor's workspace.
    pub workspace_root: PathBuf,
}

impl PermissionProfile {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            workspace_root,
        }
    }

    /// Create a profile for a subagent with decremented depth.
    pub fn subagent(workspace_root: PathBuf, supervisor_depth: u8) -> Self {
        Self {
            max_depth: supervisor_depth.saturating_sub(1),
            workspace_root,
        }
    }
}
