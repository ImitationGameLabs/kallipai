//! Context compaction: summarize old turns into a pinned summary to stay within budget.
//!
//! [`summarize_and_evict`] runs bounded summarize-and-evict passes; [`compact_if_needed`] is the
//! pre-loop wrapper for restored agents. [`CompactOutcome`] reports the result.

use anyhow::Result;
use just_llm_client::types::generation::{ContentPart, Message};
use tracing::{info, warn};

use super::turn::{Turn, TurnKind};
use crate::agent_task::AgentContext;
use crate::context::AgenticContext;
use crate::history::{RecordKind, SystemEvent};
use crate::text_slice::head_tail_slice;

/// Outcome of context compaction via [`summarize_and_evict`].
pub(crate) enum CompactOutcome {
    /// Some turns were summarized and evicted.
    Compacted,
    /// No turns to compact (context already within budget).
    NothingToCompact,
    /// Token budget exceeded during summarization.
    BudgetExceeded { consumed: u64, budget: u64 },
}

/// Character cap for the head+tail slice taken from an oversized queue-head
/// turn: `min(100_000, 90% of the summarizer input budget)`. 100K chars
/// estimates to at most ~100K tokens even at CJK 1:1 density, and the 90%
/// headroom pairs with the startup validation in `config::check_context_budget`
/// (a truncated cap within 2× of the budget is rejected there), so the slice
/// fits unless an existing summary already consumes nearly the whole budget —
/// that corner declines with a warning instead of looping. A fixed cap would
/// re-create the wedge one level down on small windows (the summarizer would
/// skip the slice and the pass would make no progress). At the default 500K
/// window this resolves to the full 100K chars.
const WEDGE_SLICE_MAX_CHARS: usize = 100_000;

/// Build a summarizable slice turn from an oversized queue-head turn.
///
/// Keeps the head (60%) and tail (40%) of the turn's message text up to the
/// character cap derived from `input_budget`, marking the omitted middle.
/// Heads of tool output carry the command echo and first errors; tails carry
/// the final state — the middle of a megabyte of repeated errors rarely adds
/// signal. The returned turn is a temporary summarizer input only: it is
/// never pushed to the store, and the write phase evicts the original turn.
fn slice_oversized_turn(turn: &Turn, input_budget: usize) -> Turn {
    let cap_chars = WEDGE_SLICE_MAX_CHARS.min(input_budget * 9 / 10);
    let mut text = String::new();
    for message in &turn.messages {
        let content = message_text(message);
        text.push_str(&content);
        text.push('\n');
    }
    let (_, _, sliced) = head_tail_slice(&text, cap_chars);
    let messages = vec![Message::user(format!(
        "[Turn {} exceeded the summarizer budget; head+tail slice]\n{sliced}",
        turn.id.0
    ))];
    Turn {
        id: turn.id,
        estimated_tokens: Turn::estimate_tokens(&messages),
        messages,
        kind: TurnKind::Conversation,
    }
}

