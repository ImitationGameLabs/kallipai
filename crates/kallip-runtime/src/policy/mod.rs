mod agent;
pub mod classifier;
mod executor;

/// Authorization decision for a tool invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    /// Defer to approval. `reason` carries the actionable explanation of why
    /// (when known) so the agent can rewrite the command instead of requesting
    /// approval; `None` for non-classifier deferral paths.
    Ask {
        reason: Option<String>,
    },
    Deny {
        reason: String,
    },
}

pub use agent::AgentPolicy;
pub use executor::AuthorizedToolExecutor;
