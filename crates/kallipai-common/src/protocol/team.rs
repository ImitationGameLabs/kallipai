//! Team domain wire types: the declarative team-management status face.
//!
//! The read-only three-way comparison — declaration vs lock vs live
//! registry — one row per role, shared by the status face and
//! converge's plan segment, so the table a CLI prints and the plan
//! converge executes cannot drift apart.

use serde::{Deserialize, Serialize};

use crate::agentid::AgentId;
use crate::declaration::RoleDeclaration;

/// Query params for `GET /team/status`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TeamStatusQuery {
    /// Filesystem path of the declaration file to read and parse. The
    /// tagma and the CLI share the machine and the parser, so a path is
    /// enough — the file content never crosses the wire.
    pub file: String,
    /// The CLI's lock mapping, as comma-separated `role:id` pairs. The
    /// lock is a CLI-side archive the tagma never holds or locates: the
    /// CLI folds its parsed content into the request. Omit for an empty
    /// lock (nothing has converged yet).
    #[serde(default)]
    pub lock: Option<String>,
}

/// What converge would decide for one role — a presence-level verdict.
///
/// Presence-level: rows compare existence (declared? locked? live?),
/// not field values; metadata alignment is converge's plan-segment
/// business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleDisposition {
    /// Declared, live, and lock-covered: in sync, nothing to do.
    Active,
    /// Declared and live with no lock record: the adoption clause
    /// applies (keep the agent, record it in the lock).
    Adopt,
    /// Declared, not live, and the lock's id sits in the inactive
    /// area: converge would restore it identity-intact.
    Restore,
    /// Declared and not live with nothing restorable: fresh spawn.
    Spawn,
    /// Live but not declared: converge would park it in the inactive
    /// area.
    Deactivate,
    /// Declared `unmanaged` and live: exempt from converge, listed
    /// explicitly — exemption is a visible state, never invisibility.
    Exempt,
    /// More than one live agent carries the role: converge rejects the
    /// whole batch until a human resolves the duplicates.
    Duplicate,
    /// Nothing to do and nothing to decide: a lock record whose role is
    /// undeclared and whose agent is not live (a dormant archive
    /// entry), or a declared-unmanaged role with no live body.
    Retain,
}

/// One row of the three-way comparison:
/// role | declaration | lock | live | verdict.
///
/// The `Option` fields skip on serialization on purpose: a missing
/// declaration or lock record is a meaningful absence (that side has
/// no entry for the role), distinct from an empty value like `live`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamRoleStatus {
    pub role: String,
    /// The declaration entry when the role is declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaration: Option<RoleDeclaration>,
    /// The lock record's agent id when the request carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_id: Option<AgentId>,
    /// True when the lock record's id is live under a DIFFERENT role —
    /// the record drifted: the id is alive but owned elsewhere, the
    /// tagma's own root included. Converge discards drifted records.
    pub lock_drift: bool,
    /// True when the lock record's id is parked in the inactive area —
    /// what separates `restore` from `spawn` for absent roles.
    pub lock_inactive: bool,
    /// True when a declared role is named `root`: the tagma's root is
    /// boot-built and outside the declaration's reach, so this role can
    /// never converge as a plain spawn — converge's plan segment rejects
    /// the batch instead. Surfaced so the table shows the dead end
    /// rather than a spawnable role.
    pub root_conflict: bool,
    /// Live agent ids carrying this role (root excluded; more than one
    /// is the duplicate case).
    pub live: Vec<AgentId>,
    pub disposition: RoleDisposition,
}

/// Response body for `GET /team/status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamStatusResponse {
    /// The declaration path the comparison ran against, echoed.
    pub declaration_path: String,
    /// One row per role in the union `declaration ∪ lock ∪ live`:
    /// declaration order first, then undeclared roles by name.
    pub roles: Vec<TeamRoleStatus>,
}

/// Request body for `POST /team/converge`. The three-way inputs as the
/// status face — the declaration path and the CLI's lock mapping in the
/// request, the live side taken from the server's registry snapshot —
/// plus the two escalation flags.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TeamConvergeRequest {
    /// Filesystem path of the declaration file to converge against.
    pub file: String,
    /// The CLI's lock mapping, as comma-separated `role:id` pairs —
    /// same spelling as [`TeamStatusQuery::lock`]. Omit for an empty
    /// lock (nothing has converged yet).
    #[serde(default)]
    pub lock: Option<String>,
    /// Plan only: produce the action list and the would-be lock
    /// mapping without touching the registry or the disk.
    #[serde(default)]
    pub dry_run: bool,
    /// Escalation: preflight passes busy deactivation targets, and
    /// each one is interrupted before teardown. The escape is recorded
    /// in the affected result rows.
    #[serde(default)]
    pub force: bool,
}