/// The message's text: plain text content, or the concatenated text parts
/// of a multimodal message (joined with newlines). Image parts contribute
/// nothing — the slice and the summary consume words, not pixels; a
/// dropped image remains in the turn's history sidecar record.
fn message_text(message: &Message) -> String {
    if let Some(content) = message.content() {
        return content.to_owned();
    }
    let Some(parts) = message.content_parts() else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
/// Summarize turns to bring context within budget.
///
/// Loops in bounded passes: each pass summarizes the oldest turns that fit
/// in the summarizer input budget, accumulates into the existing summary,
/// and evicts the summarized turns. Repeats until context fits or no
/// progress can be made.
pub(crate) async fn summarize_and_evict(ctx: &AgentContext) -> Result<CompactOutcome> {
    let effective_budget = ctx.config.effective_budget();
    let summarizer_input_budget =
        effective_budget.saturating_sub(ctx.summarizer.max_tokens as usize);
    let mut any_summarized = false;

    loop {
        // Read phase: snapshot under single lock.
        let (window, existing_summary) = {
            let guard = ctx.store.lock().await;
            if guard.turn_count() == 0 {
                break;
            }
            if guard.total_estimated_tokens() <= effective_budget {
                break;
            }
            let existing_summary = guard
                .pinned_turns()
                .find(|t| t.label() == Some("context_summary"))
                .and_then(|t| t.messages.first())
                .map(message_text);

            // Take oldest CONVERSATION turns (skip pinned) that fit in summarizer_input_budget.
            // Pinned turns are never summarized — excluding them here also prevents an infinite
            // loop: a pinned turn that survives eviction must not re-enter the window each pass.
            let mut budget = summarizer_input_budget;
            let mut window = Vec::new();
            for turn in guard.turns().iter().filter(|t| !t.is_pinned()) {
                if turn.estimated_tokens > budget {
                    break;
                }
                budget -= turn.estimated_tokens;
                window.push(turn.clone());
            }
            if window.is_empty() {
                // The oldest non-pinned turn alone exceeds the summarizer input
                // budget: the queue-head wedge that stalled compaction silently
                // (2026-09-06 incident: a ~503K-token tool result blocked 60
                // passes over 2.5h). Summarize a head+tail slice of it instead —
                // the slice fits the budget by construction, and the write
                // phase's evict clears the wedge so later passes proceed.
                match guard.turns().iter().find(|t| !t.is_pinned()) {
                    Some(wedge) => {
                        warn!(
                            turn_id = wedge.id.0,
                            estimated_tokens = wedge.estimated_tokens,
                            "oversized queue-head turn exceeds the summarizer input budget; summarizing a head+tail slice"
                        );
                        (
                            vec![slice_oversized_turn(wedge, summarizer_input_budget)],
                            existing_summary,
                        )
                    }
                    None => {
                        warn!("compaction window empty and no evictable turn; stopping compaction");
                        break;
                    }
                }
            } else {
                (window, existing_summary)
            }
        };

        // LLM call — lock released during this potentially long await.
        let (result, usage) = ctx
            .summarizer
            .summarize(
                &window,
                existing_summary.as_deref(),
                effective_budget,
                &ctx.client,
            )
            .await?;

        // If the summarizer couldn't fit even one turn alongside the existing summary, stop —
        // evicting 0 turns would leave the same window and loop forever.
        if result.source_turns == 0 {
            warn!("summarizer made no progress; stopping compaction");
            break;
        }

        // Write phase: replace summary + evict turns — single lock, no await.
        {
            let mut guard = ctx.store.lock().await;
            guard.replace_pin("context_summary", Message::assistant(&result.text))?;
            guard.evict_turns(result.source_turns);
            guard.reset_context_warnings();
            info!(
                source_turns = result.source_turns,
                estimated_tokens = result.estimated_tokens,
                "summarize pass completed"
            );
        }

        // Record compaction event in history.
        let summary_msg = vec![Message::assistant(&result.text)];
        ctx.append_history(
            None,
            &summary_msg,
            result.estimated_tokens,
            RecordKind::System,
            Some(SystemEvent::CompactionSummary),
            &[],
        );

        // Accumulate usage from the summarization LLM call WITHOUT re-anchoring: the summarizer
        // runs over a different message set (oldest turns + SUMMARIZE_PROMPT), so its
        // `prompt_tokens` does not reflect the main conversation. Bumping `cumulative_usage`
        // (operator budget) is correct; moving the prompt anchor would poison the next estimate.
        if let Some(u) = &usage {
            ctx.store.lock().await.accumulate_usage_no_anchor(u);
            ctx.token_budget
                .record_usage(u.prompt_tokens as u64, u.completion_tokens as u64);
        }

        // Check token budget after accumulating summarization usage.
        let snap = ctx.token_budget.snapshot();
        if snap.is_exceeded() {
            ctx.persist().await;
            return Ok(CompactOutcome::BudgetExceeded {
                consumed: snap.consumed,
                budget: snap.budget,
            });
        }

        any_summarized = true;
    }

    if any_summarized {
        ctx.persist().await;
    }

    Ok(if any_summarized {
        CompactOutcome::Compacted
    } else {
        CompactOutcome::NothingToCompact
    })
}

/// Compact context if it exceeds the budget.
/// Called at agent startup for restored agents.
pub(crate) async fn compact_if_needed(ctx: &AgentContext) -> Result<bool> {
    let effective_budget = ctx.config.effective_budget();
    let total_tokens = {
        let guard = ctx.store.lock().await;
        guard.total_estimated_tokens()
    };

    if total_tokens <= effective_budget {
        return Ok(false);
    }

    info!(total_tokens, effective_budget, "pre-loop compaction needed");
    match summarize_and_evict(ctx).await? {
        CompactOutcome::Compacted => Ok(true),
        CompactOutcome::NothingToCompact => Ok(false),
        // Budget exceeded during pre-loop compaction — log and proceed.
        // The pre-call budget check at the top of the first round will catch it.
        CompactOutcome::BudgetExceeded { consumed, budget } => {
            warn!(
                consumed,
                budget, "token budget exceeded during pre-loop compaction"
            );
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retry::RetryPolicy;
    use crate::test_support::{MapSource, ctx_from_source, profile};
    use crate::test_support::{tool_result_msg, user_msg};
    use just_llm_client::LlmBackend;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Fast retry policy so the wiremock suite stays snappy.
    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_retries: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            retry_timeout: Duration::from_secs(10),
        }
    }

    /// A real OpenAI-compatible backend pointed at `uri` (a wiremock server).
    fn wiremock_backend(uri: &str) -> Arc<dyn LlmBackend> {
        just_llm_client::provider::OpenAiCompatBackend::new(
            reqwest::Client::builder().use_rustls_tls(),
            "test-key",
            Some(uri),
        )
        .expect("openai-compat backend constructs without network")
    }

    /// Mount a 200 JSON completion response carrying `content` — the summarizer
    /// calls the non-streaming `chat_completion`, so this is not SSE.
    async fn mount_summary(server: &MockServer, content: &str) {
        let body = format!(
            "{{\"id\":\"1\",\"object\":\"chat.completion\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"{content}\"}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}}}"
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_raw(body.into_bytes(), "application/json"),
            )
            .mount(server)
            .await;
    }

    /// An agent whose profiles point at a wiremock server that answers every
    /// completion with the text `summary`. The server is returned alongside and
    /// must be held for the test's lifetime: dropping it frees the port and, under
    /// parallel tests, another binder can take it over while the client still aims
    /// at it.
    async fn summary_ctx() -> (AgentContext, MockServer) {
        let server = MockServer::start().await;
        mount_summary(&server, "summary").await;
        let map = HashMap::from([(("ep1".to_string()), wiremock_backend(&server.uri()))]);
        let ctx = ctx_from_source(
            vec![profile("p1", "ep1", 500_000)],
            Arc::new(MapSource(map)),
            fast_policy(),
        )
        .await;
        (ctx, server)
    }

    /// The summarizer input budget for `test_config`'s 500K window: window −
    /// output reserve − summary max. A queue-head turn above this is the wedge.
    const SUMMARIZER_INPUT_BUDGET: usize = 500_000 - 8_192 - 1_200;

    /// Consumer-level pin: the wedge face cuts through the shared helper, so
    /// the gap marker must survive here — a future local re-derivation of the
    /// slice in this file would fail loudly.
    #[test]
    fn wedge_slice_declares_its_gap() {
        let turn = Turn {
            id: crate::context::turn::TurnId(7),
            messages: vec![tool_result_msg(oversized_content(), "w0")],
            estimated_tokens: 0,
            kind: TurnKind::Conversation,
        };
        let sliced = slice_oversized_turn(&turn, SUMMARIZER_INPUT_BUDGET);
        let content = sliced.messages[0].content().unwrap_or_default();
        assert!(
            content.contains("chars omitted"),
            "wedge slice must declare its gap"
        );
    }

    /// A conversation turn whose estimate provably exceeds the budget: 600K CJK
    /// chars at tokenx's ~1 token/char. The assertion pins the precondition so a
    /// tokenx drift fails loudly here instead of silently un-testing the slice.
    fn oversized_content() -> String {
        "错".repeat(600_000)
    }

    /// The wedge stalls nothing: after its predecessors are summarized, the
    /// oversized head turn itself is slice-summarized and evicted, and the
    /// loop reports Compacted instead of silently returning NothingToCompact
    /// (the 2026-09-06 incident path).
    #[tokio::test]
    async fn oversized_head_turn_is_slice_summarized_and_evicted() {
        let (ctx, _server) = summary_ctx().await;
        {
            let mut s = ctx.store.lock().await;
            s.push_turn(vec![tool_result_msg(oversized_content(), "w0")]);
            s.push_turn(vec![user_msg("small one")]);
            let wedge_est = s.turns()[0].estimated_tokens;
            assert!(
                wedge_est > SUMMARIZER_INPUT_BUDGET,
                "precondition: wedge ({wedge_est}) must exceed the input budget ({SUMMARIZER_INPUT_BUDGET})"
            );
        }
        let outcome = summarize_and_evict(&ctx).await.unwrap();
        assert!(
            matches!(outcome, CompactOutcome::Compacted),
            "oversized head turn must not wedge compaction into NothingToCompact"
        );
        let s = ctx.store.lock().await;
        // Turns within budget legitimately survive (compaction only drains the
        // excess); the wedge itself must be gone.
        let wedged = s
            .turns()
            .iter()
            .filter(|t| !t.is_pinned())
            .any(|t| t.estimated_tokens > SUMMARIZER_INPUT_BUDGET);
        assert!(
            !wedged,
            "no turn above the summarizer input budget may survive compaction"
        );
        let survivor = s
            .turns()
            .iter()
            .filter(|t| !t.is_pinned())
            .any(|t| t.estimated_tokens > 0);
        assert!(survivor, "the in-budget turn must legitimately survive");
        assert!(
            s.pinned_labels().iter().any(|l| l == "context_summary"),
            "the slice summary must be pinned"
        );
    }

    /// A lone oversized turn (nothing else evictable) is handled by the same
    /// slice path: compacted, not wedged.
    #[tokio::test]
    async fn lone_oversized_turn_is_slice_summarized() {
        let (ctx, _server) = summary_ctx().await;
        ctx.store
            .lock()
            .await
            .push_turn(vec![tool_result_msg(oversized_content(), "w0")]);
        let outcome = summarize_and_evict(&ctx).await.unwrap();
        assert!(matches!(outcome, CompactOutcome::Compacted));
        let s = ctx.store.lock().await;
        let remaining = s.turns().iter().filter(|t| !t.is_pinned()).count();
        assert_eq!(remaining, 0);
    }

    /// A multimodal message survives the slice with its text: the
    /// pre-fix behavior dropped the whole message because `content()`
    /// is `None` for non-text content.
    #[test]
    fn slice_keeps_the_text_of_a_parts_message() {
        let message = Message::user_parts(vec![
            just_llm_client::types::generation::ContentPart::Text {
                text: "chart caption".to_owned(),
            },
            just_llm_client::types::generation::ContentPart::Image {
                source: just_llm_client::types::generation::ImageSource::Base64 {
                    data: "aGk=".to_owned(),
                    media_type: "image/png".to_owned(),
                },
                detail: None,
            },
        ]);
        let turn = Turn {
            id: crate::context::turn::TurnId(9),
            messages: vec![message],
            estimated_tokens: 0,
            kind: TurnKind::Conversation,
        };
        let sliced = slice_oversized_turn(&turn, SUMMARIZER_INPUT_BUDGET);
        let content = sliced.messages[0].content().unwrap_or_default();
        assert!(content.contains("chart caption"), "got: {content}");
        // The image itself contributes no words — text only.
        assert!(!content.contains("aGk="));
    }
}
