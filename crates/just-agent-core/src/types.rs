//! Shared types used across the agent crate.

/// Agent lifecycle state exposed via the status endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Busy,
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
        })
    }
}
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
    Retrying {
        attempt: u32,
        max_attempts: u32,
        error: String,
        delay_secs: f64,
    },
    Cancelled,
}

/// Outcome of running the agent round loop.
pub enum AgentOutcome {
    Finished { content: String },
    MaxRoundsExceeded,
    Cancelled,
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
    Retrying {
        attempt: u32,
        max_attempts: u32,
        error: String,
        delay_secs: f64,
    },
    Cancelled,
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
            AgentEvent::Retrying { attempt, max_attempts, error, delay_secs } => {
                SseEvent::Retrying { attempt, max_attempts, error, delay_secs }
            }
            AgentEvent::Cancelled => SseEvent::Cancelled,
        }
    }
}

/// Request body for creating a new agent instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    pub workspace_root: Option<String>,
    pub skills: Vec<String>,
    pub prompt: Option<String>,
    pub created_by: Option<String>,
}

/// Response body returned after creating an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentResponse {
    pub id: String,
}
