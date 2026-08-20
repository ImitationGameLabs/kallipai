//! Agent lifecycle and messaging wire types.

use serde::{Deserialize, Serialize};

use super::sse::{FailoverChainExhaustion, TransientRetryInfo};
use crate::agentid::AgentId;
use crate::context::ContextUsage;
use crate::policy::PolicyPreset;
use crate::retry::RetryRecord;

/// Agent lifecycle state exposed via the status endpoint.
///
/// `Idle`/`Busy`/`Waiting`/`Parked`/`Retrying` are stored on the live agent as an
/// `AtomicU8` (see the constants) and flipped only by the bridge task.
/// `Faulted` is **wire/display-only**: it reports an entry that could not be brought
/// up (e.g. restore failure) and so has no running task. It is never stored
/// atomically and never written by a bridge -- the `RegistryEntry` enum
/// distinguishes it structurally -- which is why there is no `FAULTED: u8`
/// constant.
///
/// `Retrying` covers chain-transient backoff (a terminal
/// [`SseEvent::FailoverChainExhausted`](super::SseEvent::FailoverChainExhausted)
/// armed a delayed retry) and doubles as the display value for in-request
/// backoff (the non-terminal `retrying`/`streamReset` events): the latter is a
/// bridge-side overlay while the true stored state is `Busy`, not a distinct
/// stored state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Busy,
    Waiting,
    Parked,
    Retrying,
    Faulted,
}

impl AgentState {
    pub const IDLE: u8 = 0;
    pub const BUSY: u8 = 1;
    pub const WAITING: u8 = 2;
    pub const PARKED: u8 = 3;
    pub const RETRYING: u8 = 4;
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AgentState::Idle => "idle",
            AgentState::Busy => "busy",
            AgentState::Waiting => "waiting",
            AgentState::Parked => "parked",
            AgentState::Retrying => "retrying",
            AgentState::Faulted => "faulted",
        })
    }
}

/// Why an agent is [`AgentState::Parked`] — structured, not free text, so the UI
/// can render and filter by cause (the failover-case lesson: surface the real
/// state, never derive it). Written by the bridge at the parking terminal event,
/// mirrored into status responses alongside `parked_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParkedReason {
    /// Failover chain exhausted with no transient retry armed (or retries already
    /// spent): re-prompting needs operator action (reconfigure failover, or kick).
    FailoverChainExhausted {
        reason: FailoverChainExhaustion,
        detail: String,
    },
    /// Undifferentiated fatal turn error.
    FatalError {
        message: String,
    },
    TokenBudgetExceeded {
        consumed: u64,
        budget: u64,
    },
    MaxRoundsExceeded,
    /// Chain-transient retries spent their attempt budget; the final FCE parked the
    /// agent instead of re-arming another backoff.
    TransientRetryExhausted,
}

impl std::fmt::Display for ParkedReason {
    /// Operator-readable prose, shared by the park-kick turn text and TUI rendering.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FailoverChainExhausted { reason, .. } => {
                write!(f, "failover chain exhausted ({reason})")
            }
            Self::FatalError { message } => write!(f, "fatal error: {message}"),
            Self::TokenBudgetExceeded { .. } => f.write_str("token budget exceeded"),
            Self::MaxRoundsExceeded => f.write_str("max rounds exceeded"),
            Self::TransientRetryExhausted => f.write_str("transient retries exhausted"),
        }
    }
}

/// Round limit for an agent, set via `CreateAgentRequest::max_tool_rounds`.
///
/// - `None` on the request → use tagma default (`KALLIP_MAX_TOOL_ROUNDS` env var
///   or built-in unlimited).
/// - `Some(Unlimited)` → force no round limit (bounded only by token budget).
/// - `Some(Limited(N))` → explicit round limit (must be > 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxToolRounds {
    /// No hard round limit — bounded only by the tagma-wide token budget.
    Unlimited,
    /// Explicit round limit. Must be greater than zero.
    Limited(usize),
}

/// Workspace delegation mode wire spellings, as carried by
/// [`CreateAgentRequest::delegation_mode`] and parsed by the runtime's
/// `DelegationMode::FromStr`. The single source for the on-wire spelling so the
/// runtime, the tagma, and the CLI cannot drift (this crate is deliberately
/// runtime-free, so the constants live here rather than on the enum).
pub const DELEGATION_CARVE_OUT: &str = "carve_out";
pub const DELEGATION_FULL_HANDOFF: &str = "full_handoff";

