//! Runtime-internal event and outcome types.
//!
//! These types carry information between the agent runner/session modules
//! and the daemon bridge. They are not serialized over the wire -- the
//! bridge converts them to the SSE wire-format events defined in
//! `just_agent_common::types::SseEvent`.

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
    ApprovalRedeemed {
        id: String,
    },
    ApprovalCancelled {
        id: String,
    },
    Cancelled,
}

/// Outcome of running the agent round loop.
pub enum AgentOutcome {
    Finished { content: String },
    MaxRoundsExceeded,
    Cancelled,
}
