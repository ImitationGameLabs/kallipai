//! Context compaction: summarize old turns into a pinned summary to stay within budget.
//!
//! [`summarize_and_evict`] runs bounded summarize-and-evict passes; [`compact_if_needed`] is the
//! pre-loop wrapper for restored agents. [`CompactOutcome`] reports the result. A failed
//! summarizer call is classified, charged to [`CompactionLedger`] and decided by
//! [`note_compaction_failure`] — the one recorder the gate, the pre-loop wrapper and the
//! failover-advance arm share — into a [`CompactionVerdict`].

use std::time::Duration;

use anyhow::Result;
use just_llm_client::BackendError;
use just_llm_client::types::generation::{ContentPart, Message};
use kallipai_common::retry::{RetryAttempt, RetryRecord};
use kallipai_common::timefmt;
use tracing::{info, warn};

use super::turn::{Turn, TurnKind};
use super::{CONTEXT_SUMMARY_LABEL, summary_record};
use crate::agent_task::AgentContext;
use crate::context::AgenticContext;
use crate::history::{RecordKind, SystemEvent};
use crate::retry::decision::{self, Attempt, Decision, FailureClass, Phase, ShapeKey};
use crate::retry::{Episode, max_sends};
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
/// the [`IMAGE_PLACEHOLDER`] — the slice notes the image existed instead
/// of dropping it without a trace; the image itself remains in the turn's
/// history sidecar record.
fn message_text(message: &Message) -> String {
    if let Some(content) = message.content() {
        return content.to_owned();
    }
    let Some(parts) = message.content_parts() else {
        return String::new();
    };
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
            ContentPart::Image { .. } => super::IMAGE_PLACEHOLDER,
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
                .find(|t| t.label() == Some(CONTEXT_SUMMARY_LABEL))
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
            guard.replace_pin(CONTEXT_SUMMARY_LABEL, summary_record(&result.text))?;
            guard.evict_turns(result.source_turns);
            guard.reset_context_warnings();
            info!(
                source_turns = result.source_turns,
                estimated_tokens = result.estimated_tokens,
                "summarize pass completed"
            );
        }

        // Record compaction event in history.
        // The record reuses the pinned record's shape, so history and the
        // context show one injection form.
        let summary_msg = vec![summary_record(&result.text)];
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
            if let Some(agent_id) = ctx.config.agent_id.as_ref() {
                ctx.usage_stats.record(agent_id, u);
            }
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

/// Pre-loop compaction for a restored agent: run the wrapper and charge any
/// failure to the ledger.
///
/// The verdict is taken, but a stop is not acted on here: this runs outside a
/// round, where the lifecycle table admits only Idle -> Running, so a park is
/// not expressible and an error event would report a stop the agent has not
/// made. What it reaches waits in the ledger instead: a stop as the message the
/// round's gate fails on, a backoff as the armed latch. The round that follows
/// reads both at its gate, so one failure costs one request and one history
/// line.
///
/// A pre-loop pass that returned at all is not a failure (compacted, nothing to
/// compact, or declined on the summarizer budget), so it ends whatever stretch
/// the ledger was carrying.
pub(crate) async fn compact_pre_loop(ctx: &mut AgentContext) {
    match compact_if_needed(ctx).await {
        Ok(_) => {
            ctx.compaction.clear(timefmt::now_epoch());
        }
        Err(error) => {
            warn!("pre-loop compaction failed: {error:#}");
            // The pre-loop precedes the first round, so its records carry round 0.
            if let CompactionVerdict::Stop { message } =
                note_compaction_failure(ctx, &error, 0).await
            {
                // A stop cannot be taken here (this runs outside a round), so it
                // is parked for the round's gate, which fails on it before
                // sending. New work restarts the count and leaves the verdict
                // where it is (`restart`).
                ctx.compaction.record_stop(message);
            }
        }
    }
}

