//! Agent task orchestration: shared context, round execution, prompt processing.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, AgentOutcome};
use crate::lifecycle::LifecycleState;
use crate::tools::DEFAULT_BREAK_TIMEOUT_SECS;
use kallip_common::protocol::{ParkedReason, TransientRetryInfo};

use crate::approval::ApprovalStore;
use crate::config::AgentConfig;
use crate::context::{ContextStore, ContextSummarizer, TurnId};
use crate::history::{HistoryWriter, RecordKind};
use crate::policy::AuthorizedToolExecutor;
use crate::runner;
use just_llm_client::types::chat::ChatMessage;

/// A cancellation token scoped to a single round, always a child of the agent's lifecycle
/// token ([`AgentContext::cancel`]). Cancelled by `interrupt_agent` to abort the current
/// round without terminating the task.
///
/// Because it is a child, a lifecycle cancel (remove / tagma shutdown) propagates to it —
/// so **lifecycle-cancelled ⟹ round-cancelled**. The converse (round cancelled but
/// lifecycle not) is exactly what distinguishes an interrupt from a lifecycle cancel. This
/// holds iff the token is always minted via [`RoundToken::new`] from the lifecycle token;
/// the newtype makes that invariant structural rather than conventional.
#[derive(Clone)]
pub struct RoundToken(CancellationToken);

impl RoundToken {
    /// Mint a round token as a child of the agent lifecycle token.
    pub fn new(lifecycle: &CancellationToken) -> Self {
        Self(lifecycle.child_token())
    }

    /// Cancel this round. Called by `interrupt_agent`.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// The underlying token, for the runner (and its callees `consume_stream` /
    /// `stream_with_retry`) to select on, and for any caller to inspect
    /// (`handle().is_cancelled()`). The runner legitimately needs the raw
    /// `CancellationToken` for its `select!` arms, so this is the intended seam —
    /// not an encapsulation leak.
    pub fn handle(&self) -> &CancellationToken {
        &self.0
    }
}

/// How to treat a round-token cancel, classified by whether the lifecycle (parent)
/// token is also cancelled. Correct iff the round token is always a child of the
/// lifecycle token — see [`RoundToken`].
#[derive(Debug, PartialEq, Eq)]
enum CancelKind {
    /// The lifecycle token was cancelled (remove / shutdown) → terminate the task.
    Lifecycle,
    /// Only the round token was cancelled (interrupt) → keep the task alive.
    Interrupt,
}

impl CancelKind {
    /// Classify a round-token cancel by inspecting the lifecycle token.
    ///
    /// A lifecycle cancel propagates to its children, so if the lifecycle token is
    /// cancelled the round-token cancel was a consequence of it (terminate);
    /// otherwise the round token alone fired (interrupt).
    fn classify(lifecycle: &CancellationToken) -> Self {
        if lifecycle.is_cancelled() {
            Self::Lifecycle
        } else {
            Self::Interrupt
        }
    }
}

/// Shared agent resources passed between modes.

/// Trait for pulling undelivered inbox messages. Implemented by the tagma
/// (where InboxStore lives) and injected into AgentContext at spawn time.
/// `None` for test contexts (no inbox).
#[async_trait::async_trait]
pub trait MessagePuller: Send + Sync + 'static {
    /// Atomically mark all undelivered direct messages as delivered and return
    /// them as a formatted string. Returns `None` when no undelivered direct
    /// messages exist.
    async fn pull_undelivered(&self) -> Option<String>;
}

