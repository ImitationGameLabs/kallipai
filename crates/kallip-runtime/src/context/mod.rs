//! Agentic context management module.
//!
//! - [`ContextStore`] — single source of truth for all context data
//! - [`compose_context`] — assembles turns into `Vec<Message>` (pinned entries first)
//! - [`ContextSummarizer`] — LLM-powered summarization of old turns
//! - `estimate_context_tokens` / `check_progressive_warnings` / `check_token_budget_warnings` /
//!   `summarize_and_evict` — the crate-private context-budget layer behind the round loop and
//!   its budget gates: estimation, warning injection, and bounded compaction, all reading
//!   `ContextStore`'s anchor API.
//!
//! Pinned persistent context (summaries, skills, notes) is stored as `TurnKind::Pinned` turns
//! ahead of conversation turns, so one collection composes in priority order.

mod compact;
mod compose;
mod estimate;
pub(crate) mod manifest;
mod store;
mod summarize;
mod tokens;
mod turn;
mod warnings;

pub use compose::{
    FetchedImage, IngestImage, ReassemblyReport, compose_context, ingest_message,
    reassemble_attachments, reassemble_pin_attachments,
};
pub(crate) use compose::{message_has_images, strip_message_images};
pub use store::{AgenticContext, ContextStore};
pub use summarize::{ContextSummarizer, Summary};
pub(crate) use tokens::estimate_text;
pub use turn::Turn;
pub use turn::TurnId;
pub use turn::TurnKind;

/// The text that stands in for an image part wherever only words flow:
/// summarizer inputs and compaction slices consume text, and the
/// placeholder keeps the fact of a dropped image visible instead of
/// letting it vanish without a trace (the image itself lives in the
/// history sidecar, not in these views).
pub(crate) const IMAGE_PLACEHOLDER: &str = "[image omitted]";

pub(crate) use compact::{CompactOutcome, compact_if_needed, summarize_and_evict};
pub(crate) use estimate::estimate_context_tokens;
pub(crate) use warnings::{check_progressive_warnings, check_token_budget_warnings};