/// The compaction attempt ledger: one failing stretch of summarizer calls.
///
/// A stretch starts at its first failure — the clock the policy's deadline is
/// measured against — and its accounting ends when compaction succeeds (in the
/// gate or at the pre-loop wrapper), the store is back within the budget, or the
/// chain advances to a new endpoint. Its budget is the retry policy's own:
/// `max_sends` sends and `retry_timeout` of wall clock, so an operator tuning
/// retries tunes this too.
///
/// A stop met outside a round is parked here rather than acted on: the round's
/// gate takes it and fails, so the verdict the wrapper reached is the one an
/// operator reads.
///
/// The accounting and what a verdict leaves behind have separate lifetimes. New
/// work restarts the count (`restart`); an armed backoff and a stop waiting for
/// a gate stay with it, because both answer for the context a round would send,
/// not for the count. They go where they no longer answer for anything: the gate
/// taking the stop, a summary landing, the store back within budget, the chain
/// advancing (`clear`).
///
/// The shape memory an episode carries belongs to the count, not to what a
/// verdict left behind: `restart` rebuilds the episode, so a shape that spent
/// its slot reads as unused to the work that follows. Unlike the latch and the
/// parked stop, that memory does not cross a restart.
#[derive(Debug)]
pub struct CompactionLedger {
    episode: Episode,
    /// The moment (epoch seconds) before which no further attempt is made.
    ///
    /// It outlives `restart`: a backoff armed outside a round still holds when
    /// new work arrives, so that work's first gate does not ask the summarizer
    /// again inside the window.
    latch_until: Option<u64>,
    /// A stop the pre-loop wrapper met outside a round, waiting for the round's
    /// gate to take it.
    ///
    /// It outlives `restart` too: new work gets a fresh count, not a fresh
    /// verdict.
    pending_stop: Option<String>,
}

impl CompactionLedger {
    /// A fresh ledger: nothing charged, nothing latched.
    pub fn new(now: u64) -> Self {
        Self {
            episode: Episode::new(now),
            latch_until: None,
            pending_stop: None,
        }
    }

    /// Whether the backoff the last failure armed is still running.
    pub(crate) fn is_latched(&self, now: u64) -> bool {
        matches!(self.latch_until, Some(until) if now < until)
    }

    /// The sends charged to this stretch.
    pub(crate) fn attempts(&self) -> u32 {
        self.episode.attempts()
    }

    /// End the stretch: the count, the latch and a parked stop all go; `restart` is
    /// the new-work reset that keeps the latch and the stop.
    pub(crate) fn clear(&mut self, now: u64) {
        *self = Self::new(now);
    }

    /// Restart the count for new work. What the last verdict left behind — the
    /// armed latch and a parked stop — stays: both answer for the context a round
    /// would send, so the round this work starts reads them at its gate.
    pub(crate) fn restart(&mut self, now: u64) {
        let latch_until = self.latch_until;
        let pending_stop = self.pending_stop.take();
        *self = Self::new(now);
        self.latch_until = latch_until;
        self.pending_stop = pending_stop;
    }

    /// Charge one send and hand back the failed attempt the verdict reads.
    ///
    /// The failure that opens the stretch starts its clock: the deadline is
    /// measured from the first failure, not from the moment the ledger was
    /// built, because an agent can carry a fresh ledger across many rounds
    /// below the compaction threshold.
    fn charge_failure(
        &mut self,
        class: FailureClass,
        shape: Option<ShapeKey>,
        now: u64,
        jitter: Duration,
    ) -> Attempt {
        if self.episode.attempts() == 0 {
            self.episode = Episode::new(now);
        }
        self.episode = self.episode.after_attempt();
        Attempt::failed(Phase::Connect, class, shape, now, jitter)
    }

    /// Spend the stretch's one shape slot for a retry this path performs, so a
    /// repeat of the same shape reads as the endpoint's business.
    fn spend_shape_slot(&mut self, error: &BackendError) {
        crate::retry::spend_shape_slot(&mut self.episode, error);
    }

    /// Arm a wait: no attempt before `until`.
    fn arm(&mut self, until: u64) {
        self.latch_until = Some(until);
    }

    /// Park a stop met outside a round: the round's gate takes it and fails.
    fn record_stop(&mut self, message: String) {
        self.pending_stop = Some(message);
    }

    /// Take the parked stop, if any: a stop is acted on once.
    pub(crate) fn take_pending_stop(&mut self) -> Option<String> {
        self.pending_stop.take()
    }

    /// Test hook: move the latch deadline to `now`, which un-latches the ledger
    /// without sleeping (a pin cannot advance a clock).
    #[cfg(test)]
    pub(crate) fn expire_latch(&mut self, now: u64) {
        self.latch_until = Some(now);
    }

    /// Test hook: the armed deadline, so a pin asserts what a verdict armed
    /// without re-deriving the jitter.
    #[cfg(test)]
    pub(crate) fn latch_until(&self) -> Option<u64> {
        self.latch_until
    }
}