/// Request body for creating a new agent instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    /// Required working directory. Rejected if absent (never silently defaulted).
    pub workspace_root: String,
    pub skills: Vec<String>,
    pub prompt: Option<String>,
    pub created_by: Option<AgentId>,
    /// Short display label for the agent ("researcher"). Subagent spawns are the
    /// only HTTP create path (`created_by = Some` is required; the tagma's root
    /// is created at startup, not over HTTP) and require a non-empty role.
    /// Never a unique address — `AgentId` is canonical. Empty means unset.
    #[serde(default)]
    pub role: String,
    /// Longer prose: what this agent is for ("gathers sources for the plan").
    /// Optional, may be empty. Supervisor-owned.
    #[serde(default)]
    pub description: String,
    /// Override the default/env-configured max tool-call rounds for this agent.
    ///
    /// - `None` → use tagma default (`KALLIP_MAX_TOOL_ROUNDS` or unlimited).
    /// - `Some(MaxToolRounds::Unlimited)` → force unlimited rounds.
    /// - `Some(MaxToolRounds::Limited(N))` → explicit limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_rounds: Option<MaxToolRounds>,
    /// Optional explicit FS-access permission class for a subagent spawn, as the
    /// lowercase wire spelling (`"normal"` / `"guest"`). Subagent spawns are the
    /// only HTTP create path (`created_by` is required); the tagma's own root
    /// takes its class at startup from `KALLIP_ROOT_AGENT_PERMISSION_CLASS`,
    /// not from this field.
    ///
    /// `None` → grant the model tier's ceiling (`ceiling_for_tier`), preserving
    /// the historical default. An explicit value is treated as a downgrade
    /// request by the tagma (the reference monitor): it is rejected with
    /// `forbidden` if it exceeds the tier ceiling or the supervisor's own
    /// granted class. The string carries no runtime type here to keep
    /// `kallip-common` free of any `kallip-runtime` dependency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_class: Option<String>,
    /// Optional workspace delegation mode for a subagent spawn, as the lowercase
    /// wire spelling ([`DELEGATION_CARVE_OUT`] / [`DELEGATION_FULL_HANDOFF`]). Omit
    /// (or [`DELEGATION_CARVE_OUT`]) for the default: the subagent scopes into a
    /// subdirectory of the supervisor's workspace. [`DELEGATION_FULL_HANDOFF`]
    /// transfers the supervisor's entire workspace write-lock to the child for its
    /// lifetime (exclusive: the supervisor may have no other child while it lives).
    /// String-typed to keep `kallip-common` free of a `kallip-runtime` dependency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegation_mode: Option<String>,
}

/// Response body returned after creating an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentResponse {
    pub id: AgentId,
}

/// Whether an agent is on-duty (accepting messages) or off-duty (buffering to inbox).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DutyStatus {
    #[default]
    OnDuty,
    OffDuty,
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
    /// Whether the agent is on-duty or off-duty (off-duty agents buffer messages).
    #[serde(default)]
    pub duty: DutyStatus,
    /// Present only when `state == Faulted`: why the agent could not be brought up
    /// (e.g. "restore failed: workspace ... not found"). Absent for live agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faulted_reason: Option<String>,
    /// The external conversation id for this agent's chat — present ONLY on the
    /// root agent summary (`GET /agents/root`), absent for every other agent and
    /// when the tagma is not enrolled (pure-offline, no durable history). The
    /// offline frontend keys its IndexedDB cache + history pulls under it so the
    /// direct and relay paths share one conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// Present only when `state == Parked`: why the agent parked, structured
    /// (bridge-written at the parking terminal event; cleared on any
    /// non-parked terminal). Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_reason: Option<ParkedReason>,
    /// Present only when `state == Retrying` (chain-transient backoff armed):
    /// the attempt counters and backoff delay, mirroring the FCE event's
    /// `transient_retry` payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrying: Option<TransientRetryInfo>,
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

/// The model profile an agent's client is currently using: the tier's positional
/// index, the registry profile id, the provider (endpoint) id, and the concrete model
/// string sent to the backend. This is the *runtime* active profile — it drifts from
/// the spawn-time active after a within-tier failover advance or an online profile
/// apply, which is exactly when an operator needs to see it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveProfile {
    /// Positional tier index in the registry (0-based; display layers add 1).
    pub tier_index: usize,
    pub profile_id: String,
    /// The endpoint (provider) id this profile connects through.
    pub provider: String,
    /// For env-configured single-profile agents this is the raw
    /// `KALLIP_LLM_MODEL` value, so an env-only setup still shows a
    /// meaningful model string.
    pub model: String,
}

/// Combined agent status: lifecycle state + context usage + recent retry history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusResponse {
    pub state: AgentState,
    pub context: ContextUsage,
    pub recent_retries: Vec<RetryRecord>,
    /// Tagma-wide token consumption budget (shared by all agents).
    pub token_budget: u64,
    /// Cumulative tagma-wide tokens consumed toward the budget.
    pub token_consumed: u64,
    /// Ephemeral, agent-self-reported current activity. Empty when idle.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
    /// Present only when `state == Parked` — see [`AgentSummary::parked_reason`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_reason: Option<ParkedReason>,
    /// Present only when `state == Retrying` — see [`AgentSummary::retrying`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrying: Option<TransientRetryInfo>,
    /// The model profile the agent's client is currently using; see
    /// [`ActiveProfile`]. Absent only on responses from tagma versions that
    /// predate the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ActiveProfile>,
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
    /// The tagma-global `bash_exec` classify rule-set in effect for this agent
    /// (read-only — it is set once at tagma startup from `KALLIP_POLICY_PRESET`).
    pub preset: PolicyPreset,
    /// FS-access permission class actually granted to this agent, as the
    /// lowercase wire spelling (`"normal"` / `"guest"`) — the value the tagma
    /// clamped at spawn and re-validates on restore. Surfaced here (it was
    /// previously invisible to clients) so an explicit downgrade is observable
    /// and verifiable. String-typed to keep `kallip-common` runtime-free.
    pub permission_class: String,
}

#[cfg(test)]
mod tests {
    use super::CreateAgentRequest;

    #[test]
    fn rejects_request_without_workspace_root() {
        let json = r#"{"skills":[],"created_by":null,"role":"reviewer"}"#;
        assert!(
            serde_json::from_str::<CreateAgentRequest>(json).is_err(),
            "missing workspace_root must be rejected"
        );
    }
}
