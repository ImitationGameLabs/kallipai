//! Round-loop context-budget gates.
//!
//! Owns the two phase gates the round loop runs around the LLM call: the pre-call gate
//! (progressive warnings, auto-compact at the threshold) and the post-stream gate (usage
//! accumulation, budget warnings, exhaustion). The budget primitives (estimation, warning
//! injection, compaction) live in [`crate::context`] and the shared tree-wide counter in
//! [`crate::token_budget`]; the round loop in `crate::runner` calls both gates and
//! dispatches on [`BudgetAction`].

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::agent_task::AgentContext;
use crate::context::{
    CompactOutcome, check_progressive_warnings, check_token_budget_warnings, summarize_and_evict,
};
use crate::event::AgentOutcome;

// ---------------------------------------------------------------------------
// Round-loop budget gates
// ---------------------------------------------------------------------------

/// A budget-gate phase's verdict on whether the round loop should proceed. Makes a phase's
/// early-exit contract (`continue` / `return`) explicit at the call site.
pub(crate) enum BudgetAction {
    /// Re-compose context and re-enter the loop (a warning was injected or context was compacted).
    Recompose,
    /// Exit the loop with this terminal outcome.
    Return(AgentOutcome),
    /// Fall through to the next phase.
    Proceed,
}

/// Progressive-warning + auto-compact gate run before the LLM request. `Recompose` when a warning
/// was injected or context was compacted (the loop re-composes); `Return` on budget exhaustion or
/// a raced-in cancel; `Proceed` to send the request.
pub(crate) async fn enforce_pre_call_budget(
    ctx: &mut AgentContext,
    prompt_tokens: usize,
    round_cancel: &CancellationToken,
) -> BudgetAction {
    if prompt_tokens == 0 {
        return BudgetAction::Proceed;
    }
    let effective_budget = ctx.config.effective_budget();
    let usage_pct = prompt_tokens * 100 / effective_budget;

    // Step 1: Progressive warnings.
    if check_progressive_warnings(ctx, usage_pct, effective_budget).await {
        return BudgetAction::Recompose;
    }

    // Step 2: Auto-compact at the highest threshold.
    let auto_threshold = ctx.config.auto_compact_threshold() as usize;
    if usage_pct >= auto_threshold {
        info!(
            prompt_tokens,
            context_window = ctx.config.context_window_tokens,
            "context exceeds budget"
        );
        match summarize_and_evict(ctx).await {
            Ok(CompactOutcome::Compacted) => return BudgetAction::Recompose,
            Ok(CompactOutcome::NothingToCompact) => {} // fall through
            Ok(CompactOutcome::BudgetExceeded { consumed, budget }) => {
                return BudgetAction::Return(AgentOutcome::TokenBudgetExceeded {
                    consumed,
                    budget,
                });
            }
            Err(e) => warn!("summarize_and_evict failed: {e:#}"),
        }
        if round_cancel.is_cancelled() {
            return BudgetAction::Return(AgentOutcome::Cancelled);
        }
    }

    BudgetAction::Proceed
}

/// Post-stream budget gate: accumulate usage, inject budget warnings, and check exhaustion.
/// `Recompose` when a warning was injected; `Return` on exhaustion; `Proceed` to handle tool calls.
pub(crate) async fn enforce_post_stream_budget(
    ctx: &mut AgentContext,
    usage: Option<&just_llm_client::types::chat::Usage>,
) -> BudgetAction {
    if let Some(usage) = usage {
        ctx.store.lock().await.accumulate_usage(usage);
        ctx.token_budget
            .record_usage(usage.prompt_tokens as u64, usage.completion_tokens as u64);
    }

    // Reload budget — the operator may have increased it via API mid-round.
    let snap = ctx.token_budget.snapshot();

    // Token budget warning injection (before exhaustion check).
    if check_token_budget_warnings(ctx, &snap).await {
        return BudgetAction::Recompose;
    }

    // Token budget exhaustion check (shared tree-wide counter).
    if snap.is_exceeded() {
        return BudgetAction::Return(AgentOutcome::TokenBudgetExceeded {
            consumed: snap.consumed,
            budget: snap.budget,
        });
    }

    BudgetAction::Proceed
}
