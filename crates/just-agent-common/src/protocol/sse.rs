//! SSE wire-format events for daemon-to-client streaming.

use serde::{Deserialize, Serialize};

use crate::approval::ApprovalStatus;

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
    Cancelled,
    /// The current round was interrupted; the agent stays alive and idle, ready for the
    /// next prompt. Distinct from `Cancelled`, which is terminal (delete/shutdown).
    Interrupted,
    TokenBudgetExceeded {
        consumed: u64,
        budget: u64,
    },
}
