//! Agentic context management module.
//!
//! - [`ContextStore`] — single source of truth for all context data
//! - [`compose_context`] — assembles layers into `Vec<ChatMessage>`
//! - [`CompactionStrategy`] — pluggable strategies for context reduction
//! - [`AgenticContext`] — trait for the agent's context management interface
//!
//! Layers are filled in priority order: pinned → summary → working.
//! Budget checking uses accurate token estimation via ChatClient.

mod compact;
mod compose;
mod store;
mod turn;

pub use compact::{CompactionStrategy, SummarizeStrategy};
pub use compose::compose_context;
pub use store::{AgenticContext, ContextStore, ContextUsage};
