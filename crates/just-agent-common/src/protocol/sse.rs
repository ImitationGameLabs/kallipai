//! SSE wire-format events for daemon-to-client streaming.

use serde::{Deserialize, Serialize};

use crate::approval::ApprovalStatus;

/// Distinguishable cause for within-tier failover chain exhaustion.
///
/// Carried by [`SseEvent::FailoverChainExhausted`] (and, on the runtime side, by the matching
/// `AgentOutcome` / `FailoverOutcome` variants) so operators can tell apart the structurally
/// distinct exhaustion modes instead of seeing a generic error.
///
/// Defined here (not in the runtime) because it is part of the serialized event contract, so the
/// wire crate must own the taxonomy — the same shape as [`ApprovalStatus`], which is shared
/// between the runtime and the daemon API. Fieldless and `Copy` (common has no `anyhow`); the
/// trigger text rides in a separate `detail: String` on the carrying event. Typed here (rather
/// than a free-text `reason: String` like [`SseEvent::Failover`]) because the exhaustion *states*
/// are enumerable and clients branch on them, whereas a failover hop's cause is an opaque
/// per-backend error.
///
/// **Terminal-reason coalescing:** a chain that skips candidates for *mixed* reasons (some
/// unbuildable, some window-infeasible) surfaces [`AllCandidatesInfeasible`](Self::AllCandidatesInfeasible)
/// — the per-candidate `warn!`s in `advance_failover` carry each skip's precise cause. A single
/// infeasible candidate wins the reason even when the majority were unbuildable: the
/// window-infeasibility mode is the subtler, more actionable one to surface (the operator can
/// retune the budget shape without redeploying credentials). Intentional — do not "fix" it to
/// last-reason or counts without revisiting the operator-UX rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailoverChainExhaustion {
    /// The tier has a single profile — failover was never configured.
    NoFailoverConfigured,
    /// Multi-profile tier, but the active profile was already the last (the chain was advanced
    /// through and now its tail has failed terminally).
    AllBackupsExhausted,
    /// Remaining candidate profiles existed but every one's backend refused to build
    /// (configuration / credential failure, distinct from the runtime trigger).
    AllCandidatesUnbuildable,
    /// Remaining candidate profiles existed and every one's declared `max_context_window` violated
    /// a budget invariant (e.g. `summary_max_tokens` exceeds the pinned budget at that window) —
    /// tune `summary_max_tokens` / `pinned_budget_ratio` or raise the window. Distinct from
    /// [`AllCandidatesUnbuildable`](Self::AllCandidatesUnbuildable): these candidates build fine,
    /// their window just can't serve the configured budget shape.
    AllCandidatesInfeasible,
}

impl std::fmt::Display for FailoverChainExhaustion {
    /// Operator-readable lowercase prose, shared by the TUI, stdio, and `just-agent-run` so all
    /// three surfaces render the same cause identically.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::NoFailoverConfigured => "no failover configured",
            Self::AllBackupsExhausted => "all backups exhausted",
            Self::AllCandidatesUnbuildable => "all failover candidates unbuildable",
            Self::AllCandidatesInfeasible => "all failover candidates had infeasible windows",
        };
        f.write_str(s)
    }
}

/// Wire-format event for SSE transport (daemon to client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SseEvent {
    Reasoning {
        content: String,
    },
    AssistantContent {
        content: String,
    },
    AssistantContentDelta {
        delta: String,
    },
    ReasoningDelta {
        delta: String,
    },
    ToolCall {
        name: String,
        args: String,
    },
    ToolResult {
        result: String,
    },
    Finished {
        content: String,
    },
    MaxRoundsExceeded,
    Error {
        message: String,
    },
    Status {
        message: String,
    },
    Busy,
    ApprovalUpdated {
        id: String,
        status: ApprovalStatus,
    },
    Retrying {
        attempt: u32,
        max_attempts: u32,
        error: String,
        delay_secs: f64,
    },
    /// Within-tier failover: the active profile failed terminally and the runner advanced to the
    /// next profile in the tier's chain. Non-terminal — the agent stays busy and continues the
    /// turn on the new profile. `from`/`to` are profile ids.
    Failover {
        from: String,
        to: String,
        reason: String,
    },
    Cancelled,
    /// The current round was interrupted; the agent stays alive and idle, ready for the
    /// next prompt. Distinct from `Cancelled`, which is terminal (remove/shutdown).
    Interrupted,
    TokenBudgetExceeded {
        consumed: u64,
        budget: u64,
    },
    /// Within-tier failover chain exhausted: the active profile failed terminally and no
    /// buildable backup remained. Terminal for the turn (the agent goes idle) but **not**
    /// lifecycle-terminal — the agent stays alive and can be re-prompted (e.g. after the
    /// operator reconfigures failover). `reason` distinguishes the cause; `detail` is the
    /// original trigger's `{:#}` display. Distinct from a generic [`Failover`](Self::Failover)
    /// (a non-terminal hop) and from [`Error`](Self::Error) (an undifferentiated turn error).
    FailoverChainExhausted {
        reason: FailoverChainExhaustion,
        detail: String,
    },
}
