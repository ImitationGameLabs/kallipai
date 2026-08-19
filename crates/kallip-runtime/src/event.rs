//! Runtime-internal event and outcome types.
//!
//! These types carry information between the agent runner/agent_task modules
//! and the tagma bridge. They are not serialized over the wire -- the
//! bridge converts them to the SSE wire-format events defined in
//! `kallip_common::protocol::SseEvent`.

use kallip_common::protocol::{FailoverChainExhaustion, TransientRetryInfo};

/// Events emitted by the agent runner during execution.
///
/// Sent over an internal mpsc channel from the runtime to the tagma bridge,
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
    /// The agent yielded control by calling the `break` tool (or the harness
    /// force-idled it after the no-progress guardrail). Content-less: a message
    /// to the user is now a deliberate `kallip lesche send` CLI call, not the
    /// final assistant message, so this event carries no text — it is a pure
    /// status transition (the task parks, awaiting external input).
    Idle,
    /// The agent deliberately parked itself waiting (via `break(wait)`) with an armed
    /// timer. Terminal for the turn like [`Idle`](Self::Idle), but the task stays
    /// alive with unfinished business: the outer loop wakes it on the timer (an
    /// injected `[system]` turn -- the agent then decides) or on any external
    /// event; `break(idle)` finishes. Mirrors
    /// [`SseEvent::Waiting`](kallip_common::protocol::SseEvent::Waiting).
    Waiting {
        timeout_secs: u64,
    },
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
        transient_retry: Option<TransientRetryInfo>,
    },
}

