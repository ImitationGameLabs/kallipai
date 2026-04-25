//! Shared types used across the agent crate.

use serde::{Deserialize, Serialize};
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
    DeferredCreated {
        request_id: String,
        tool_name: String,
        summary: String,
        reason: String,
        dangerous: bool,
    },
}

/// Outcome of running the agent round loop.
pub enum AgentOutcome {
    Finished { content: String },
    MaxRoundsExceeded,
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
    DeferredCreated {
        request_id: String,
        tool_name: String,
        summary: String,
        reason: String,
        dangerous: bool,
    },
    DeferredApproved {
        request_id: String,
    },
    DeferredDenied {
        request_id: String,
        reason: String,
    },
}

impl From<AgentEvent> for SseEvent {
    fn from(event: AgentEvent) -> Self {
        match event {
            AgentEvent::Reasoning(content) => SseEvent::Reasoning { content },
            AgentEvent::AssistantContent(content) => SseEvent::AssistantContent { content },
            AgentEvent::AssistantContentDelta { delta } => {
                SseEvent::AssistantContentDelta { delta }
            }
            AgentEvent::ReasoningDelta { delta } => SseEvent::ReasoningDelta { delta },
            AgentEvent::ToolCall { name, args } => SseEvent::ToolCall { name, args },
            AgentEvent::ToolResult(result) => SseEvent::ToolResult { result },
            AgentEvent::Finished(content) => SseEvent::Finished { content },
            AgentEvent::MaxRoundsExceeded => SseEvent::MaxRoundsExceeded,
            AgentEvent::Error(msg) => SseEvent::Error { message: msg },
            AgentEvent::Status(msg) => SseEvent::Status { message: msg },
            AgentEvent::Busy => SseEvent::Busy,
            AgentEvent::DeferredCreated { request_id, tool_name, summary, reason, dangerous } => {
                SseEvent::DeferredCreated { request_id, tool_name, summary, reason, dangerous }
            }
        }
    }
}
