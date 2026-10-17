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

use just_llm_client::types::generation::Message;

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
pub(crate) use compose::{message_has_images, required_modalities, strip_message_images};
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

/// The compaction ledger is part of the context's public shape: a spawn site
/// constructs the context and owns its initial state.
pub use compact::CompactionLedger;
pub(crate) use compact::{
    CompactOutcome, CompactionVerdict, compact_pre_loop, note_compaction_failure,
    summarize_and_evict,
};
pub(crate) use estimate::estimate_context_tokens;
pub(crate) use warnings::{check_progressive_warnings, check_token_budget_warnings};

/// Label of the pinned summary that compaction and eviction both maintain.
pub(crate) const CONTEXT_SUMMARY_LABEL: &str = "context_summary";

/// Header that fronts every pinned `context_summary`, whatever produced it
/// (compaction or an explicit eviction). The bracketed form keeps it from
/// reading as a request the model is meant to answer.
pub(crate) const CONTEXT_SUMMARY_HEADER: &str =
    "[context summary — earlier turns condensed, not a request]";

/// The body of a summary record: `text` without the canonical header.
///
/// Re-wrapping goes through here, so a text that already carries the header is
/// fronted exactly once instead of once per round.
pub(crate) fn summary_body(text: &str) -> &str {
    match text.strip_prefix(CONTEXT_SUMMARY_HEADER) {
        Some(rest) => rest.trim_start_matches('\n'),
        None => text,
    }
}

/// Builds the summary record for `text`: one shape for its three consumers —
/// the pinned layer, the history line, and the compactor's carried summary.
///
/// The summary is injected context, never a model turn.
pub(crate) fn summary_record(text: impl AsRef<str>) -> Message {
    Message::user(format!(
        "{CONTEXT_SUMMARY_HEADER}\n\n{}",
        summary_body(text.as_ref())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_record_is_a_user_record_behind_one_header() {
        let message = summary_record("earlier work");
        assert_eq!(
            message.role(),
            "user",
            "a summary is injected context, not a model turn"
        );
        let text = message.content().unwrap_or_default();
        assert!(text.starts_with(CONTEXT_SUMMARY_HEADER));
        assert!(text.ends_with("earlier work"));
    }

    #[test]
    fn summary_record_does_not_stack_headers() {
        let once = summary_record("earlier work");
        let twice = summary_record(once.content().unwrap_or_default());
        assert_eq!(
            once.content(),
            twice.content(),
            "re-wrapping an already headed summary must not repeat the header"
        );
    }
}