pub struct AgentContext {
    pub client: crate::profile::ChatClient,
    /// Within-tier failover state: the resolved capability tier, the profile registry (for
    /// rebuilding the client on advance), the system prompt, and the sticky `profile_idx` (the
    /// sole writer of which is `FailoverState::advance_to`). See `FailoverState`.
    pub failover: crate::failover::FailoverState,
    pub store: Arc<Mutex<ContextStore>>,
    pub approvals: Arc<Mutex<ApprovalStore>>,
    pub executor: AuthorizedToolExecutor,
    pub summarizer: ContextSummarizer,
    pub config: AgentConfig,
    /// Agent directory for persistence.
    pub agent_dir: Option<PathBuf>,
    /// Append-only conversation history writer. `Some` when `agent_dir` is `Some`.
    pub history: Option<HistoryWriter>,
    /// Cancellation signal for graceful interruption.
    pub cancel: CancellationToken,
    /// The current round's cancellation token, reachable by `interrupt_agent`. `Some` only
    /// while a round is running. See [`RoundToken`].
    pub round_cancel: Arc<std::sync::Mutex<Option<RoundToken>>>,
    /// Wake signal triggered by external events (e.g. approval notifications).
    /// The agent task awaits this in the outer loop; callers signal via `notify_one()`.
    pub notify: Arc<Notify>,
    /// Wake signal for the timed transient-retry path. A separate [`Notify`] (not
    /// `notify`) so the approval arm's `has_notifications()` guard stays the single
    /// authority for approval wakes — see the outer-loop comment. Driven by a
    /// best-effort spawned sleep task armed by `schedule_transient_retry`.
    pub retry_notify: Arc<Notify>,
    /// The armed transient-retry deadline, or `None` when no retry is pending. The
    /// retry select arm is authoritative: it re-enters only when this is `Some(t)`
    /// with `t <= now`, then clears it. Cleared on every non-retry wake so a stale
    /// stored permit (a sleep that fired after a different arm won the race) cannot
    /// trigger a spurious round.
    pub retry_at: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
    /// Consecutive transient (failover-chain-exhausted) parks. Survives outer-loop
    /// wakes so a retry sequence accumulates; reset on any non-transient round
    /// outcome. Hard-parks (surfaces to the operator) once it exceeds
    /// `config.max_transient_retries`.
    pub transient_fails: u32,
    /// Authoritative lifecycle state (the design's C4 shape: the outer loop keeps
    /// a single `select!` whose per-state arm differences are guards; this enum
    /// owns the state itself, replacing the scattered wait/park Option-fields).
    /// Written only via `LifecycleState::transition`, which asserts the
    /// legal-edge table in debug builds. `Running` is held only inside
    /// `run_and_report`.
    pub lifecycle: std::sync::Mutex<LifecycleState>,
    /// The armed wait-timer deadline (`break(wait)` or a budget-blocked
    /// re-arm), or `None`. Mirrors `retry_at`'s authority pattern: the wait
    /// select arm honors a stored permit only when this is `Some(t)` with
    /// `t <= now`, then clears it; every other wake clears it too, so a
    /// stale stored permit cannot fire a spurious turn.
    pub wait_until: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
    /// Wake signal for the wait timer. Separate from `notify` for the same
    /// reason as `retry_notify` (each signal's guard stays the sole
    /// authority for its wake). Driven by a best-effort, cancel-aware
    /// spawned sleep.
    pub wait_notify: Arc<Notify>,
    /// The `timeout_secs` the wait timer was last armed with (for the
    /// elapsed-turn text). Meaningful only while `wait_until` is armed.
    pub wait_armed_secs: u64,
    /// Tagma-wide token budget shared by all agents.
    /// Cloned from `AppState` — same underlying Arc counters across all agents.
    pub token_budget: crate::token_budget::TokenBudget,
    /// Pending profile-reset cell: the tagma's apply handler writes a
    /// [`ProfileReset`] here; the agent task drains it at the top of
    /// [`run_and_report`] and rebuilds its failover state + client. Shared
    /// (same `Arc`) with the tagma `Agent` struct so the apply route can write
    /// to it without reaching into runtime internals. `None` when no reset is
    /// pending.
    pub pending_profile_reset: Arc<std::sync::Mutex<Option<crate::failover::ProfileReset>>>,
    /// Inbox message puller. Injected by the tagma so the runtime can pull
    /// undelivered direct messages on a notify wake without depending on the
    /// tagma crate. `None` for test contexts and agents without an inbox.
    pub message_puller: Option<Arc<dyn MessagePuller>>,
}

