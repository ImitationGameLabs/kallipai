//! Runtime-internal event and outcome types.
//!
//! These types carry information between the agent runner/agent_task modules
//! and the daemon bridge. They are not serialized over the wire -- the
//! bridge converts them to the SSE wire-format events defined in
//! `kallip_common::protocol::SseEvent`.

use kallip_common::protocol::FailoverChainExhaustion;

/// Events emitted by the agent runner during execution.
///
/// Sent over an internal mpsc channel from the runtime to the daemon bridge,
/// which converts them to SSE wire-format events.
#[derive(Debug)]
pub enum AgentEvent {
    Reasoning(String),
    AssistantContent(String),
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
    ToolResult(String),
    Finished(String),
    MaxRoundsExceeded,
    Error(String),
    Status(String),
    Busy,
    ApprovalCommitted {
        id: String,
        tool_name: String,
        arguments: serde_json::Value,
        commit_reason: String,
    },
    Retrying {
        attempt: u32,
        max_attempts: u32,
        error: String,
        delay_secs: f64,
    },
    /// The LLM stream dropped mid-way (transport error after content started flowing) and the
    /// runner is retrying from scratch. Unlike [`Retrying`](Self::Retrying) — which fires at the
    /// prepare/send boundary, before any content — this fires *after* deltas were already emitted,
    /// so downstream consumers must treat the partial assistant/reasoning content accumulated since
    /// the last boundary as abandoned (fold/discard it) before rendering the retried stream afresh.
    /// Fields mirror [`Retrying`](Self::Retrying) plus the carried `error`. Non-terminal; the agent
    /// stays busy.
    StreamReset {
        error: String,
        attempt: u32,
        max_attempts: u32,
        delay_secs: f64,
    },
    /// Within-tier failover: the active profile failed terminally and the runner advanced to the
    /// next profile in the tier's chain.
    Failover {
        from: String,
        to: String,
        reason: String,
    },
    ApprovalRedeemed {
        id: String,
    },
    ApprovalCancelled {
        id: String,
    },
    Cancelled,
    /// The current round was interrupted (`interrupt_agent`); the task stays alive
    /// and returns to the outer loop for the next prompt. Distinct from `Cancelled`,
    /// which is terminal (remove/shutdown).
    Interrupted,
    TokenBudgetExceeded {
        consumed: u64,
        budget: u64,
    },
    /// Within-tier failover chain exhausted (terminal for the turn). The runner reached a known
    /// end-of-chain state — distinct from [`Error`](Self::Error), which is an undifferentiated
    /// failure. Bridges to `SseEvent::FailoverChainExhausted`; emitted by `run_and_report`.
    FailoverChainExhausted {
        reason: FailoverChainExhaustion,
        detail: String,
    },
}

/// Outcome of running the agent round loop.
#[derive(Debug)]
pub enum AgentOutcome {
    Finished {
        content: String,
    },
    MaxRoundsExceeded,
    Cancelled,
    TokenBudgetExceeded {
        consumed: u64,
        budget: u64,
    },
    /// Within-tier failover chain exhausted — a defined non-success round-end (sibling of
    /// `MaxRoundsExceeded`), not an `Err`. The active profile failed terminally and no buildable
    /// backup remained; `reason` distinguishes the cause, `detail` is the original trigger's
    /// `{:#}` display. The agent stays alive and idle (the operator may reconfigure failover and
    /// re-prompt).
    FailoverChainExhausted {
        reason: FailoverChainExhaustion,
        detail: String,
    },
}
