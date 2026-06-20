//! Context compaction: summarize old turns into a pinned summary to stay within budget.
//!
//! [`summarize_and_evict`] runs bounded summarize-and-evict passes; [`compact_if_needed`] is the
//! pre-loop wrapper for restored agents. [`CompactOutcome`] reports the result.

use anyhow::Result;
use just_llm_client::types::chat::ChatMessage;
use tracing::{info, warn};

use crate::agent_task::AgentContext;
use crate::context::AgenticContext;
use crate::history::{RecordKind, SystemEvent};

/// Outcome of context compaction via [`summarize_and_evict`].
pub(crate) enum CompactOutcome {
    /// Some turns were summarized and evicted.
    Compacted,
    /// No turns to compact (context already within budget).
    NothingToCompact,
    /// Token budget exceeded during summarization.
    BudgetExceeded { consumed: u64, budget: u64 },
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
                .and_then(|m| m.content().map(|c| c.to_owned()));

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
                break;
            }
            (window, existing_summary)
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
            guard.replace_pin("context_summary", ChatMessage::assistant(&result.text))?;
            guard.evict_turns(result.source_turns);
            guard.reset_context_warnings();
            info!(
                source_turns = result.source_turns,
                estimated_tokens = result.estimated_tokens,
                "summarize pass completed"
            );
        }

        // Record compaction event in history.
        let summary_msg = vec![ChatMessage::assistant(&result.text)];
        ctx.append_history(
            None,
            &summary_msg,
            result.estimated_tokens,
            RecordKind::System,
            Some(SystemEvent::CompactionSummary),
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
