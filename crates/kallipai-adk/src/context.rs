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

use just_llm_client::types::generation::{Message, Reasoning, ToolCall};

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

/// Builds the assistant message for one model response: the text it produced,
/// the tool calls it asked for, and its own reasoning when it produced any.
///
/// Empty reasoning stays absent instead of becoming a stand-in: invented
/// reasoning would put words in the model's mouth. A thinking-mode
/// backend refuses such a message only when another assistant message
/// follows it or when it ends the request; the agent path always builds
/// model responses here rather than through `Message::assistant`, so the
/// convention lives in one place.
///
/// A `content` of `None` covers one round shape: it dispatched tool calls
/// and carried no text.
///
/// Upstream offers no text-plus-reasoning constructor, so this goes through the
/// tool-call face, which serialises plain assistant text when the list is empty.
pub(crate) fn assistant_message(
    content: Option<String>,
    tool_calls: Vec<ToolCall>,
    reasoning: &str,
) -> Message {
    let reasoning = if reasoning.is_empty() {
        None
    } else {
        Some(Reasoning {
            text: Some(reasoning.to_owned()),
            ..Reasoning::default()
        })
    };
    Message::assistant_tool_calls(content, tool_calls, reasoning)
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

    #[test]
    fn assistant_message_keeps_the_models_own_thinking() {
        let message = assistant_message(Some("answer".into()), Vec::new(), "weighing the options");
        assert_eq!(message.role(), "assistant");
        assert_eq!(message.content(), Some("answer"));
        assert_eq!(
            message.reasoning().and_then(|r| r.text.as_deref()),
            Some("weighing the options")
        );
    }

    #[test]
    fn assistant_message_leaves_empty_reasoning_absent() {
        let message = assistant_message(Some("answer".into()), Vec::new(), "");
        assert_eq!(message.role(), "assistant");
        assert!(
            message.reasoning().is_none(),
            "an empty stream must not become a stand-in for thinking"
        );
    }
}