impl AgentContext {
    /// Persist context and approval state to disk. Logs warnings on failure.
    pub async fn persist(&self) {
        let Some(ref dir) = self.agent_dir else {
            return;
        };

        {
            let guard = self.store.lock().await;
            if let Ok(json) = serde_json::to_string(&*guard)
                && let Err(e) = crate::persistence::persist_context(&json, dir)
            {
                tracing::error!("context persist failed: {e:#}");
            }
        }
        {
            let guard = self.approvals.lock().await;
            if let Ok(json) = serde_json::to_string(&*guard)
                && let Err(e) = crate::persistence::persist_approvals(&json, dir)
            {
                tracing::error!("approval persist failed: {e:#}");
            }
        }
    }

    /// Fire-and-forget append to history. Logs a warning on failure.
    pub(crate) fn append_history(
        &self,
        turn_id: Option<u64>,
        messages: &[ChatMessage],
        estimated_tokens: usize,
        kind: RecordKind,
        event: Option<crate::history::SystemEvent>,
    ) {
        if let Some(ref history) = self.history
            && let Err(e) = history.append(turn_id, messages, estimated_tokens, kind, event)
        {
            tracing::warn!(turn_id = ?turn_id, "history write failed: {e:#}");
        }
    }

    /// Record a turn to both the context store and the append-only history log.
    /// Returns the assigned `TurnId`.
    pub async fn record_turn(&self, messages: Vec<ChatMessage>) -> TurnId {
        let (turn_id, estimated_tokens) = {
            let mut guard = self.store.lock().await;
            guard.push_turn(messages.clone())
        };
        self.append_history(
            Some(turn_id.0),
            &messages,
            estimated_tokens,
            RecordKind::Turn,
            None,
        );
        turn_id
    }
}
pub async fn agent_task(
    mut ctx: AgentContext,
    initial_prompt: Option<String>,
    mut prompt_rx: tokio::sync::mpsc::Receiver<String>,
    agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
) {
    // Pre-loop compaction: handle context overflow from restored agents.
    if let Err(e) = crate::context::compact_if_needed(&ctx).await {
        tracing::warn!("pre-loop compaction failed: {e:#}");
    }

    if let Some(p) = initial_prompt {
        if p.is_empty() {
            return;
        }
        ctx.record_turn(vec![ChatMessage::user(&p)]).await;
        if run_and_report(&mut ctx, &agent_tx, &mut prompt_rx).await {
            return;
        }
    }

    loop {
        tokio::select! {
            input = prompt_rx.recv() => {
                match input {
                    Some(text) => {
                        clear_transient_retry(&ctx);
                        clear_wait_timer(&ctx);
                        ctx.record_turn(vec![ChatMessage::user(&text)]).await;
                        if run_and_report(&mut ctx, &agent_tx, &mut prompt_rx).await {
                            break;
                        }
                    }
                    None => break,
                }
            }
            // Lifecycle cancel (remove / tagma shutdown): terminate the task.
            // Per-agent interrupt never reaches here — it cancels only the current
            // round token inside `run_and_report`, not the lifecycle token.
            _ = ctx.cancel.cancelled() => {
                tracing::info!("agent task: lifecycle cancel, persisting and exiting");
                terminate_cancelled(&ctx, &agent_tx).await;
                break;
            }
            _ = ctx.notify.notified() => {
                // Three producers share this Notify: inbox message delivery,
                // approval notifications, and the profile-apply handler (which
                // writes a pending-reset cell then calls notify_one). Because
                // Notify coalesces permits (at most 1 stored), a single wake may
                // carry work from multiple producers — evaluate ALL sequentially.
                let has_reset = ctx
                    .pending_profile_reset
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
                if has_reset {
                    apply_pending_profile_reset(&mut ctx);
                }

                let mut should_run = false;

                // Pull undelivered inbox messages (after reset so the round
                // uses the fresh client). Drains ALL undelivered in one atomic
                // call to survive notify coalescing.
                if let Some(ref puller) = ctx.message_puller {
                    if let Some(msg) = puller.pull_undelivered().await {
                        ctx.record_turn(vec![ChatMessage::user(&msg)]).await;
                        should_run = true;
                    }
                }

                // Approval notifications.
                if ctx.approvals.lock().await.has_notifications() {
                    should_run = true;
                }

                if should_run {
                    clear_transient_retry(&ctx);
                    clear_wait_timer(&ctx);
                    if run_and_report(&mut ctx, &agent_tx, &mut prompt_rx).await {
                        break;
                    }
                }
            }
            // Timed transient retry: the spawned sleep from `schedule_transient_retry`
            // fired. The guard is authoritative — a stored permit is only honored when
            // the armed deadline has actually passed, so a wake that raced a hard-park
            // or a different arm is a no-op.
            _ = ctx.retry_notify.notified() => {
                if transient_retry_due(&ctx) {
                    clear_transient_retry(&ctx);
                    if run_and_report(&mut ctx, &agent_tx, &mut prompt_rx).await {
                        break;
                    }
                }
            }
            // Wait timer: the fuse from `break(wait)` (or a budget-blocked
            // re-arm) elapsed. Guard-authoritative, mirroring the retry arm.
            // Unlike Retrying's automatic re-run, the wake is an injected
            // [system] turn — the agent decides what to do next.
            _ = ctx.wait_notify.notified() => {
                if wait_timer_due(&ctx) {
                    let armed_secs = ctx.wait_armed_secs;
                    clear_wait_timer(&ctx);
                    // A budget-probe wake (the fuse the budget gate itself
                    // re-armed) skips the injection: the round gate still
                    // blocks, so the [system] turn could never reach a
                    // model — it would only accumulate context noise on
                    // every probe cycle. Real wait wakes inject as usual.
                    if !ctx.token_budget.is_exceeded() {
                        ctx.record_turn(vec![ChatMessage::user(&wait_elapsed_text(armed_secs))])
                            .await;
                    }
                    if run_and_report(&mut ctx, &agent_tx, &mut prompt_rx).await {
                        break;
                    }
                }
            }
        }
    }
}

