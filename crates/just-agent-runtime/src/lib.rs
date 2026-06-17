pub mod agent_task;
pub mod approval;
pub mod config;
pub mod context;
mod env_util;
pub mod event;
mod failover;
pub mod history;
pub mod persistence;
pub mod policy;
pub mod profile;
pub mod retry;
pub mod runner;
mod stream_accumulator;
pub mod token_budget;
pub mod tools;

// Re-exported so the daemon (another crate) can construct `AgentContext.failover`. The state's
// accessors stay `pub(crate)` — only the runtime reads them.
pub use failover::FailoverState;
