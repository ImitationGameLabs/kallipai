//! Context-budget warning injection: notify the LLM as it approaches token limits.
//!
//! [`check_progressive_warnings`] fires on context-window usage (per-agent); [`check_token_budget_warnings`]
//! on daemon-wide budget usage. Each injects a `[system]` user turn the first time a threshold is
//! crossed, and is a no-op below the lowest threshold. These mutate the store (push a turn + mark
//! the threshold fired) so the warning fires once per level.

use just_agent_common::tokens::format_tokens_m;
use just_llm_client::types::chat::ChatMessage;
use tracing::info;

use crate::agent_task::AgentContext;
use crate::context::AgenticContext;
use crate::history::RecordKind;
use crate::token_budget::TokenBudgetSnapshot;

/// Check progressive warning thresholds and inject a [system] message if crossed.
/// Returns `true` if a warning was injected (caller should continue to re-compose).
pub(crate) async fn check_progressive_warnings(
    ctx: &mut AgentContext,
    usage_pct: usize,
    effective_budget: usize,
) -> bool {
    let warnings = ctx.config.warning_thresholds();
    if warnings.is_empty() {
        return false;
    }

    let mut guard = ctx.store.lock().await;

    // If usage is below the lowest threshold, reset warning state.
    let lowest = warnings[0] as usize;
    if usage_pct < lowest {
        guard.reset_context_warnings();
        return false;
    }

    // Find the highest crossed threshold that hasn't been warned yet.
    let Some(threshold) = warnings
        .iter()
        .rev()
        .find(|&&t| usage_pct >= t as usize && guard.should_warn(t))
        .copied()
    else {
        return false;
    };

    let msg = format!(
        "[system]\nContext usage is at {}% ({} / {} tokens). \
         Use context_status to review current turns, then context_evict with a \
         summary to evict all turns while preserving key facts.",
        threshold,
        effective_budget * threshold as usize / 100,
        effective_budget
    );

    guard.mark_warned(threshold);
    let msgs = vec![ChatMessage::user(&msg)];
    let (turn_id, estimated_tokens) = guard.push_turn(msgs.clone());
    drop(guard);
    ctx.append_history(
        Some(turn_id.0),
        &msgs,
        estimated_tokens,
        RecordKind::Turn,
        None,
    );
    info!(threshold, "injected context warning");
    true
}

/// Check token budget warning thresholds and inject a [system] message if crossed.
/// Returns `true` if a warning was injected (caller should continue to re-compose).
pub(crate) async fn check_token_budget_warnings(
    ctx: &mut AgentContext,
    snap: &TokenBudgetSnapshot,
) -> bool {
    let warnings = &ctx.config.token_budget_warnings;
    if warnings.is_empty() {
        return false;
    }

    let pct = snap.usage_pct();
    // usage_pct() returns 0 when budget is 0, so no warnings fire.
    if pct == 0 {
        return false;
    }

    let mut guard = ctx.store.lock().await;

    // Find the highest crossed threshold that hasn't been warned yet.
    let Some(threshold) = warnings
        .iter()
        .rev()
        .find(|&&t| pct >= t && guard.should_warn_budget(t))
        .copied()
    else {
        return false;
    };

    let msg = format!(
        "[system]\nToken budget usage is at {}% ({} consumed, {} remaining of {}). \
         Wrap up your current work concisely.",
        threshold,
        format_tokens_m(snap.consumed),
        format_tokens_m(snap.remaining()),
        format_tokens_m(snap.budget)
    );

    guard.mark_budget_warned(threshold);
    let msgs = vec![ChatMessage::user(&msg)];
    let (turn_id, estimated_tokens) = guard.push_turn(msgs.clone());
    drop(guard);
    ctx.append_history(
        Some(turn_id.0),
        &msgs,
        estimated_tokens,
        RecordKind::Turn,
        None,
    );
    info!(
        threshold,
        consumed = snap.consumed,
        budget = snap.budget,
        "injected token budget warning"
    );
    true
}