/// Persist context + approval state and emit the terminal [`AgentEvent::Cancelled`].
///
/// The single exit path for a lifecycle cancel (remove / tagma shutdown), shared by the
/// outer-loop cancel arm (idle) and `run_and_report`'s mid-round lifecycle-cancel branch.
async fn terminate_cancelled(ctx: &AgentContext, agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>) {
    ctx.persist().await;
    agent_tx.send(AgentEvent::Cancelled).await.ok();
}

/// Drain the pending profile-reset cell (if set by the tagma's apply handler)
/// and rebuild the failover state + client. Called at two sites: once at the
/// top of each `run_and_report` wake-up, and once in the notify arm when a
/// pending reset is detected. On error, logs and continues on prior config —
/// the cell is cleared regardless so a bad reset does not retry forever.
fn apply_pending_profile_reset(ctx: &mut AgentContext) {
    let reset = ctx
        .pending_profile_reset
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(reset) = reset else { return };
    let new_window = reset.tier.active_profile().max_context_window;
    match ctx.failover.reset_and_rebuild(reset.tier, reset.registry) {
        Ok(new_client) => {
            ctx.client = new_client;
            if let Err(e) = ctx.config.set_context_window(new_window) {
                tracing::warn!(
                    window = new_window,
                    "profile reset: failed to re-apply context window, keeping prior: {e:#}"
                );
            }
            tracing::info!("agent profile reset applied");
        }
        Err(e) => {
            tracing::error!("profile reset failed, continuing on prior config: {e:#}");
        }
    }
}