impl AgentEvent {
    /// Whether this event terminates the current turn: the agent goes idle
    /// when the bridge sees it. Turn-level, not lifecycle-level — see the
    /// wire-side mirror [`SseEvent::is_terminal`](kallip_common::protocol::SseEvent::is_terminal)
    ///
    /// **Mirror obligation:** this classification MUST stay in sync with
    /// `SseEvent::is_terminal` (kallip-common) — the bridge gates its idle mark
    /// here while downstream consumers gate on the wire side, and a divergence
    /// between the two silently desynchronizes idle marking from what clients
    /// see. Enforced mechanically: the per-side exhaustive snapshot (this file,
    /// and the wire side in `protocol::sse`) pins each classification, and the
    /// tagma bridge's `convert_event_preserves_terminal_parity` test asserts the
    /// two sides agree for every variant.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Idle
                | Self::Waiting { .. }
                | Self::MaxRoundsExceeded
                | Self::Error(_)
                | Self::Cancelled
                | Self::Interrupted
                | Self::TokenBudgetExceeded { .. }
                | Self::FailoverChainExhausted { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive classification snapshot, mirroring the wire-side test in
    /// `kallip_common::protocol::sse`. The `let _: () = match` arm set must cover
    /// every variant, so adding a variant without classifying it here is a
    /// compile error — the mirror cannot silently drift.
    #[test]
    fn terminal_classification_is_exhaustive_and_matches_predicate() {
        for v in SAMPLES {
            let terminal = v.is_terminal();
            let _: () = match v {
                AgentEvent::Reasoning(_)
                | AgentEvent::AssistantContent(_)
                | AgentEvent::AssistantContentDelta { .. }
                | AgentEvent::ReasoningDelta { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::ToolResult(_)
                | AgentEvent::Status(_)
                | AgentEvent::Busy
                | AgentEvent::ApprovalCommitted { .. }
                | AgentEvent::ApprovalRedeemed { .. }
                | AgentEvent::ApprovalCancelled { .. }
                | AgentEvent::Retrying { .. }
                | AgentEvent::StreamReset { .. }
                | AgentEvent::Failover { .. } => {
                    assert!(!terminal, "{v:?}");
                }
                AgentEvent::Idle
                | AgentEvent::Waiting { .. }
                | AgentEvent::MaxRoundsExceeded
                | AgentEvent::Error(_)
                | AgentEvent::Cancelled
                | AgentEvent::Interrupted
                | AgentEvent::TokenBudgetExceeded { .. }
                | AgentEvent::FailoverChainExhausted { .. } => {
                    assert!(terminal, "{v:?}");
                }
            };
        }
    }

    /// One instance of every variant, for the exhaustive snapshot above.
    const SAMPLES: &[AgentEvent] = &[
        AgentEvent::Reasoning(String::new()),
        AgentEvent::AssistantContent(String::new()),
        AgentEvent::AssistantContentDelta {
            delta: String::new(),
        },
        AgentEvent::ReasoningDelta {
            delta: String::new(),
        },
        AgentEvent::ToolCall {
            name: String::new(),
            args: String::new(),
        },
        AgentEvent::ToolResult(String::new()),
        AgentEvent::Idle,
        AgentEvent::Waiting { timeout_secs: 0 },
        AgentEvent::MaxRoundsExceeded,
        AgentEvent::Error(String::new()),
        AgentEvent::Status(String::new()),
        AgentEvent::Busy,
        AgentEvent::ApprovalCommitted {
            id: String::new(),
            tool_name: String::new(),
            arguments: serde_json::Value::Null,
            commit_reason: String::new(),
        },
        AgentEvent::Retrying {
            attempt: 0,
            max_attempts: 0,
            error: String::new(),
            delay_secs: 0.0,
        },
        AgentEvent::StreamReset {
            error: String::new(),
            attempt: 0,
            max_attempts: 0,
            delay_secs: 0.0,
        },
        AgentEvent::Failover {
            from: String::new(),
            to: String::new(),
            reason: String::new(),
        },
        AgentEvent::ApprovalRedeemed { id: String::new() },
        AgentEvent::ApprovalCancelled { id: String::new() },
        AgentEvent::Cancelled,
        AgentEvent::Interrupted,
        AgentEvent::TokenBudgetExceeded {
            consumed: 0,
            budget: 0,
        },
        AgentEvent::FailoverChainExhausted {
            reason: FailoverChainExhaustion::NoFailoverConfigured,
            detail: String::new(),
            transient_retry: None,
        },
    ];

    /// Vocabulary snapshot: the Debug shape of every variant — variant name
    /// plus, for struct variants, field names. Tuple-variant payloads
    /// (Reasoning/AssistantContent/ToolResult/Error/Status carry one String)
    /// show only the value; their field meaning is positional by definition.
    /// AgentEvent is runtime-internal (never serialized), so this Debug pin
    /// is the layer's shape contract: adding/renaming/removing a variant or
    /// field reds here. Values are not pinned — fixtures reuse [`SAMPLES`]
    /// empty/zero values; semantics live in the classification snapshot.
    #[test]
    fn schema_snapshot() {
        assert_eq!(SAMPLES.len(), 22, "variant count drifted — extend SAMPLES");
        let expected: &[&str] = &[
            "Reasoning(\"\")",
            "AssistantContent(\"\")",
            "AssistantContentDelta { delta: \"\" }",
            "ReasoningDelta { delta: \"\" }",
            "ToolCall { name: \"\", args: \"\" }",
            "ToolResult(\"\")",
            "Idle",
            "Waiting { timeout_secs: 0 }",
            "MaxRoundsExceeded",
            "Error(\"\")",
            "Status(\"\")",
            "Busy",
            "ApprovalCommitted { id: \"\", tool_name: \"\", arguments: Null, commit_reason: \"\" }",
            "Retrying { attempt: 0, max_attempts: 0, error: \"\", delay_secs: 0.0 }",
            "StreamReset { error: \"\", attempt: 0, max_attempts: 0, delay_secs: 0.0 }",
            "Failover { from: \"\", to: \"\", reason: \"\" }",
            "ApprovalRedeemed { id: \"\" }",
            "ApprovalCancelled { id: \"\" }",
            "Cancelled",
            "Interrupted",
            "TokenBudgetExceeded { consumed: 0, budget: 0 }",
            "FailoverChainExhausted { reason: NoFailoverConfigured, detail: \"\", transient_retry: None }",
        ];
        for (v, want) in SAMPLES.iter().zip(expected) {
            assert_eq!(format!("{v:?}"), *want);
        }
    }
}

/// Outcome of running the agent round loop.
#[derive(Debug)]
pub enum AgentOutcome {
    /// The agent deliberately yielded (via `break`) or was force-idled by the
    /// no-progress guardrail. Content-less — see [`AgentEvent::Idle`].
    Idle,
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
