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
    /// Operator-readable prose, shared by the park-kick turn text and client rendering.
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
    /// Registry-unique: it doubles as the readable addressing alias
    /// (`AgentId` stays the canonical, stable identity). Empty means unset.
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
    /// Profile set the subagent resolves against, by exact name. Required:
    /// the spawn rejects unknown names (the error lists the available
    /// ones), so a binding never lands on the record by accident.
    pub profile_set: String,
    /// FS-access permission class for the subagent spawn, as the lowercase
    /// wire spelling (`"normal"` / `"guest"`). Required — there is no
    /// implicit default: the tagma (the reference monitor) accepts the
    /// request only as a downgrade, rejecting it with `forbidden` if it
    /// exceeds the supervisor's own granted class. The string carries no
    /// runtime type here to keep `kallip-common` free of any
    /// `kallip-runtime` dependency.
    pub permission_class: String,
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

/// Workspace write-lock visibility for a live agent, joined from the lock
/// manager when a summary is built (`held` = the agent owns the write-lock
/// on its workspace root; `missing` = it does not). Only live Normal-class
/// agents carry a `lock` value: a Guest never acquires a workspace lock and
/// a faulted agent holds nothing by definition, so `None` there is the norm,
/// not a signal. `Some(Missing)` under that contract is the red flag the
/// fleet view exists to surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockState {
    Held,
    Missing,
}

/// Summary of an agent instance returned in list responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: AgentId,
    pub workspace_root: String,
    pub state: AgentState,
    pub created_by: Option<AgentId>,
    /// Short display label ("researcher"), registry-unique — it doubles as
    /// the readable addressing alias in CLI commands. Empty when unset.
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
    /// Workspace lock visibility, present only for live Normal-class agents
    /// — see [`LockState`]. Joined from the lock manager at summary build
    /// time, so a summary is a point-in-time probe, not a guarantee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<LockState>,
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
    /// The agent's recorded profile-set binding (exact set name), present once
    /// bound. The root may carry none before its first apply — restore-time
    /// default derivation fills it. Dangling names (the set was deleted) are
    /// surfaced by delivery rejections, not scrubbed here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_set: Option<String>,
    /// Unix seconds of the most recent state transition (creation sets the
    /// baseline). Absent for entries that predate the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<u64>,
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

/// Request body for `PUT /agents/{id}/profile-set` — rebind the agent to a named
/// profile set. The name is exact: unknown names are rejected with the list of
/// available sets. Requires the operator or a superior of the target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSetUpdateRequest {
    pub profile_set: String,
}

/// Request body for `PUT /profiles/default` — transfer the default-set marker
/// to an existing set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetDefaultRequest {
    pub default: String,
}

/// One agent still bound to a set — the reference list a set deletion must
/// clear (by interrupt) before the set can be removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetReference {
    pub id: AgentId,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
}

/// Response body for `DELETE /profiles/sets/{name}` — the removed name plus the
/// agents that were interrupted to release it (empty when unreferenced).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteSetResponse {
    pub removed: String,
    pub interrupted: Vec<SetReference>,
}

/// Request body for `PUT /agents/{id}/activity` — the agent reports its current
/// activity as free text. Only the agent itself (or operator) may call this.
/// An empty string clears the activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateActivityRequest {
    #[serde(default)]
    pub activity: String,
}

/// The model profile an agent's client is currently using: the recorded set
/// name, the registry profile id, the provider (endpoint) id, and the concrete model
/// string sent to the backend. This is the *runtime* active profile — it drifts from
/// the spawn-time active after a within-set failover advance or an online profile
/// apply, which is exactly when an operator needs to see it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveProfile {
    /// The recorded profile-set name this agent resolves against.
    pub profile_set: String,
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
    /// Whether the tagma-wide budget is unlimited (no enforcement;
    /// consumption still tracked). Absent (false) from older servers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub token_budget_unlimited: bool,
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

/// The input modality a model profile accepts or a file attachment carries.
/// `image` is the only non-text modality implemented so far; `audio` and
/// `video` are reserved for providers that accept them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
}

impl Modality {
    /// Canonical display order — human faces render in this order, not
    /// sorted order.
    pub const ALL: [Modality; 4] = [
        Modality::Text,
        Modality::Image,
        Modality::Audio,
        Modality::Video,
    ];

