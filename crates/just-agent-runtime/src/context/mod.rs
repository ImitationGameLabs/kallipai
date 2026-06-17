//! Agentic context management module.
//!
//! - [`ContextStore`] — single source of truth for all context data
//! - [`compose_context`] — assembles layers into `Vec<ChatMessage>`
//! - [`ContextSummarizer`] — LLM-powered summarization of old turns
//! - `estimate_context_tokens` / `check_progressive_warnings` / `check_token_budget_warnings` /
//!   `summarize_and_evict` — the round loop's crate-private context-budget layer: estimation,
//!   warning injection, and bounded compaction, all reading `ContextStore`'s anchor API.
//!
//! Layers are filled in priority order: pinned → turns.

mod compact;
mod compose;
mod estimate;
mod store;
mod summarize;
mod turn;
mod warnings;

pub use compose::compose_context;
pub use store::{AgenticContext, ContextStore};
pub use summarize::{ContextSummarizer, Summary};
pub use turn::Turn;
pub use turn::TurnId;

pub(crate) use compact::{CompactOutcome, compact_if_needed, summarize_and_evict};
pub(crate) use estimate::estimate_context_tokens;
pub(crate) use warnings::{check_progressive_warnings, check_token_budget_warnings};
