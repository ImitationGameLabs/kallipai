use just_agent_core::context::ContextUsage;
use just_agent_core::retry::RetryRecord;
use just_agent_core::types::AgentState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub(crate) struct PromptRequest {
    pub text: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateAgentRequest {
    pub workspace_root: Option<String>,
    pub skills: Vec<String>,
    pub prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateAgentResponse {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListAgentsResponse {
    pub agents: Vec<AgentSummary>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentSummary {
    pub id: String,
    pub workspace_root: String,
    pub state: AgentState,
}

#[derive(Debug, Serialize)]
pub(crate) struct ApprovalRequestBody {
    pub request_id: String,
    pub decision: String,
    pub reason: Option<String>,
}

/// Deferred action info extracted from an SSE `DeferredCreated` event.
#[derive(Debug, Clone)]
pub struct DeferredInfo {
    pub request_id: String,
    pub tool_name: String,
    pub summary: String,
    pub reason: String,
    pub dangerous: bool,
}

/// Combined agent status: context usage + retry history.
#[derive(Debug, Deserialize)]
pub struct AgentStatusResponse {
    pub state: AgentState,
    pub context: ContextUsage,
    pub recent_retries: Vec<RetryRecord>,
}
