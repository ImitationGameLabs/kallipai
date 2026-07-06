mod agent;
pub mod classifier;
mod executor;

/// Authorization decision for a tool invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Ask,
    Deny { reason: String },
}

pub use agent::AgentPolicy;
pub use executor::AuthorizedToolExecutor;