/// Run agent rounds for one external wake and send results via channel.
///
/// Owns the heartbeat loop: a bare-assistant round (no `break`, no tool calls) no
/// longer terminates the run — the harness records the assistant turn, injects a
/// heartbeat prompt, and re-enters the round loop. The no-progress guardrail
/// (`config.max_heartbeat_rounds`) force-idles after a bounded storm. Only `break`
/// (or a non-deliberate park reason) returns.
///
/// Owns the round-token lifecycle per iteration: mints a fresh child of the
/// lifecycle token, publishes it into `ctx.round_cancel` (so `interrupt_agent` can
/// reach it), runs the round, then clears the slot. Returns `true` only on a
/// lifecycle cancel (the task should terminate); every other outcome returns
/// `false` so the outer loop continues and the task stays alive.
pub async fn run_and_report(
    ctx: &mut AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
) -> bool {
    // Drain any pending profile reset before starting the round loop.
    apply_pending_profile_reset(ctx);
    // Every entry here is from a non-Running state by construction (the outer
    // loop parks between runs); the transition table asserts exactly that.
    ctx.lifecycle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .transition(LifecycleState::Running);
    let mut no_progress: u32 = 0;
    loop {
        let round = RoundToken::new(&ctx.cancel);
        // Publish the round token for the duration of this round so `interrupt_agent`
        // can cancel it. `Some` only while the round is in flight.
        *ctx.round_cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(round.clone());

        agent_tx.send(AgentEvent::Busy).await.ok();
        let result = runner::run_agent_rounds(ctx, agent_tx, prompt_rx, round.handle()).await;

        // Always clear the slot: a stale token cancelled by a later interrupt would be a
        // no-op (nobody selects on it), but clearing keeps the invariant tight.
        *ctx.round_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;

        match result {
            Ok(runner::RoundOutcome::Break(until)) => {
                // Deliberate yield: park as Idle (`until:"idle"`) or Waiting
                // (armed wake timer, the default). Any tool calls preceding
                // `break` were already recorded inside the round loop.
                ctx.transient_fails = 0;
                match until {
                    runner::BreakUntil::Idle => {
                        ctx.lifecycle
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .transition(LifecycleState::Idle);
                        agent_tx.send(AgentEvent::Idle).await.ok();
                    }
                    runner::BreakUntil::Wait { timeout_secs } => {
                        let deadline = arm_wait_timer(ctx, timeout_secs);
                        ctx.lifecycle
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .transition(LifecycleState::Waiting {
                                until: deadline.into_std(),
                            });
                        agent_tx
                            .send(AgentEvent::Waiting { timeout_secs })
                            .await
                            .ok();
                    }
                }
                return false;
            }
            Ok(runner::RoundOutcome::BareAssistant { content }) => {
                // No `break`, no tool calls — heartbeat and continue. Reset the
                // transient-retry counter (a real response is progress).
                ctx.transient_fails = 0;
                no_progress += 1;
                if no_progress > ctx.config.max_heartbeat_rounds {
                    // Self-monologue guardrail: force-idle instead of looping forever.
                    ctx.lifecycle
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .transition(LifecycleState::Idle);
                    agent_tx.send(AgentEvent::Idle).await.ok();
                    return false;
                }
                ctx.record_turn(vec![ChatMessage::assistant(&content)])
                    .await;
                ctx.record_turn(vec![ChatMessage::user(HEARTBEAT_TEXT)])
                    .await;
                continue;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::Cancelled)) => {
                ctx.transient_fails = 0;
                match CancelKind::classify(&ctx.cancel) {
                    // Lifecycle cancel propagated to the round token → terminate.
                    CancelKind::Lifecycle => {
                        tracing::info!(
                            "agent task: lifecycle cancel mid-round, persisting and exiting"
                        );
                        terminate_cancelled(ctx, agent_tx).await;
                        return true;
                    }
                    // Only the round token was cancelled (interrupt) → keep living.
                    CancelKind::Interrupt => {
                        agent_tx.send(AgentEvent::Interrupted).await.ok();
                        ctx.lifecycle
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .transition(LifecycleState::Idle);
                        return false;
                    }
                }
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::FailoverChainExhausted {
                reason,
                detail,
                ..
            })) => {
                // Transient: the whole chain is down. Schedule a timed retry
                // (bounded by `max_transient_retries`); the outer loop's retry
                // arm re-enters after the backoff. On the last attempt,
                // hard-park and surface so the operator can reconfigure
                // failover. The arming rides the terminal event's payload so
                // the bridge can mark RETRYING (not PARKED) without flicker.
                ctx.transient_fails = ctx.transient_fails.saturating_add(1);
                let armed = ctx.transient_fails <= ctx.config.max_transient_retries;
                let retry_deadline = armed.then(|| schedule_transient_retry(ctx));
                agent_tx
                    .send(AgentEvent::FailoverChainExhausted {
                        reason,
                        detail,
                        transient_retry: retry_deadline.map(|(_, delay_secs)| TransientRetryInfo {
                            attempt: ctx.transient_fails,
                            max_attempts: ctx.config.max_transient_retries,
                            retry_in_secs: delay_secs,
                        }),
                    })
                    .await
                    .ok();
                match retry_deadline {
                    Some((deadline, _)) => {
                        ctx.lifecycle
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .transition(LifecycleState::Retrying {
                                attempt: ctx.transient_fails,
                                max_attempts: ctx.config.max_transient_retries,
                                retry_at: deadline.into_std(),
                            });
                    }
                    None => {
                        ctx.lifecycle
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .transition(LifecycleState::Parked {
                                reason: ParkedReason::TransientRetryExhausted,
                                at: std::time::Instant::now(),
                            });
                    }
                }
                return false;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::TokenBudgetExceeded {
                consumed,
                budget,
            })) => {
                // Non-fatal: the task stays alive — and per the design's
                // budget-probe decision it stays *Waiting* with a re-armed
                // timer rather than parking: the next timer wake re-checks
                // the budget before any LLM call (the round gate runs
                // first), so the armed fuse doubles as a zero-cost
                // budget-recovery probe.
                ctx.transient_fails = 0;
                agent_tx
                    .send(AgentEvent::TokenBudgetExceeded { consumed, budget })
                    .await
                    .ok();
                let deadline = arm_wait_timer(ctx, DEFAULT_BREAK_TIMEOUT_SECS);
                ctx.lifecycle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .transition(LifecycleState::Waiting {
                        until: deadline.into_std(),
                    });
                return false;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::MaxRoundsExceeded)) => {
                ctx.transient_fails = 0;
                ctx.lifecycle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .transition(LifecycleState::Parked {
                        reason: ParkedReason::MaxRoundsExceeded,
                        at: std::time::Instant::now(),
                    });
                agent_tx.send(AgentEvent::MaxRoundsExceeded).await.ok();
                return false;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::Idle)) => {
                // Not produced by the round loop today (only Break/BareAssistant/Park
                // are); parked here for exhaustiveness and forward-compat.
                ctx.transient_fails = 0;
                ctx.lifecycle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .transition(LifecycleState::Idle);
                agent_tx.send(AgentEvent::Idle).await.ok();
                return false;
            }
            Err(e) => {
                // Permanent (Fatal) error — park and surface; the operator acts.
                ctx.transient_fails = 0;
                let rendered = crate::llm_error::render_error(e.as_ref());
                ctx.lifecycle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .transition(LifecycleState::Parked {
                        reason: ParkedReason::FatalError {
                            message: rendered.clone(),
                        },
                        at: std::time::Instant::now(),
                    });
                agent_tx
                    .send(AgentEvent::Error(rendered))
                    .await
                    .ok();
                return false;
            }
        }
    }
}

