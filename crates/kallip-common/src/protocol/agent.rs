//! Agent lifecycle and messaging wire types.

use serde::{Deserialize, Serialize};

use crate::agentid::AgentId;
use crate::context::ContextUsage;
use crate::policy::PolicyPreset;
use crate::retry::RetryRecord;

/// Agent lifecycle state exposed via the status endpoint.
///
/// `Idle`/`Busy` are stored on the live agent as an `AtomicU8` (see `IDLE`/`BUSY`
/// constants) and flipped only by the bridge task. `Faulted` is **wire/display-only**:
/// it reports an entry that could not be brought up (e.g. restore failure) and so has no
/// running task. It is never stored atomically and never written by a bridge -- the
/// `RegistryEntry` enum distinguishes it structurally -- which is why there is no
/// `FAULTED: u8` constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Busy,
    Faulted,
}

impl AgentState {
    pub const IDLE: u8 = 0;
    pub const BUSY: u8 = 1;
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AgentState::Idle => "idle",
            AgentState::Busy => "busy",
            AgentState::Faulted => "faulted",
        })
    }
}

/// Round limit for an agent, set via `CreateAgentRequest::max_tool_rounds`.
///
/// - `None` on the request → use daemon default (`KALLIP_MAX_TOOL_ROUNDS` env var
///   or built-in unlimited).
/// - `Some(Unlimited)` → force no round limit (bounded only by token budget).
/// - `Some(Limited(N))` → explicit round limit (must be > 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxToolRounds {
    /// No hard round limit — bounded only by the daemon-wide token budget.
    Unlimited,
    /// Explicit round limit. Must be greater than zero.
    Limited(usize),
}

/// Request body for creating a new agent instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    pub workspace_root: Option<String>,
    pub skills: Vec<String>,
    pub prompt: Option<String>,
    pub created_by: Option<AgentId>,
    /// Short display label for the agent ("researcher"). Required non-empty for
    /// subagent spawns (`created_by = Some`); optional for root/operator spawns.
    /// Never a unique address — `AgentId` is canonical. Empty means unset.
    #[serde(default)]
    pub role: String,
    /// Longer prose: what this agent is for ("gathers sources for the plan").
    /// Optional, may be empty. Supervisor-owned.
    #[serde(default)]
    pub description: String,
    /// Override the default/env-configured max tool-call rounds for this agent.
    ///
    /// - `None` → use daemon default (`KALLIP_MAX_TOOL_ROUNDS` or unlimited).
    /// - `Some(MaxToolRounds::Unlimited)` → force unlimited rounds.
    /// - `Some(MaxToolRounds::Limited(N))` → explicit limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_rounds: Option<MaxToolRounds>,
    /// Optional explicit FS-access permission class for a subagent spawn, as the
    /// lowercase wire spelling (`"normal"` / `"guest"`). Honored only when
    /// `created_by` is set (subagent path); ignored for root/operator spawns,
    /// whose class is governed by `KALLIP_ROOT_AGENT_PERMISSION_CLASS`.
    ///
    /// `None` → grant the model tier's ceiling (`ceiling_for_tier`), preserving
    /// the historical default. An explicit value is treated as a downgrade
    /// request by the daemon (the reference monitor): it is rejected with
    /// `forbidden` if it exceeds the tier ceiling or the supervisor's own
    /// granted class. The string carries no runtime type here to keep
    /// `kallip-common` free of any `kallip-runtime` dependency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_class: Option<String>,
}

/// Response body returned after creating an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentResponse {
    pub id: AgentId,
}

/// Summary of an agent instance returned in list responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: AgentId,
    pub workspace_root: String,
    pub state: AgentState,
    pub created_by: Option<AgentId>,
    /// Short display label ("researcher"). Empty when unset.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// Longer prose ("gathers sources for the plan"). Empty when unset.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Ephemeral, agent-self-reported current activity ("reading docs/x.md").
    /// Empty when idle (the bridge clears it on terminal events). Not persisted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
    /// Present only when `state == Faulted`: why the agent could not be brought up
    /// (e.g. "restore failed: workspace ... not found"). Absent for live agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faulted_reason: Option<String>,
}

/// Response body for listing agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListAgentsResponse {
    pub agents: Vec<AgentSummary>,
}

/// Query params for `GET /agents`. Omit `created_by` to list all agents (the
/// default); set it to list only the direct subagents of a given superior.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListAgentsQuery {
    #[serde(default)]
    pub created_by: Option<AgentId>,
}

/// Request body for `PUT /agents/{id}/metadata` — update `role` and/or
/// `description`.
///
/// `None` fields are left unchanged; `Some(s)` sets the field. `role: Some(s)`
/// must be non-empty (the handler validates this — an explicit set must not be
/// empty). Only the direct supervisor (or operator) may call this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAgentMetadataRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Request body for `PUT /agents/{id}/activity` — the agent reports its current
/// activity as free text. Only the agent itself (or operator) may call this.
/// An empty string clears the activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateActivityRequest {
    #[serde(default)]
    pub activity: String,
}

/// Combined agent status: lifecycle state + context usage + recent retry history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusResponse {
    pub state: AgentState,
    pub context: ContextUsage,
    pub recent_retries: Vec<RetryRecord>,
    /// Daemon-wide token consumption budget (shared by all agents).
    pub token_budget: u64,
    /// Cumulative daemon-wide tokens consumed toward the budget.
    pub token_consumed: u64,
    /// Ephemeral, agent-self-reported current activity. Empty when idle.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
}

/// Request body for sending a message to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRequest {
    pub text: String,
}

/// Response body for sending a message to an agent.
///
/// Includes queue depth feedback so callers can gauge expected latency:
/// - `queue_depth == 0`: agent will process the message immediately.
/// - `queue_depth > 0`: message is queued behind existing messages; a
///   warning is included.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageResponse {
    /// Approximate number of messages queued ahead of this one (0 = immediate processing).
    pub queue_depth: usize,
    /// Human-readable note when queue is non-empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// Response for GET /agents/{id}/permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPermissionsResponse {
    pub max_depth: u8,
    pub workspace_root: String,
    pub created_by: Option<AgentId>,
    /// The daemon-global `bash_exec` classify rule-set in effect for this agent
    /// (read-only — it is set once at daemon startup from `KALLIP_POLICY_PRESET`).
    pub preset: PolicyPreset,
    /// FS-access permission class actually granted to this agent, as the
    /// lowercase wire spelling (`"normal"` / `"guest"`) — the value the daemon
    /// clamped at spawn and re-validates on restore. Surfaced here (it was
    /// previously invisible to clients) so an explicit downgrade is observable
    /// and verifiable. String-typed to keep `kallip-common` runtime-free.
    pub permission_class: String,
}