/// What converge would do for one role. Presence-level verbs; field
/// alignment rides on the row that keeps the body (adopt rows may
/// also carry metadata alignment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamAction {
    /// Fresh spawn of a declared role with no restorable body.
    Spawn,
    /// Bring a parked inactive body back identity-intact (lock's id
    /// found in the inactive area). No prompt is injected.
    Restore,
    /// Keep a live body and record it in the lock: the role is
    /// declared, the body is live, and no lock record names it.
    Adopt,
    /// Park a live body in the inactive area (undeclared role, not
    /// exempt). The lock keeps the binding so the role can come back.
    Deactivate,
    /// The live body stays; declared metadata differs and is aligned
    /// in the same round (description, profile set, permission class
    /// downgrades only). Non-alignable fields are annotated.
    AlignMetadata,
    /// Nothing to do: an in-sync live role, an exempt (unmanaged)
    /// role, or a dormant record.
    Retain,
}

/// One row of converge's plan: the verb, the body it lands on, and
/// the annotations an operator should read before running without
/// `dry_run` (drifted or discarded lock records, prompt fields with
/// no alignment basis, exempt bodies).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamPlanRow {
    pub role: String,
    pub action: TeamAction,
    /// The agent the action lands on. `None` for planned spawns: the
    /// id is minted at execution time, never at plan time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    /// Human-readable annotations (lock-record discards, alignment
    /// caveats). Empty when the row is unremarkable.
    #[serde(default)]
    pub notes: Vec<String>,

    /// The presence-level verdict behind the row (duplicate, restore,
    /// spawn, ...). Programs branch on this instead of matching note
    /// text; `notes` stays the human-readable face.
    pub disposition: RoleDisposition,
}
/// Why preflight refused the batch: a machine-readable kind plus the
/// human-facing message. Programs branch on the kind — the CLI's
/// `--drain` loop waits specifically on busy rejections — while the
/// message is what the operator reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRejectionKind {
    /// One id mapped to two roles in the submitted lock.
    LockAmbiguity,
    /// The declaration claims the reserved `root` role.
    RootRole,
    /// Multiple live bodies carry one declared role.
    Duplicate,
    /// A deactivation target is mid-task; `force` is the escape.
    Busy,
    /// A deactivation target still has live subagents.
    LiveChildren,
    /// A stale inactive body would collide with the rename.
    StaleInactive,
    /// The declaration names no profile_set.
    SpawnProfileSet,
    /// The declared permission_class is unknown or exceeds the root's.
    SpawnProfileClass,
    /// A declared skill cannot be loaded.
    SpawnSkill,
    /// The tagma root is not live; declared roles spawn under it.
    SpawnRootDown,
    /// The batch would end over the global agent limit.
    CapacityAgents,
    /// The root would end over its subagent limit.
    CapacityChildren,
}

/// One preflight refusal: kind for programs, message for operators.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamRejection {
    pub kind: TeamRejectionKind,
    pub message: String,
}

/// How one executed action landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRowOutcome {
    /// The action was applied.
    Applied,
    /// The action failed; the whole batch stopped here (fail-fast).
    /// Already-applied rows stay applied; re-run converge to plan
    /// from the current reality.
    Failed,
}

/// One executed action's result row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamActionResult {
    pub role: String,
    pub action: TeamAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    pub outcome: TeamRowOutcome,
    /// Confirmation or the failure reason.
    pub detail: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// The converge run's overall outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamConvergeOutcome {
    /// Dry run: the plan is advisory, nothing was applied.
    Planned,
    /// Every planned action was applied.
    Applied,
    /// Execution stopped early (a pre-execution state re-read found
    /// the plan→execute window open, or an action failed). Applied
    /// rows are listed; the returned mapping is the input records that
    /// survived planning, updated by the rows that landed.
    Aborted,
    /// Preflight refused the whole batch: zero actions were applied.
    Rejected,
}

/// One entry of the post-converge lock mapping the CLI writes to
/// `tagma.lock` (temp+rename, CLI-side archive). Full-set semantics:
/// dormant roles keep their records so a declaration can re-adopt
/// them later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamLockEntry {
    pub role: String,
    pub id: AgentId,
    /// RFC 3339 UTC timestamp of the converge run that produced this
    /// mapping — the binding's last confirmation, not necessarily its
    /// first establishment.
    pub converged_at: String,
}

/// Response body for `POST /team/converge`.
///
/// HTTP status: `200` for every evaluated run (including a preflight
/// rejection — the structured per-row findings ride in the body),
/// `409` only for the global mutex (`converge already in progress`),
/// which carries no plan body to lose.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConvergeResponse {
    pub declaration_path: String,
    pub dry_run: bool,
    pub outcome: TeamConvergeOutcome,
    /// The full plan, present for every outcome — the operator reads
    /// the same rows whether they were executed, refused, or aborted.
    pub plan: Vec<TeamPlanRow>,
    /// Executed action results (applied and aborted runs). Empty for
    /// planned and rejected runs.
    pub results: Vec<TeamActionResult>,
    /// Preflight refusal reasons (rejected runs), each naming the
    /// role or agent involved and the fix.
    pub rejections: Vec<TeamRejection>,
    /// The lock mapping to write. For an applied run this is the
    /// full post-converge set; for an aborted run it covers applied
    /// rows only; for a planned (dry) run it is the mapping modulo
    /// planned spawns, whose ids do not exist yet. Rejected runs
    /// return the input lock, pruned of discarded records.
    pub lock: Vec<TeamLockEntry>,
}