    /// The wire/JSON spelling (also used in diagnostics).
    pub fn as_str(self) -> &'static str {
        match self {
            Modality::Text => "text",
            Modality::Image => "image",
            Modality::Audio => "audio",
            Modality::Video => "video",
        }
    }
}

/// A file attached to a message: where it lives in the files service (the
/// record the sender uploaded/delivered) plus the display facts a file card
/// needs without a round trip. Wire shape `{record_id, name, size}`; the
/// single attachment type shared by every surface (room, relay, direct) --
/// platform and app crates both resolve it from this base crate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileAttachment {
    pub record_id: uuid::Uuid,
    pub name: String,
    pub size: u64,
    /// Input modality the attachment carries, when declared. Absent from the
    /// historical wire shape; serde default + skip keep bodies without one
    /// byte-identical (older servers/clients ignore the unknown field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modality: Option<Modality>,
}

/// Request body for sending a message to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRequest {
    pub text: String,
    /// A file attached to the message, when the sender shared one. Optional
    /// with `serde(default)` + `skip_serializing_if`: bodies without an
    /// attachment keep the historical wire shape and older servers are none
    /// the wiser (they ignore the unknown field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<FileAttachment>,
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
/// Request body for ingesting an attachment into an agent's live context:
/// the tagma fetches the media bytes from the files service, assembles the
/// multimodal message, and records the turn (sidecar refs plus the live
/// store). The bound set's effective modalities gate the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentIngestRequest {
    /// The files-service record id of the media to ingest.
    pub record_id: uuid::Uuid,
    /// The modality to ingest as. The endpoint is generic; each CLI
    /// subcommand pins one.
    pub modality: Modality,
    /// Media type for the assembled image part (e.g. `image/png`).
    /// Absent defaults to `image/png` server-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Optional human-readable caption carried alongside the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

/// Response body for an attachment ingest: the recorded turn id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentIngestResponse {
    pub turn_id: u64,
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
    /// clamped at spawn and re-validates on restore. Surfaced here so an
    /// explicit downgrade is observable and verifiable. String-typed to keep
    /// kallip-common runtime-free.
    pub permission_class: String,
}

#[cfg(test)]
mod tests {
    use super::{AttachmentIngestRequest, CreateAgentRequest, Modality};

    #[test]
    fn modality_as_str_matches_serde_spelling() {
        for m in super::Modality::ALL {
            assert_eq!(
                serde_json::to_string(&m).unwrap(),
                format!("\"{}\"", m.as_str())
            );
        }
    }

    #[test]
    fn attachment_ingest_request_round_trips_with_and_without_optionals() {
        let full = AttachmentIngestRequest {
            record_id: uuid::Uuid::from_bytes([1; 16]),
            modality: Modality::Image,
            media_type: Some("image/png".to_owned()),
            caption: Some("a chart".to_owned()),
        };
        let line = serde_json::to_string(&full).unwrap();
        assert!(line.contains("media_type"));
        assert!(line.contains("caption"));
        let back: AttachmentIngestRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(back.record_id, full.record_id);

        // Absent optionals deserialize to None (older callers keep the
        // historical-minimum body).
        let minimal = format!(
            "{{\"record_id\":\"{}\",\"modality\":\"image\"}}",
            uuid::Uuid::from_bytes([1; 16])
        );
        let back: AttachmentIngestRequest = serde_json::from_str(&minimal).unwrap();
        assert_eq!(back.media_type, None);
        assert_eq!(back.caption, None);
    }

    #[test]
    fn rejects_request_without_workspace_root() {
        let json = r#"{"skills":[],"created_by":null,"role":"reviewer"}"#;
        assert!(
            serde_json::from_str::<CreateAgentRequest>(json).is_err(),
            "missing workspace_root must be rejected"
        );
    }

    #[test]
    fn agent_summary_state_since_defaults_absent() {
        // A pre-field payload must deserialize with the timestamp absent,
        // and a round-trip must preserve it once set.
        let legacy = r#"{"id":"11111111-1111-1111-1111-111111111111",
            "workspace_root":"/tmp","state":"idle","created_by":null}"#;
        let parsed: super::AgentSummary = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.state_since, None);
        let stamped = super::AgentSummary {
            state_since: Some(1_700_000_000),
            ..parsed
        };
        let json = serde_json::to_string(&stamped).unwrap();
        assert!(json.contains("\"state_since\":1700000000"));
        let back: super::AgentSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state_since, Some(1_700_000_000));
    }
}
