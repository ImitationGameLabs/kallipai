//! The task's outer loop, and the entrances that reach it.
//!
//! One `select!` waits on a prompt message, the notify signal (inbox delivery,
//! approvals, a profile reset, a park cancel) and the retry, wait and pacing
//! fuses; the lifecycle cancel sits beside them, and the task ends there or when
//! the prompt channel closes. The first prompt runs its wake before the loop, and
//! the notify arm drains the cells it carries before it pulls work.

use super::context::{AgentContext, apply_pending_profile_reset};
use super::fuses::{
    clear_pacing_wake, clear_transient_retry, clear_wait_timer, pacing_wake_due,
    transient_retry_due, wait_elapsed_text, wait_timer_due,
};
use super::wake::{WakeDisposition, wake_and_drain};
use crate::event::AgentEvent;
use crate::lifecycle::LifecycleState;
use crate::retry::AttemptOrigin;
use just_llm_client::types::generation::Message;
use std::sync::atomic::Ordering;

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

/// Cleanup of spill files belongs to the system /tmp mechanisms; the agent
/// runtime does not duplicate it in-process.
pub async fn agent_task(
    mut ctx: AgentContext,
    initial_prompt: Option<String>,
    mut prompt_rx: tokio::sync::mpsc::Receiver<String>,
    agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
) {
    // Pre-loop compaction: a restored agent whose context is over budget is
    // compacted before the first round. The wrapper charges a failure and warns;
    // what it records waits for the first round's gate (a stop the gate fails
    // on, a backoff the gate honors), so this execution emits no error event
    // for a stop it has not taken. The wake restarts the count, not those
    // records (see `restart`).
    crate::context::compact_pre_loop(&mut ctx).await;

    if let Some(p) = initial_prompt {
        if p.is_empty() {
            // No round runs for an empty prompt, and this execution ends here: its
            // context goes with the task's future, taking this execution's ledger
            // with it. That ledger is the only holder of a stop the pre-loop parked,
            // so no gate ever takes that stop. What the pre-loop's failure wrote
            // down outlives the execution because its holders do, whichever of them
            // the verdict filled: the shared context store (retry records), the
            // pacing table, and the agent's own history (a stop's line).
            return;
        }
        ctx.record_turn(vec![Message::user(&p)]).await;
        match wake_and_drain(&mut ctx, &agent_tx, &mut prompt_rx, AttemptOrigin::External).await {
            WakeDisposition::Terminated => return,
            WakeDisposition::Awake | WakeDisposition::Held => {}
        }
    }

    loop {
        tokio::select! {
            input = prompt_rx.recv() => {
                // Declared race window (accepted, microseconds): this arm has
                // no parked guard, so a busy notice enqueued just before the
                // recipient parks is presented here and runs one round while
                // parked. Nothing is lost: the message body already sits in
                // the inbox and the notice is only a visibility hint. The
                // sender side of this window is documented in delivery.rs.
                match input {
                    Some(text) => {
                        clear_transient_retry(&ctx);
                        clear_wait_timer(&ctx);
                        clear_pacing_wake(&ctx);
                        ctx.record_message_turn(&text).await;
                        match wake_and_drain(
                            &mut ctx,
                            &agent_tx,
                            &mut prompt_rx,
                            AttemptOrigin::External,
                        )
                        .await
                        {
                            WakeDisposition::Terminated => break,
                            WakeDisposition::Awake | WakeDisposition::Held => {}
                        }
                    }
                    None => break,
                }
            }
            // Lifecycle cancel: terminate the task.
            // Per-agent interrupt never reaches here — it cancels only the current
            // round token inside `run_and_report`, not the lifecycle token.
            _ = ctx.cancel.cancelled() => {
                tracing::info!("agent task: lifecycle cancel, persisting and exiting");
                terminate_cancelled(&ctx, &agent_tx).await;
                break;
            }
            _ = ctx.notify.notified() => {
                // Four producers share this Notify: inbox message delivery,
                // approval notifications, the profile-apply handler, and the
                // interrupt route (which write a cell then call notify_one —
                // a reset cell and a park-cancel cell, both drained here).
                // Notify coalesces permits (at most 1 stored), so one wake may
                // carry work from several producers. The reset is applied
                // first: it is memory-only (no round runs), and a wake that
                // ends in a park-cancel must not postpone it to a later wake.
                if ctx
                    .pending_profile_reset
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some()
                {
                    apply_pending_profile_reset(&mut ctx).await;
                }
                // Park-cancel cell next: drained unconditionally, so a
                // request can never outlive the wake it rode in on. It
                // applies only to the park it names — the stamp must still
                // be the current generation and the authoritative lifecycle
                // must still be a wake park — and anything else is dropped
                // where it stands.
                let park_cancel = ctx
                    .park_cancel
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(stamp) = park_cancel
                    && stamp == ctx.park_epoch.load(Ordering::SeqCst)
                    && matches!(
                        *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()),
                        LifecycleState::Waiting { .. } | LifecycleState::Retrying { .. }
                    )
                {
                    ctx.end_cancelled_park();
                    // The park form of an interrupt: no round was running, so the
                    // event says the wait ended rather than a turn was cut short.
                    agent_tx.send(AgentEvent::WaitCancelled).await.ok();
                    // The cancel owns this wake: falling through would let the
                    // merged permits pull a message and start a round the
                    // interrupt was meant to prevent. Wake the loop once more
                    // instead: with the cell drained, the fuses cleared and the
                    // agent idle, that wake serves work that rode the same
                    // permit (an inbox message, an approval) or sleeps.
                    ctx.notify.notify_one();
                    continue;
                }

                // Parked guard (design guard matrix: notify events buffer,
                // they do not wake a parked agent). The delivery gate's kick
                // turn (auto-wake on message arrival) is the sole exit from
                // Parked; a bare notify must not run a round the agent never
                // consented to (the kick turn tells the agent why it parked;
                // the message itself waits in the inbox for the wake's
                // post-round drain).
                // The reset cell was applied above — memory-only, no round
                // runs — so a later kick runs on the fresh client.
                if matches!(
                    *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()),
                    LifecycleState::Parked { .. }
                ) {
                    continue;
                }

                let mut should_run = false;

                // Pull undelivered inbox messages (after reset so the round
                // uses the fresh client). Drains ALL undelivered in one atomic
                // call to survive notify coalescing.
                if let Some(ref puller) = ctx.message_puller
                    && let Some(msg) = puller.pull_undelivered().await
                {
                    ctx.record_message_turn(&msg).await;
                    should_run = true;
                }

                // Approval notifications.
                if ctx.approvals.lock().await.has_notifications() {
                    should_run = true;
                }

                if should_run {
                    clear_transient_retry(&ctx);
                    clear_wait_timer(&ctx);
                    clear_pacing_wake(&ctx);
                    match wake_and_drain(&mut ctx, &agent_tx, &mut prompt_rx, AttemptOrigin::External)
                        .await
                    {
                        WakeDisposition::Terminated => break,
                        WakeDisposition::Awake | WakeDisposition::Held => {}
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
                    match wake_and_drain(&mut ctx, &agent_tx, &mut prompt_rx, AttemptOrigin::System)
                        .await
                    {
                        WakeDisposition::Terminated => break,
                        WakeDisposition::Awake | WakeDisposition::Held => {}
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
                        ctx.record_turn(vec![Message::user(wait_elapsed_text(armed_secs))])
                            .await;
                    }
                    match wake_and_drain(&mut ctx, &agent_tx, &mut prompt_rx, AttemptOrigin::System)
                        .await
                    {
                        WakeDisposition::Terminated => break,
                        WakeDisposition::Awake | WakeDisposition::Held => {}
                    }
                }
            }
            // Pacing wake: the window a quota park was waiting out has ended.
            // Guard-authoritative like the other timer arms. The round starts
            // fresh, so its episode claims the window rather than obeying the
            // record this wake was armed for.
            _ = ctx.pacing_notify.notified() => {
                if pacing_wake_due(&ctx) {
                    clear_pacing_wake(&ctx);
                    match wake_and_drain(&mut ctx, &agent_tx, &mut prompt_rx, AttemptOrigin::System)
                        .await
                    {
                        WakeDisposition::Terminated => break,
                        WakeDisposition::Awake | WakeDisposition::Held => {}
                    }
                }
            }
        }
    }
}

/// Persist context + approval state and emit the terminal [`AgentEvent::Cancelled`].
///
/// The single exit path for a lifecycle cancel, shared by the
/// outer-loop cancel arm (idle) and `run_and_report`'s mid-round lifecycle-cancel branch.
pub(super) async fn terminate_cancelled(
    ctx: &AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    ctx.persist().await;
    agent_tx.send(AgentEvent::Cancelled).await.ok();
}
