mod agent;
mod classifier;
mod executor;

/// User response to a tool approval prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApprovalDecision {
    Allow,
    AlwaysAllow,
    Deny,
}

/// Authorization decision for a tool invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Ask { reason: String, dangerous: bool },
    Deny { reason: String },
}

pub use agent::AgentPolicy;
pub use executor::AuthorizedToolExecutor;