/// The synthetic user turn injected after a bare-assistant round, nudging the
/// agent to either make progress, reply, or call `break`. Persisted like an
/// approval notification (bounded by the no-progress guardrail).
const HEARTBEAT_TEXT: &str = "[system] You produced a response with no tool action and did not \
call `break`. If you are done, call `break` with `{\"until\":\"idle\"}`; if you are blocked \
waiting on something, call `break` (the default parks you with a wake timer). Otherwise \
continue your work.";

/// Arm a timed retry for the transient (failover-chain-exhausted) path: back off
/// and notify `retry_at` after the delay. Returns the armed `(deadline, delay)`
/// so the caller can ride the same numbers on the terminal event's
/// `transient_retry` payload and the lifecycle state. The spawned sleep is
/// best-effort and cancel-aware — the outer-loop guard is authoritative, so a
/// wake that fires after a hard-park or a different-arm win is a no-op (the
/// guard re-checks `retry_at`).
fn schedule_transient_retry(ctx: &AgentContext) -> (tokio::time::Instant, f64) {
    // `transient_fails` was just incremented (1-based); backoff uses a 0-based index.
    let attempt = ctx.transient_fails.saturating_sub(1);
    let delay = crate::retry::backoff_delay(&ctx.config.retry_policy, attempt);
    let deadline = tokio::time::Instant::now() + delay;
    *ctx.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(deadline);
    let notify = ctx.retry_notify.clone();
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => notify.notify_one(),
            _ = cancel.cancelled() => {}
        }
    });
    (deadline, delay.as_secs_f64())
}