/// What one failed compaction attempt decided.
#[derive(Debug)]
pub(crate) enum CompactionVerdict {
    /// Transient: the latch is armed and the round proceeds; the next attempt
    /// waits out the policy's backoff.
    Retry { after: Duration },
    /// A class the failover chain owns: the observation is written, the round
    /// proceeds, and the main send handles the endpoint.
    Defer { reason: &'static str },
    /// No send can help, or the stretch is spent: the round ends with this
    /// operator-facing message.
    Stop { message: String },
}

/// The deadline a wait arms: whole seconds, rounded up and never below one, so
/// a sub-second backoff still holds the gate off for a moment instead of
/// arming the instant it failed.
fn latch_deadline(now: u64, wait: Duration) -> u64 {
    let secs = wait
        .as_secs()
        .saturating_add(u64::from(wait.subsec_nanos() > 0));
    now.saturating_add(secs.max(1))
}

/// Record one failed compaction attempt and return its verdict.
///
/// The single recorder behind all three call sites (the pre-call gate, the
/// pre-loop wrapper, the failover-advance arm), so a compaction failure is
/// classified, charged and decided the same way wherever it is met: the class
/// from the shared table ([`decision::classify`], connect phase) and the verdict
/// from the shared verdict function ([`decision::decide`]) over this ledger's
/// facts — the pair the retry shell consults.
/// The records it writes carry the retry shell's own shape ([`RetryRecord::retry`]
/// and [`RetryRecord::observed`]), so an operator reads one log.
pub(crate) async fn note_compaction_failure(
    ctx: &mut AgentContext,
    error: &anyhow::Error,
    round: usize,
) -> CompactionVerdict {
    let now = timefmt::now_epoch();
    let policy = ctx.config.retry_policy.clone();
    let backend = error
        .chain()
        .find_map(|node| node.downcast_ref::<BackendError>());
    let class = backend
        .map(|backend| decision::classify(backend, Phase::Connect))
        .unwrap_or(FailureClass::Unclassified);
    let attempt = ctx.compaction.charge_failure(
        class,
        backend.and_then(decision::shape_key),
        now,
        crate::retry::jitter_now(),
    );
    let verdict = decision::decide(&ctx.compaction.episode.facts(&attempt), &policy);

    match verdict {
        Decision::Retry { after } => {
            let Some(kind) = decision::retry_kind(&class) else {
                // A retryable class always maps to a record kind; recording the
                // attempt under a made-up one would be worse than stopping.
                return stop_compaction(
                    ctx,
                    class,
                    error,
                    backend,
                    "the failure class carries no retry kind",
                )
                .await;
            };
            let record = RetryRecord::retry(
                RetryAttempt {
                    round,
                    attempt: ctx.compaction.attempts(),
                    max_attempts: ctx.config.retry_policy.max_retries,
                },
                now,
                Some(ctx.failover.current_profile().endpoint.clone()),
                kind,
                format!("{error:#}"),
                after.as_secs_f64(),
            );
            ctx.store
                .lock()
                .await
                .record_retries([record], crate::acquisition::retry_keep_since(ctx));
            ctx.persist().await;
            // A shape-class retry spends the stretch's one shape slot for that
            // key, so a repeat of the same shape reads as the endpoint's
            // business; only a retry this path performs spends the slot.
            if let Some(backend) = backend {
                ctx.compaction.spend_shape_slot(backend);
            }
            ctx.compaction.arm(latch_deadline(now, after));
            CompactionVerdict::Retry { after }
        }
        Decision::AdvanceFailover { reason } => {
            if let Some(backend) = backend {
                crate::retry::record_fact(
                    ctx.profile_pacing.as_deref(),
                    &ctx.failover.current_profile().id,
                    class,
                    backend,
                    now,
                );
            }
            // The main path records the class whether the chain then advances or
            // runs out; only the classes that reach an advance carry a kind.
            if let Some(kind) = decision::retry_kind(&class) {
                let record = RetryRecord::observed(
                    RetryAttempt {
                        round,
                        attempt: ctx.compaction.attempts(),
                        max_attempts: ctx.config.retry_policy.max_retries,
                    },
                    now,
                    Some(ctx.failover.current_profile().endpoint.clone()),
                    kind,
                    format!("{error:#}"),
                    None,
                );
                ctx.store
                    .lock()
                    .await
                    .record_retries([record], crate::acquisition::retry_keep_since(ctx));
                ctx.persist().await;
            }
            // The endpoint is the main send's business now; the latch holds the
            // gate off for one policy backoff — a moment, not the send's own
            // outcome.
            let wait = crate::retry::backoff_delay(&policy, 0);
            ctx.compaction.arm(latch_deadline(now, wait));
            CompactionVerdict::Defer { reason }
        }
        Decision::Stop { reason, .. } | Decision::Fatal { reason } => {
            stop_compaction(ctx, class, error, backend, reason).await
        }
        Decision::Proceed => {
            // A settled failure always yields a verdict about it: `Proceed` is the
            // pre-send reading, and this attempt already failed.
            stop_compaction(
                ctx,
                class,
                error,
                backend,
                "the verdict for a failed attempt was to proceed",
            )
            .await
        }
    }
}

/// End the stretch: write the durable history line and build the message the
/// round ends with.
///
/// The message carries the class's own label, the attempt count against the
/// policy's cap, and how far over budget the context sits — the numbers the
/// round could not honor, which is what the operator acts on. The history line
/// is the agent's own append-only account of what ended the stretch: the park
/// reason is in-memory current state, and the `Error` event is a notification to
/// a superior the agent may not have.
async fn stop_compaction(
    ctx: &mut AgentContext,
    class: FailureClass,
    error: &anyhow::Error,
    backend: Option<&BackendError>,
    reason: &str,
) -> CompactionVerdict {
    let attempt = ctx.compaction.attempts();
    let max_attempts = max_sends(&ctx.config.retry_policy);
    let detail = match backend {
        Some(backend) => crate::llm_error::render_error(backend),
        None => format!("{error:#}"),
    };
    let label = class.label();
    let consumed = ctx.store.lock().await.total_estimated_tokens();
    let budget = ctx.config.effective_budget();
    ctx.append_history(
        None,
        &[],
        0,
        RecordKind::System,
        Some(SystemEvent::CompactionFailure {
            attempt,
            max_attempts,
            class: label.to_string(),
            detail,
        }),
        &[],
    );
    CompactionVerdict::Stop {
        message: format!(
            "compaction failed: {reason} (class: {label}; attempt {attempt} of {max_attempts}; \
             context {consumed} tokens over the {budget}-token budget)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::CONTEXT_SUMMARY_HEADER;
    use crate::history::{HistoryRecord, HistoryWriter};
    use crate::retry::RetryPolicy;
    use crate::retry::decision::TransientKind;
    use crate::test_support::{
        FAILURE_PIN_WINDOW, MapSource, ctx_from_source, ctx_with_mock, mount_summary, profile,
        seed_over_budget_store, set_context_window, tool_result_msg, user_msg,
    };
    use just_llm_client::LlmBackend;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::MockServer;

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

    /// A failed summarizer call surfaces as an error and leaves the store exactly as it
    /// was: no summary pinned, no turn evicted. Dressing the failure up as
    /// `NothingToCompact` would hide a lost compaction behind a benign verdict.
    #[tokio::test]
    async fn a_failed_summary_call_leaves_the_context_untouched() {
        let (mut ctx, _server) =
            ctx_with_mock(vec![profile("p1", "ep1", 500_000)], &["ep1"], 400).await;
        set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
        let (turns_before, tokens_before) = seed_over_budget_store(&mut ctx).await;

        let result = summarize_and_evict(&ctx).await;

        assert!(
            result.is_err(),
            "a 400 from the summarizer must surface as an error"
        );
        let store = ctx.store.lock().await;
        assert!(
            !store.pinned_labels().iter().any(|l| l == "context_summary"),
            "a failed call must not pin a summary"
        );
        assert_eq!(
            store.turns().iter().filter(|t| !t.is_pinned()).count(),
            turns_before,
            "a failed call must not evict turns"
        );
        assert_eq!(store.total_estimated_tokens(), tokens_before);
    }

    /// A failed summarizer call surfaces as an error from `compact_if_needed`
    /// instead of being swallowed into a `false` that reads as "the context
    /// already fits": the wrapper and the gate both need the error to record it.
    #[tokio::test]
    async fn a_failed_summary_call_surfaces_from_compact_if_needed() {
        let (mut ctx, _server) =
            ctx_with_mock(vec![profile("p1", "ep1", 500_000)], &["ep1"], 400).await;
        set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
        seed_over_budget_store(&mut ctx).await;

        let result = compact_if_needed(&ctx).await;

        assert!(
            result.is_err(),
            "the helper must report the failure, not answer `false`"
        );
    }

    /// A stopped stretch leaves a durable line in the agent's own history: the
    /// park reason is in-memory current state and the `Error` event is a
    /// notification to a superior the agent may not have, so the append-only log
    /// is the account an operator reads after the fact.
    #[tokio::test]
    async fn a_stopped_compaction_writes_the_history_record() {
        let (mut ctx, _server) =
            ctx_with_mock(vec![profile("p1", "ep1", 500_000)], &["ep1"], 400).await;
        set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
        seed_over_budget_store(&mut ctx).await;
        let dir = tempfile::tempdir().unwrap();
        ctx.history = Some(HistoryWriter::new(dir.path().to_owned()));

        let Err(error) = summarize_and_evict(&ctx).await else {
            panic!("a 400 from the summarizer must surface as an error");
        };
        let verdict = note_compaction_failure(&mut ctx, &error, 3).await;

        assert!(
            matches!(verdict, CompactionVerdict::Stop { .. }),
            "a 400 is the class table's terminal class"
        );
        let file = std::fs::read_dir(dir.path().join("history"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let line = std::fs::read_to_string(file).unwrap();
        let record: HistoryRecord = serde_json::from_str(line.trim_end()).unwrap();

        assert_eq!(record.kind, RecordKind::System);
        let Some(SystemEvent::CompactionFailure {
            attempt,
            max_attempts,
            class,
            detail,
        }) = record.event
        else {
            panic!("a stopped compaction must write its own history event");
        };
        assert_eq!(attempt, 1, "the stretch spent one send");
        assert_eq!(
            max_attempts,
            max_sends(&ctx.config.retry_policy),
            "the cap is the policy's send budget"
        );
        assert_eq!(class, "the request itself was rejected");
        assert!(
            !detail.is_empty(),
            "the rendered backend detail rides the line"
        );
    }
    /// A shape-class failure spends the stretch's one shape slot: the repeat of
    /// the same shape is the endpoint's business, so the recorder defers it to
    /// the main send instead of asking the summarizer for that shape again.
    #[tokio::test]
    async fn a_repeated_summarizer_shape_yields_to_the_main_send() {
        let (mut ctx, _server) =
            ctx_with_mock(vec![profile("p1", "ep1", 500_000)], &["ep1"], 500).await;
        set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
        seed_over_budget_store(&mut ctx).await;
        let error = anyhow::Error::new(BackendError::provider(
            "probe",
            just_llm_client::TransportError::InvalidResponse("a body of the wrong shape".into()),
        ));

        let first = note_compaction_failure(&mut ctx, &error, 0).await;
        let repeat = note_compaction_failure(&mut ctx, &error, 0).await;

        assert!(
            matches!(first, CompactionVerdict::Retry { .. }),
            "the first shape failure is retryable"
        );
        assert!(
            matches!(repeat, CompactionVerdict::Defer { .. }),
            "the repeat of the same shape defers to the main send"
        );
        assert_eq!(ctx.compaction.attempts(), 2, "both sends were charged");
    }

    /// The parked stop is taken once: the gate fails on it, and a later gate call
    /// reads the context afresh instead of replaying the same verdict.
    #[tokio::test]
    async fn a_parked_stop_is_taken_once() {
        let (mut ctx, _server) =
            ctx_with_mock(vec![profile("p1", "ep1", 500_000)], &["ep1"], 400).await;
        set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
        let (_, prompt_tokens) = seed_over_budget_store(&mut ctx).await;
        ctx.config.context_thresholds = vec![80];

        compact_pre_loop(&mut ctx).await;
        assert_eq!(ctx.compaction.attempts(), 1, "the wrapper charged the send");

        let cancel = tokio_util::sync::CancellationToken::new();
        let first =
            crate::budget_gate::enforce_pre_call_budget(&mut ctx, prompt_tokens, 0, &cancel).await;
        assert!(
            matches!(first, crate::budget_gate::BudgetAction::Fail(_)),
            "the parked stop ends the round"
        );
        assert_eq!(
            ctx.compaction.attempts(),
            1,
            "the parked stop is taken without a summarizer send"
        );

        let second =
            crate::budget_gate::enforce_pre_call_budget(&mut ctx, prompt_tokens, 1, &cancel).await;
        assert!(
            matches!(second, crate::budget_gate::BudgetAction::Fail(_)),
            "the context still does not fit: the gate reads it afresh"
        );
        assert_eq!(
            ctx.compaction.attempts(),
            2,
            "the parked stop is taken once, not replayed"
        );
    }

    /// A landing pre-loop compaction clears the stretch: the round that follows
    /// does not inherit the earlier failure's latch or its spent budget.
    #[tokio::test]
    async fn a_pre_loop_success_clears_the_stretch() {
        let (mut ctx, _server) = summary_ctx().await;
        set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
        seed_over_budget_store(&mut ctx).await;
        ctx.config.context_thresholds = vec![80];
        // A stretch an earlier round left open: one send charged, the backoff
        // armed well past this call.
        ctx.compaction.charge_failure(
            FailureClass::Transient(TransientKind::Server),
            None,
            timefmt::now_epoch(),
            Duration::ZERO,
        );
        ctx.compaction.arm(timefmt::now_epoch() + 600);

        compact_pre_loop(&mut ctx).await;

        assert_eq!(
            ctx.compaction.attempts(),
            0,
            "the landing summary ends the stretch"
        );
        assert!(
            !ctx.compaction.is_latched(timefmt::now_epoch()),
            "the old latch does not survive a landing summary"
        );
    }
    /// Both compaction sites write the same user-role record: the pin, and the
    /// history line that mirrors it.
    #[tokio::test]
    async fn compaction_pins_the_summary_as_a_user_record() {
        let (mut ctx, _server) = summary_ctx().await;
        let history_dir = tempfile::TempDir::new().unwrap();
        ctx.history = Some(crate::history::HistoryWriter::new(
            history_dir.path().to_owned(),
        ));
        {
            let mut s = ctx.store.lock().await;
            s.push_turn(vec![tool_result_msg(oversized_content(), "w0")]);
        }

        let outcome = summarize_and_evict(&ctx).await.unwrap();
        assert!(
            matches!(outcome, CompactOutcome::Compacted),
            "precondition: the window must compact"
        );

        let s = ctx.store.lock().await;
        let summary = s
            .turns()
            .iter()
            .find(|t| t.is_pinned() && t.label() == Some(CONTEXT_SUMMARY_LABEL))
            .expect("compaction pins a summary");
        let message = &summary.messages[0];
        assert_eq!(message.role(), "user");
        assert!(
            message
                .content()
                .unwrap_or_default()
                .starts_with(CONTEXT_SUMMARY_HEADER)
        );
        let line = std::fs::read_to_string(
            std::fs::read_dir(history_dir.path().join("history"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        let record: crate::history::HistoryRecord = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            record.messages[0].content(),
            message.content(),
            "the history line records the same injected form as the pin"
        );
    }

    /// The carried summary reaches the compactor as our own user-role record,
    /// fronted exactly once however many rounds it has already survived.
    #[tokio::test]
    async fn the_compactor_reads_the_carried_summary_as_one_user_record() {
        let (ctx, server) = summary_ctx().await;
        {
            let mut s = ctx.store.lock().await;
            // A stored summary that already carries its header: the round trip
            // must not front it a second time.
            let stored = summary_record("earlier work");
            let carried = summary_record(stored.content().unwrap_or_default());
            s.pin(CONTEXT_SUMMARY_LABEL, carried).unwrap();
            s.push_turn(vec![tool_result_msg(oversized_content(), "w0")]);
        }

        let outcome = summarize_and_evict(&ctx).await.unwrap();
        assert!(
            matches!(outcome, CompactOutcome::Compacted),
            "precondition: the window must compact"
        );

        let requests = server.received_requests().await.unwrap_or_default();
        assert_eq!(requests.len(), 1, "one compaction pass, one request");
        let body = String::from_utf8_lossy(&requests[0].body);
        assert_eq!(
            body.matches("earlier turns condensed").count(),
            1,
            "the carried summary is fronted exactly once: {body}"
        );
        let parsed: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let carried = parsed["messages"]
            .as_array()
            .expect("the request carries messages")
            .iter()
            .find(|m| {
                m["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("earlier work")
            })
            .expect("the carried summary reaches the compactor");
        assert_eq!(
            carried["role"], "user",
            "the carried summary is injected context, not a model turn"
        );
    }
}
