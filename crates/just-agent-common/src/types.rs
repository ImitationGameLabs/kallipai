//! Shared types used across the agent crate.

use serde::{Deserialize, Serialize};

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

/// Unique identifier for an agent instance.
///
/// Thin wrapper around a UUID string — provides type safety without format validation.
/// Use [`AgentId::random()`] to generate a new one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(String);

impl AgentId {
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for AgentId {
    fn from(s: String) -> Self {
        AgentId(s)
    }
}

impl From<AgentId> for String {
    fn from(id: AgentId) -> Self {
        id.0
    }
}

impl std::str::FromStr for AgentId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(AgentId(s.to_owned()))
    }
}

impl std::borrow::Borrow<str> for AgentId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

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
    DeferredCommitted {
        id: String,
        tool_name: String,
        arguments: serde_json::Value,
        reason: String,
        dangerous: bool,
    },
    Retrying {
        attempt: u32,
        max_attempts: u32,
        error: String,
        delay_secs: f64,
    },
    DeferredRedeemed {
        id: String,
    },
    DeferredCancelled {
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
    DeferredActionUpdated {
        id: String,
        status: DeferredActionStatus,
    },
    Retrying {
        attempt: u32,
        max_attempts: u32,
        error: String,
        delay_secs: f64,
    },
    Cancelled,
}

impl SseEvent {
    /// Convert an AgentEvent to an SSE event for broadcast.
    /// Returns `None` for events handled by other means (e.g., routed to superiors).
    pub fn try_from_agent(event: AgentEvent) -> Option<Self> {
        match event {
            AgentEvent::DeferredCommitted { .. } => None,
            AgentEvent::DeferredRedeemed { id } => Some(Self::DeferredActionUpdated {
                id,
                status: DeferredActionStatus::Redeemed,
            }),
            AgentEvent::DeferredCancelled { id } => Some(Self::DeferredActionUpdated {
                id,
                status: DeferredActionStatus::Cancelled,
            }),
            AgentEvent::Reasoning(content) => Some(Self::Reasoning { content }),
            AgentEvent::AssistantContent(content) => Some(Self::AssistantContent { content }),
            AgentEvent::AssistantContentDelta { delta } => {
                Some(Self::AssistantContentDelta { delta })
            }
            AgentEvent::ReasoningDelta { delta } => Some(Self::ReasoningDelta { delta }),
            AgentEvent::ToolCall { name, args } => Some(Self::ToolCall { name, args }),
            AgentEvent::ToolResult(result) => Some(Self::ToolResult { result }),
            AgentEvent::Finished(content) => Some(Self::Finished { content }),
            AgentEvent::MaxRoundsExceeded => Some(Self::MaxRoundsExceeded),
            AgentEvent::Error(msg) => Some(Self::Error { message: msg }),
            AgentEvent::Status(msg) => Some(Self::Status { message: msg }),
            AgentEvent::Busy => Some(Self::Busy),
            AgentEvent::Retrying {
                attempt,
                max_attempts,
                error,
                delay_secs,
            } => Some(Self::Retrying {
                attempt,
                max_attempts,
                error,
                delay_secs,
            }),
            AgentEvent::Cancelled => Some(Self::Cancelled),
        }
    }
}

/// Request body for creating a new agent instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    pub workspace_root: Option<String>,
    pub skills: Vec<String>,
    pub prompt: Option<String>,
    pub created_by: Option<AgentId>,
}

/// Response body returned after creating an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentResponse {
    pub id: AgentId,
}

/// Status of a deferred tool action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredActionStatus {
    Pending,
    Committed,
    Approved,
    Denied,
    Redeemed,
    Cancelled,
}

impl DeferredActionStatus {
    /// Parse a status string (e.g. from a query parameter).
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "committed" => Some(Self::Committed),
            "approved" => Some(Self::Approved),
            "denied" => Some(Self::Denied),
            "redeemed" => Some(Self::Redeemed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Redeemed => "redeemed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for DeferredActionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Complete tool call content for a deferred action.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallContent {
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// A single deferred action entry in API responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredActionEntry {
    pub id: String,
    pub requested_by: AgentId,
    pub content: ToolCallContent,
    pub reason: String,
    pub dangerous: bool,
    pub status: DeferredActionStatus,
    pub deny_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Response for listing deferred actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDeferredActionsResponse {
    pub items: Vec<DeferredActionEntry>,
    pub total: usize,
}

/// Request body for approving or denying a deferred action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredActionDecisionBody {
    pub decision: String,
    pub reason: Option<String>,
}