/// Clear an armed transient-retry deadline. Called on every non-retry wake so a
/// stale stored permit (a sleep that fired after a different arm won the race)
/// cannot trigger a spurious round via the retry arm's guard.
fn clear_transient_retry(ctx: &AgentContext) {
    *ctx.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Arm the wait timer (`break(wait)` or a budget-blocked re-arm): park with a
/// fuse that wakes the outer loop. Mirrors `schedule_transient_retry`'s
/// best-effort, cancel-aware spawn; the `wait_notify` arm's due guard is
/// authoritative. Returns the armed deadline.
fn arm_wait_timer(ctx: &mut AgentContext, timeout_secs: u64) -> tokio::time::Instant {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    *ctx.wait_until.lock().unwrap_or_else(|e| e.into_inner()) = Some(deadline);
    ctx.wait_armed_secs = timeout_secs;
    let notify = ctx.wait_notify.clone();
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => notify.notify_one(),
            _ = cancel.cancelled() => {}
        }
    });
    deadline
}

/// Clear an armed wait timer. Called on every non-wait wake so a stale stored
/// permit cannot fire a spurious turn via the wait arm's guard (same
/// discipline as `clear_transient_retry`).
fn clear_wait_timer(ctx: &AgentContext) {
    *ctx.wait_until.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether the wait timer is genuinely due right now. The wait select arm's
/// authority: a stored permit is only honored when the armed deadline has
/// passed.
fn wait_timer_due(ctx: &AgentContext) -> bool {
    ctx.wait_until
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t <= tokio::time::Instant::now())
        .unwrap_or(false)
}

/// The `[system]` turn injected when the wait timer elapses: the agent decides
/// (keep waiting, finish, or work) — the timer never auto-runs work.
fn wait_elapsed_text(armed_secs: u64) -> String {
    format!(
        "[system] wait timer elapsed (armed {armed_secs}s). Continue waiting with break(wait), \
         finish with break(idle), or work."
    )
}

