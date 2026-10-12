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
    usage: Option<&just_llm_client::types::generation::Usage>,
) -> BudgetAction {
    if let Some(usage) = usage {
        ctx.store.lock().await.accumulate_usage(usage);
        ctx.token_budget
            .record_usage(usage.prompt_tokens as u64, usage.completion_tokens as u64);
        if let Some(agent_id) = ctx.config.agent_id.as_ref() {
            ctx.usage_stats.record(agent_id, usage);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::retry::RetryPolicy;
    use crate::test_support::{
        FAILURE_PIN_WINDOW, RecordingHealth, ctx_from_source, make_ctx, mock_source, profile,
        seed_over_budget_store, set_context_window, usage,
    };
    use std::sync::Arc;
    use std::time::Duration;

    /// Unlimited gate passthrough: the post-stream gate must fall
    /// through to `Proceed` even with an enormous accumulated
    /// consumption — no warning injection (usage_pct is 0 when
    /// unlimited) and no exhaustion (`is_exceeded` short-circuits
    /// false on the unlimited snapshot).
    #[tokio::test]
    async fn post_stream_gate_proceeds_when_budget_is_unlimited() {
        let mut ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
        ctx.token_budget.set_unlimited();
        ctx.token_budget.record_usage(u64::MAX / 2, u64::MAX / 2);
        assert!(ctx.token_budget.snapshot().unlimited);

        let action = enforce_post_stream_budget(&mut ctx, Some(&usage(1_000))).await;

        assert!(
            matches!(action, BudgetAction::Proceed),
            "an unlimited budget must never gate a round"
        );
    }

    /// An over-budget context whose summarizer answers `400`: every gate call reaches
    /// the summarizer and fails there. `context_thresholds = vec![80]` drops the
    /// progressive-warning level so one call reaches the compaction branch.
    async fn failing_gate_ctx(policy: RetryPolicy) -> (AgentContext, wiremock::MockServer) {
        let (source, server) = mock_source(&["ep1"], 400).await;
        let mut ctx = ctx_from_source(vec![profile("p1", "ep1", 500_000)], source, policy).await;
        ctx.config.context_thresholds = vec![80];
        set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
        seed_over_budget_store(&mut ctx).await;
        (ctx, server)
    }

    /// How many requests the mock server saw. Every request here is a summarizer call:
    /// the gate issues none of its own.
    async fn summary_posts(server: &wiremock::MockServer) -> usize {
        server.received_requests().await.unwrap_or_default().len()
    }

    /// A round whose compaction fails still sends its own request: the gate warns and
    /// proceeds, so the failure changes nothing the caller can see.
    #[tokio::test]
    async fn pre_call_gate_proceeds_when_compaction_fails() {
        let (mut ctx, server) = failing_gate_ctx(RetryPolicy::default()).await;
        let prompt_tokens = ctx.store.lock().await.total_estimated_tokens();

        let action =
            enforce_pre_call_budget(&mut ctx, prompt_tokens, &CancellationToken::new()).await;

        assert!(
            matches!(action, BudgetAction::Proceed),
            "a failed compaction must not stop the round"
        );
        assert_eq!(summary_posts(&server).await, 1, "one attempt per gate call");
    }

    /// One gate call per over-budget round, and each call asks the summarizer again.
    #[tokio::test]
    async fn a_failed_compaction_is_attempted_again_by_the_next_round() {
        let (mut ctx, server) = failing_gate_ctx(RetryPolicy::default()).await;

        for round in 0..3 {
            let prompt_tokens = ctx.store.lock().await.total_estimated_tokens();
            let action =
                enforce_pre_call_budget(&mut ctx, prompt_tokens, &CancellationToken::new()).await;
            assert!(
                matches!(action, BudgetAction::Proceed),
                "round {round}: a failed compaction must not stop the round"
            );
        }

        assert_eq!(
            summary_posts(&server).await,
            3,
            "every over-budget round asks the summarizer again"
        );
    }

    /// The failure never enters the retry shell: with a 30s base delay a retried call
    /// would sit in the wait far past this test's own 5s bound (the production retry
    /// timeout default of 300s is never in play here), and no endpoint fact is written.
    #[tokio::test]
    async fn a_failed_compaction_is_not_retried_with_backoff() {
        let policy = RetryPolicy {
            base_delay: Duration::from_secs(30),
            ..RetryPolicy::default()
        };
        let (mut ctx, server) = failing_gate_ctx(policy).await;
        let health = Arc::new(RecordingHealth::default());
        ctx.endpoint_health = Some(health.clone());

        let started = std::time::Instant::now();
        for _ in 0..2 {
            let prompt_tokens = ctx.store.lock().await.total_estimated_tokens();
            let _ =
                enforce_pre_call_budget(&mut ctx, prompt_tokens, &CancellationToken::new()).await;
        }
        let elapsed = started.elapsed();

        assert_eq!(
            summary_posts(&server).await,
            2,
            "one attempt per call: the failure is not re-sent inside the call"
        );
        assert_eq!(
            ctx.episode.attempts(),
            0,
            "no send is charged to the retry ledger: the failure bypassed the retry shell"
        );
        assert!(
            health.recorded().is_empty(),
            "a summary failure writes no endpoint fact"
        );
        // Guardrail: a shell that retried would wait out the 30s base delay, far past
        // this bound. The attempts ledger above is the primary evidence.
        assert!(
            elapsed < Duration::from_secs(5),
            "a retried attempt would wait out the 30s base delay; took {elapsed:?}"
        );
    }
}
