//! Round-loop context-budget gates.
//!
//! Owns the two phase gates the round loop runs around the LLM call: the pre-call gate
//! (progressive warnings, auto-compact at the threshold) and the post-stream gate (usage
//! accumulation, budget warnings, exhaustion). The budget primitives (estimation, warning
//! injection, compaction) live in [`crate::context`] and the shared tree-wide counter in
//! [`crate::token_budget`]; the round loop in `crate::runner` calls both gates and
//! dispatches on [`BudgetAction`].

use kallipai_common::timefmt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::agent_task::AgentContext;
use crate::context::{
    CompactOutcome, CompactionVerdict, check_progressive_warnings, check_token_budget_warnings,
    note_compaction_failure, summarize_and_evict,
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
    /// End the round with this error: compaction cannot continue — the class table
    /// ruled the failure terminal, or a caller parked a stop in the ledger to take.
    Fail(anyhow::Error),
}

/// Progressive-warning + auto-compact gate run before the LLM request. `Recompose` when a warning
/// was injected or context was compacted (the loop re-composes); `Return` on budget exhaustion or
/// a raced-in cancel; `Fail` when compaction cannot continue (the class table ruled the failure
/// terminal, or a caller parked a stop in the ledger for this gate to take); `Proceed` sends.
pub(crate) async fn enforce_pre_call_budget(
    ctx: &mut AgentContext,
    prompt_tokens: usize,
    round: usize,
    round_cancel: &CancellationToken,
) -> BudgetAction {
    if prompt_tokens == 0 {
        // No count, no ruling: this call cannot judge the context, so a parked
        // stop and an armed latch stay where they are for the call that can.
        // Both answer for the context a round would send, which this call has
        // not measured.
        return BudgetAction::Proceed;
    }
    let effective_budget = ctx.config.effective_budget();
    let usage_pct = prompt_tokens * 100 / effective_budget;
    let now = timefmt::now_epoch();

    // Step 1: Progressive warnings.
    if check_progressive_warnings(ctx, usage_pct, effective_budget).await {
        return BudgetAction::Recompose;
    }

    // Step 2: Auto-compact at the highest threshold.
    let auto_threshold = ctx.config.auto_compact_threshold() as usize;
    if usage_pct < auto_threshold {
        // Back within the budget: the stretch a failure opened is over.
        ctx.compaction.clear(now);
        return BudgetAction::Proceed;
    }
    info!(
        prompt_tokens,
        context_window = ctx.config.context_window_tokens,
        "context exceeds budget"
    );
    if let Some(message) = ctx.compaction.take_pending_stop() {
        // A stop recorded outside the round ends it here: the summarizer already
        // failed terminally for this stretch, and a request over budget is what
        // this gate exists to prevent. The parked stop is taken once, so a later
        // call reads the context afresh.
        return BudgetAction::Fail(anyhow::anyhow!(message));
    }
    if ctx.compaction.is_latched(now) {
        // The backoff the last failure armed is still running: this round sends
        // its request without asking the summarizer again.
        debug!(
            attempt = ctx.compaction.attempts(),
            "compaction is waiting out its backoff"
        );
    } else {
        match summarize_and_evict(ctx).await {
            Ok(CompactOutcome::Compacted) => {
                // A summary landed: the stretch is over.
                ctx.compaction.clear(timefmt::now_epoch());
                return BudgetAction::Recompose;
            }
            Ok(CompactOutcome::NothingToCompact) => {} // fall through
            Ok(CompactOutcome::BudgetExceeded { consumed, budget }) => {
                return BudgetAction::Return(AgentOutcome::TokenBudgetExceeded {
                    consumed,
                    budget,
                });
            }
            Err(e) => match note_compaction_failure(ctx, &e, round).await {
                CompactionVerdict::Retry { after } => {
                    debug!(
                        ?after,
                        "compaction failed; the next attempt waits out the backoff"
                    );
                }
                CompactionVerdict::Defer { reason } => {
                    debug!(
                        reason,
                        "compaction failed; the endpoint is the chain's business"
                    );
                }
                CompactionVerdict::Stop { message } => {
                    return BudgetAction::Fail(e.context(message));
                }
            },
        }
    }
    if round_cancel.is_cancelled() {
        return BudgetAction::Return(AgentOutcome::Cancelled);
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

    use crate::context::CompactionLedger;
    use crate::retry::decision::RETRY_JITTER_MAX;
    use crate::retry::{RetryPolicy, max_sends};
    use crate::test_support::{
        FAILURE_PIN_WINDOW, RecordingHealth, ctx_from_source, make_ctx, mock_source, mount_summary,
        profile, seed_over_budget_store, set_context_window, usage,
    };
    use kallipai_common::retry::RetryKind;
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

    /// An over-budget context whose summarizer answers `status`: every gate call
    /// reaches the summarizer and fails there. `context_thresholds = vec![80]`
    /// drops the progressive-warning level so one call reaches the compaction
    /// branch.
    async fn gate_ctx(status: u16, policy: RetryPolicy) -> (AgentContext, wiremock::MockServer) {
        let (source, server) = mock_source(&["ep1"], status).await;
        let mut ctx = ctx_from_source(vec![profile("p1", "ep1", 500_000)], source, policy).await;
        ctx.config.context_thresholds = vec![80];
        set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
        seed_over_budget_store(&mut ctx).await;
        (ctx, server)
    }

    /// A policy whose first backoff outlasts the test body, so what a pin
    /// observes is the latch rather than a second attempt.
    fn latching_policy() -> RetryPolicy {
        RetryPolicy {
            base_delay: Duration::from_secs(1),
            ..RetryPolicy::default()
        }
    }

    /// How many requests the mock server saw. Every request here is a summarizer call:
    /// the gate issues none of its own.
    async fn summary_posts(server: &wiremock::MockServer) -> usize {
        server.received_requests().await.unwrap_or_default().len()
    }

    /// One gate call with the prompt count the caller would pass and a fresh
    /// cancel token.
    async fn gate(ctx: &mut AgentContext, round: usize) -> BudgetAction {
        let prompt_tokens = ctx.store.lock().await.total_estimated_tokens();
        enforce_pre_call_budget(ctx, prompt_tokens, round, &CancellationToken::new()).await
    }

    /// A `400` from the summarizer is the class table's terminal class: no send
    /// can help, so the round ends with the reason instead of a second attempt.
    #[tokio::test]
    async fn a_request_permanent_compaction_failure_stops_the_round_without_a_second_attempt() {
        let (mut ctx, server) = gate_ctx(400, RetryPolicy::default()).await;

        let action = gate(&mut ctx, 0).await;

        let BudgetAction::Fail(error) = action else {
            panic!("a terminal compaction failure must end the round");
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("compaction failed"),
            "the message must name the failure: {message}"
        );
        assert!(
            message.contains("the request itself was rejected"),
            "the message must carry the class label: {message}"
        );
        assert_eq!(summary_posts(&server).await, 1, "one attempt per gate call");
        assert_eq!(ctx.compaction.attempts(), 1, "the stretch spent one send");
        assert!(
            ctx.store.lock().await.retry_log.is_empty(),
            "a terminal class writes no retry record"
        );
    }

    /// The latch holds the gate off until the backoff it armed runs out: three
    /// over-budget rounds ask the summarizer once, and the expiry reopens it.
    #[tokio::test]
    async fn a_latched_compaction_is_not_attempted_again_before_the_deadline() {
        let (mut ctx, server) = gate_ctx(500, latching_policy()).await;

        for round in 0..3 {
            let action = gate(&mut ctx, round).await;
            assert!(
                matches!(action, BudgetAction::Proceed),
                "round {round}: a transient failure leaves the send to the round"
            );
        }
        assert_eq!(
            summary_posts(&server).await,
            1,
            "the armed backoff covers the later rounds"
        );

        ctx.compaction.expire_latch(timefmt::now_epoch());
        let action = gate(&mut ctx, 3).await;

        assert!(matches!(action, BudgetAction::Proceed));
        assert_eq!(
            summary_posts(&server).await,
            2,
            "the expiry reopens the gate"
        );
    }

    /// A transient failure arms the policy's own backoff on the ledger and hands
    /// the round back to the main send: the send is charged to the compaction
    /// stretch, never to the round's retry ledger.
    #[tokio::test]
    async fn a_transient_compaction_failure_arms_the_policy_backoff_and_the_round_proceeds() {
        let (mut ctx, server) = gate_ctx(500, latching_policy()).await;
        let before = timefmt::now_epoch();

        let action = gate(&mut ctx, 0).await;
        let after = timefmt::now_epoch();

        assert!(matches!(action, BudgetAction::Proceed));
        assert_eq!(summary_posts(&server).await, 1);
        assert_eq!(ctx.compaction.attempts(), 1, "the stretch spent one send");
        assert_eq!(
            ctx.episode.attempts(),
            0,
            "the failure is the compaction stretch's, not the round's"
        );

        let log = ctx.store.lock().await.retry_log.clone();
        let record = log.last().expect("a retryable class writes its record");
        assert_eq!(record.kind, RetryKind::Server);
        let delay = record.delay_secs;
        assert!(
            delay >= 1.0 && delay <= 1.0 + RETRY_JITTER_MAX.as_secs_f64(),
            "the first backoff is the policy's base delay plus jitter, got {delay}"
        );
        let until = ctx
            .compaction
            .latch_until()
            .expect("a retryable class arms the latch");
        // The deadline is the recorded wait rounded up to a whole second and
        // never below one: a sub-second backoff still holds the gate off.
        let expected = (delay.ceil() as u64).max(1);
        assert!(
            until >= before + expected && until <= after + expected,
            "the deadline is the failure moment plus the recorded delay: {until}"
        );
        assert!(ctx.compaction.is_latched(timefmt::now_epoch()));
    }

    /// The stretch is bounded by the policy's send budget: once spent, the next
    /// failure is terminal and the message says which verdict ended it.
    #[tokio::test]
    async fn a_spent_compaction_ledger_stops_the_round() {
        let policy = RetryPolicy {
            max_retries: 2,
            base_delay: Duration::from_secs(1),
            ..RetryPolicy::default()
        };
        let budget = max_sends(&policy);
        let (mut ctx, server) = gate_ctx(500, policy).await;

        for round in 0..budget - 1 {
            ctx.compaction.expire_latch(timefmt::now_epoch());
            let action = gate(&mut ctx, round as usize).await;
            assert!(
                matches!(action, BudgetAction::Proceed),
                "attempt {} of {budget} is still retryable",
                round + 1
            );
        }
        ctx.compaction.expire_latch(timefmt::now_epoch());
        let action = gate(&mut ctx, budget as usize).await;

        let BudgetAction::Fail(error) = action else {
            panic!("a spent stretch must end the round");
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("retry budget exhausted"),
            "the message must carry the verdict's reason: {message}"
        );
        assert_eq!(ctx.compaction.attempts(), budget);
        assert_eq!(
            summary_posts(&server).await as u32,
            budget,
            "one attempt per charged send, no more"
        );
    }

    /// A compaction that finally succeeds ends the stretch: the attempt count and
    /// the latch start over, so the next failure is judged on its own.
    #[tokio::test]
    async fn a_successful_compaction_clears_the_ledger() {
        let (mut ctx, server) = gate_ctx(500, latching_policy()).await;

        let action = gate(&mut ctx, 0).await;
        assert!(matches!(action, BudgetAction::Proceed));
        assert_eq!(ctx.compaction.attempts(), 1);

        // The summary mock is the narrower one and outranks the catch-all `500`.
        mount_summary(&server, "summary").await;
        ctx.compaction.expire_latch(timefmt::now_epoch());
        let action = gate(&mut ctx, 1).await;

        assert!(
            matches!(action, BudgetAction::Recompose),
            "a compacted context re-enters the loop"
        );
        assert_eq!(ctx.compaction.attempts(), 0, "the stretch ended");
        assert!(
            !ctx.compaction.is_latched(timefmt::now_epoch()),
            "a cleared ledger carries no latch"
        );
    }

    /// A round that comes back within the budget ends the stretch: the next
    /// gate call drops the ledger, so a later over-budget round is judged
    /// afresh instead of inheriting the spent attempt count.
    #[tokio::test]
    async fn a_round_back_within_budget_clears_the_ledger() {
        let (mut ctx, server) = gate_ctx(500, latching_policy()).await;

        let action = gate(&mut ctx, 0).await;
        assert!(matches!(action, BudgetAction::Proceed));
        assert_eq!(ctx.compaction.attempts(), 1);

        // A wider window puts the same store back within the budget.
        set_context_window(&mut ctx, 500_000).await;
        let action = gate(&mut ctx, 1).await;

        assert!(matches!(action, BudgetAction::Proceed));
        assert_eq!(
            ctx.compaction.attempts(),
            0,
            "a round back within budget starts the stretch over"
        );
        assert!(!ctx.compaction.is_latched(timefmt::now_epoch()));
        assert_eq!(
            summary_posts(&server).await,
            1,
            "the cleared ledger asks the summarizer for nothing"
        );
    }

    /// A class the failover chain owns is deferred, not stopped: the observation
    /// reaches the endpoint-health view and the retry log, the latch keeps the
    /// gate quiet until the main send's outcome is known, and the round sends.
    #[tokio::test]
    async fn a_chain_owned_compaction_failure_is_deferred_to_the_main_send() {
        let (mut ctx, server) = gate_ctx(429, latching_policy()).await;
        let health = Arc::new(RecordingHealth::default());
        ctx.profile_pacing = Some(health.clone());

        let action = gate(&mut ctx, 0).await;

        assert!(matches!(action, BudgetAction::Proceed));
        assert_eq!(summary_posts(&server).await, 1);
        assert_eq!(
            health.recorded().len(),
            1,
            "the class is written to the endpoint-health view"
        );
        let log = ctx.store.lock().await.retry_log.clone();
        assert_eq!(log.len(), 1, "the class reaches the retry log");
        assert_eq!(log[0].kind, RetryKind::RateLimit);
        assert_eq!(
            log[0].delay_secs, 0.0,
            "the main send owns the wait, so the record carries none"
        );
    }
    /// The stretch's clock starts at its first failure, not at the moment the
    /// ledger was built: an agent that carried a fresh ledger for longer than
    /// the policy's deadline must still get that first retry. A clock started
    /// at construction reads the first attempt as past the deadline and ends
    /// the round on it.
    #[tokio::test]
    async fn a_first_failure_long_after_the_ledger_was_built_still_retries() {
        let (mut ctx, server) = gate_ctx(500, latching_policy()).await;
        // The ledger was built when the agent spawned, 400s before the failure.
        ctx.compaction = CompactionLedger::new(timefmt::now_epoch() - 400);
        assert!(
            ctx.config.retry_policy.retry_timeout < Duration::from_secs(400),
            "precondition: a stale clock is what the deadline would blow"
        );

        let action = gate(&mut ctx, 0).await;

        assert!(
            matches!(action, BudgetAction::Proceed),
            "the first failure of a stretch is retryable"
        );
        assert_eq!(ctx.compaction.attempts(), 1, "the stretch spent one send");
        assert!(
            ctx.compaction.is_latched(timefmt::now_epoch()),
            "the backoff the verdict armed is running"
        );

        let action = gate(&mut ctx, 1).await;

        assert!(
            matches!(action, BudgetAction::Proceed),
            "the latched gate sends without asking again"
        );
        assert_eq!(
            summary_posts(&server).await,
            1,
            "one request for the failure: the latch holds the gate off"
        );
    }

    /// A gate call that cannot judge the context — no token count — neither
    /// consumes nor erases a parked stop: the verdict answers for the context a
    /// round would send, which an unmeasured call has not decided. The next call
    /// that has a count takes it.
    #[tokio::test]
    async fn a_gate_call_without_a_token_count_leaves_the_parked_stop_parked() {
        let (mut ctx, server) = gate_ctx(400, RetryPolicy::default()).await;
        crate::context::compact_pre_loop(&mut ctx).await;
        assert_eq!(ctx.compaction.attempts(), 1, "the wrapper charged the send");

        let unmeasured = enforce_pre_call_budget(&mut ctx, 0, 0, &CancellationToken::new()).await;
        assert!(
            matches!(unmeasured, BudgetAction::Proceed),
            "without a count the gate has nothing to rule on"
        );

        let action = gate(&mut ctx, 1).await;

        let BudgetAction::Fail(error) = action else {
            panic!("the stop is still parked for the call that can rule");
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("the request itself was rejected"),
            "the failure carries the parked verdict: {message}"
        );
        assert_eq!(
            summary_posts(&server).await,
            1,
            "taking the parked stop costs no second request"
        );
    }

    /// A stop parked while the context was over budget is dropped once the
    /// context is back within it: the verdict spoke for a context that no longer
    /// exists, so the next over-budget round judges afresh instead of failing on
    /// a verdict the stretch no longer stands behind.
    #[tokio::test]
    async fn a_stop_parked_before_the_context_fit_again_is_dropped() {
        let (mut ctx, server) = gate_ctx(400, RetryPolicy::default()).await;
        crate::context::compact_pre_loop(&mut ctx).await;
        assert_eq!(ctx.compaction.attempts(), 1, "the wrapper charged the send");

        // A wider window puts the same store back within the budget: the gate
        // ends the stretch, stop included.
        set_context_window(&mut ctx, 500_000).await;
        let settled = gate(&mut ctx, 1).await;
        assert!(matches!(settled, BudgetAction::Proceed));

        // Over budget again: a stop that survived the stretch would fail this
        // call without a request, so the request count tells the two apart.
        set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
        let action = gate(&mut ctx, 2).await;
        assert!(
            matches!(action, BudgetAction::Fail(_)),
            "the fresh summarizer failure is the terminal class"
        );
        assert_eq!(
            summary_posts(&server).await,
            2,
            "the dropped stop does not answer for the new stretch"
        );
    }
}