/// Whether a transient retry is genuinely due right now. The retry select arm's
/// authority: a stored permit is only honored when the armed deadline has passed.
fn transient_retry_due(ctx: &AgentContext) -> bool {
    ctx.retry_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t <= tokio::time::Instant::now())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The core inference: a round-token cancel is classified as an interrupt (keep
    /// living) unless the lifecycle token was also cancelled — which also verifies
    /// the parent→child propagation that makes the classification work.
    #[test]
    fn cancel_kind_classifies_by_lifecycle_token() {
        // Interrupt: cancel only the round token; lifecycle stays uncancelled.
        let lifecycle = CancellationToken::new();
        let round = RoundToken::new(&lifecycle);
        round.cancel();
        assert!(round.handle().is_cancelled());
        assert!(!lifecycle.is_cancelled());
        assert_eq!(CancelKind::classify(&lifecycle), CancelKind::Interrupt);

        // Lifecycle cancel (remove / shutdown): propagates to the round token (child).
        let lifecycle = CancellationToken::new();
        let round = RoundToken::new(&lifecycle);
        lifecycle.cancel();
        assert!(round.handle().is_cancelled());
        assert_eq!(CancelKind::classify(&lifecycle), CancelKind::Lifecycle);
    }

    /// Guard-authority pin: a stored wait permit is inert once the timer was
    /// cleared (an external event won the race) — `wait_timer_due` is the
    /// wait arm's sole authority, so the stale permit cannot fire a turn.
    #[tokio::test]
    async fn stale_wait_permit_is_inert_after_clear() {
        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        arm_wait_timer(&mut ctx, 600);
        assert!(!wait_timer_due(&ctx), "freshly armed fuse is not yet due");
        ctx.wait_notify.notify_one(); // stale permit: timer fires after the clear
        clear_wait_timer(&ctx);
        assert!(!wait_timer_due(&ctx), "stale permit must be inert");
    }

    /// Re-arming replaces the fuse: the second `break(wait)` wins (latest
    /// deadline + its own armed-secs), mirroring the transient-retry
    /// overwrite semantics.
    #[tokio::test]
    async fn rearming_wait_replaces_deadline_and_secs() {
        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        arm_wait_timer(&mut ctx, 600);
        let first = ctx.wait_until.lock().unwrap().unwrap();
        arm_wait_timer(&mut ctx, 0); // zero-delay fuse: due immediately
        let second = ctx.wait_until.lock().unwrap().unwrap();
        assert!(second < first, "re-arm must replace, not min/max, the deadline");
        assert_eq!(ctx.wait_armed_secs, 0);
        assert!(wait_timer_due(&ctx));
    }

    /// Integration test: a MessagePuller returning a message drives the notify
    /// arm to record the message as a user turn before any network call.
    #[tokio::test]
    async fn notify_pull_drives_round() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct StubPuller(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl MessagePuller for StubPuller {
            async fn pull_undelivered(&self) -> Option<String> {
                if self.0.swap(false, Ordering::SeqCst) {
                    Some("inbox test message".to_string())
                } else {
                    None
                }
            }
        }

        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;

        let flag = Arc::new(AtomicBool::new(true));
        ctx.message_puller = Some(Arc::new(StubPuller(flag.clone())));

        let notify = ctx.notify.clone();
        let cancel = ctx.cancel.clone();
        let store = ctx.store.clone();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (_prompt_tx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

        let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

        // Trigger the notify arm.
        notify.notify_one();

        // Poll the store until the message appears (record_turn runs before
        // any network call in run_and_report).
        let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                let has_msg = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
                    m.content()
                        .map(|t| t.contains("inbox test message"))
                        .unwrap_or(false)
                });
                drop(guard);
                if has_msg {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;

        cancel.cancel();
        let _ = rx.recv().await;
        handle.abort();

        assert!(found.unwrap_or(false), "inbox message should drive a round");
    }

    /// Guard-authority twin for the retry arm (the wait twin is above): a
    /// stored permit is inert once the retry deadline was cleared — a sleep
    /// that fired after an external wake won the race cannot fire a turn.
    /// Together with the wait twin this pins both directions of the
    /// design's silent-retry-loss suspicion: permits are doorbells only,
    /// the deadline check is the sole authority.
    #[tokio::test]
    async fn stale_retry_permit_is_inert_after_clear() {
        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        ctx.transient_fails = 1;
        let (_deadline, _) = schedule_transient_retry(&ctx);
        assert!(!transient_retry_due(&ctx), "freshly armed backoff is not yet due");
        ctx.retry_notify.notify_one(); // stale permit: sleep fired after the clear
        clear_transient_retry(&ctx);
        assert!(!transient_retry_due(&ctx), "stale permit must be inert");
    }

    /// Five-wake-set pin (design §9 v2-⑤): an approval decision (approve/
    /// deny) wakes the parked agent through the shared notify arm and the
    /// notification reaches the next round's context — no prompt needed.
    /// (Inbox: `notify_pull_drives_round` above; prompt: every full-loop
    /// test; duty: `update_duty_on_notifies_agent` at the tagma layer;
    /// profile reset: `apply_pending_profile_reset`'s tagma tests.)
    #[tokio::test]
    async fn approval_decision_wakes_agent_and_drives_round() {
        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;

        // Commit + deny one approval so a notification is pending.
        let id = {
            let mut q = ctx.approvals.lock().await;
            let id = q.enqueue("bash_exec", "{}", None);
            q.commit(&id, "test justification").unwrap();
            q.deny(&id, "test deny").unwrap();
            assert!(q.has_notifications());
            id
        };

        let notify = ctx.notify.clone();
        let cancel = ctx.cancel.clone();
        let store = ctx.store.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (_ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

        notify.notify_one();

        // The notification is injected as a user turn before any network
        // call (same ordering as the inbox twin).
        let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                let has = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
                    m.content()
                        .map(|t| t.contains(&format!("Approval {id} has been denied")))
                        .unwrap_or(false)
                });
                drop(guard);
                if has {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;

        cancel.cancel();
        let _ = rx.recv().await;
        handle.abort();
        assert!(
            found.unwrap_or(false),
            "an approval decision must wake the agent and reach the round context"
        );
    }
}
