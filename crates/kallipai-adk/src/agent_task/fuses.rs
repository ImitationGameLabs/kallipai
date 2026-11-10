//! The park machine: one fuse and one generation per park.
//!
//! A park arms the timer its kind calls for, publishes the generation a cancel has
//! to name, and returns the state the caller mirrors; the two halves live together
//! so a mirrored park can never be invisible to the cancel route. The three timers
//! are best effort: the spawned sleep only wakes the loop, and the due guard is
//! what makes a stored permit authoritative.

use super::context::AgentContext;
use crate::lifecycle::LifecycleState;
use kallipai_common::facts::{MIN_RETRY_INTERVAL_SECS, window_end};
use kallipai_common::timefmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Which fuse a wake park arms: `break(wait)` and the budget-probe re-arm
/// share one timer; a chain-transient retry has its own fuse and backoff.
pub(super) enum ParkArm {
    Wait { timeout_secs: u64 },
    Retry,
}

/// An armed park together with the state it mirrors. Only
/// [`AgentContext::arm_park`] produces one, and it also publishes the
/// generation a cancel has to name to reach this park.
#[must_use]
pub(super) struct ArmedPark {
    pub(super) next: LifecycleState,
    /// Backoff a `Retry` park announces to its caller; `None` on a `Wait`
    /// park, whose deadline is already inside `next`.
    pub(super) retry_in_secs: Option<f64>,
}

impl AgentContext {
    /// End the parked episode a cancel names: drop every wake fuse, reset
    /// the transient-failure count, and settle idle. Memory-only — no round
    /// runs here; the caller still keeps this wake from pulling work.
    pub(super) fn end_cancelled_park(&mut self) {
        clear_wait_timer(self);
        clear_transient_retry(self);
        clear_pacing_wake(self);
        // A park cancel ended this episode, so a later unrelated transient
        // failure counts from zero instead of inheriting these parks.
        self.transient_fails = 0;
        self.settle(LifecycleState::Idle, "park cancel");
    }

    /// Arm one wake fuse and publish this park's generation, returning the
    /// state the caller must mirror. Both halves live in this one place: a
    /// mirrored park whose generation was never published is invisible to
    /// the cancel route, so the bump cannot be a call-site duty.
    pub(super) fn arm_park(&mut self, arm: ParkArm) -> ArmedPark {
        let (next, retry_in_secs) = match arm {
            ParkArm::Wait { timeout_secs } => {
                let deadline = arm_wait_timer(self, timeout_secs);
                (
                    LifecycleState::Waiting {
                        until: deadline.into_std(),
                    },
                    None,
                )
            }
            ParkArm::Retry => {
                let (deadline, delay_secs) = schedule_transient_retry(self);
                (
                    LifecycleState::Retrying {
                        attempt: self.transient_fails,
                        max_attempts: self.config.max_transient_retries,
                        retry_at: deadline.into_std(),
                    },
                    Some(delay_secs),
                )
            }
        };
        self.park_epoch.fetch_add(1, Ordering::SeqCst);
        ArmedPark {
            next,
            retry_in_secs,
        }
    }

    /// Mirror the state an armed park names. The only way to publish a wake
    /// park, so the generation and the mirror cannot come apart.
    pub(super) fn settle_park(&self, armed: ArmedPark, cause: &'static str) {
        self.settle(armed.next, cause);
    }
}

/// Hand a park-cancel to the agent task: stamp the cell with the generation
/// this call reads, then wake the loop. The order matters — a wake that
/// arrives before the stamp would drain an empty cell and lose the request
/// silently; the generation keeps a request from reaching a later park, it
/// cannot recover one the loop never saw. The loop still decides at drain
/// time whether the park it finds is the one named here, so a stale request
/// is dropped instead of cancelling the next park. Returns the stamped
/// generation, for the caller's log line and its tests.
pub fn request_park_cancel(
    park_epoch: &Arc<AtomicU64>,
    park_cancel: &Arc<std::sync::Mutex<Option<u64>>>,
    notify: &Arc<tokio::sync::Notify>,
) -> u64 {
    let epoch = park_epoch.load(Ordering::SeqCst);
    *park_cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(epoch);
    notify.notify_one();
    epoch
}

/// Arm a timed retry for the transient (failover-chain-exhausted) path: back off
/// and notify `retry_at` after the delay. Returns the armed `(deadline, delay)`
/// so the caller can ride the same numbers on the terminal event's
/// `transient_retry` payload and the lifecycle state. The spawned sleep is
/// best-effort and cancel-aware — the outer-loop guard is authoritative, so a
/// wake that fires after a hard-park or a different-arm win is a no-op (the
/// guard re-checks `retry_at`).
pub(super) fn schedule_transient_retry(ctx: &AgentContext) -> (tokio::time::Instant, f64) {
    // `transient_fails` was just incremented (1-based); backoff uses a 0-based index.
    let attempt = ctx.transient_fails.saturating_sub(1);
    let delay = crate::retry::backoff_delay(&ctx.config.retry_policy, attempt);
    let deadline = tokio::time::Instant::now() + delay;
    *ctx.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(deadline);
    let notify = ctx.retry_notify.clone();
    let cancel = ctx.cancel.clone();
    // A timer, not work: it carries no context of its own, so the bare
    // spawn is right here.
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
pub(super) fn clear_transient_retry(ctx: &AgentContext) {
    *ctx.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Arm the wait timer (`break(wait)` or a budget-blocked re-arm): park with a
/// fuse that wakes the outer loop. Mirrors `schedule_transient_retry`'s
/// best-effort, cancel-aware spawn; the `wait_notify` arm's due guard is
/// authoritative. Returns the armed deadline.
pub(super) fn arm_wait_timer(ctx: &mut AgentContext, timeout_secs: u64) -> tokio::time::Instant {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    *ctx.wait_until.lock().unwrap_or_else(|e| e.into_inner()) = Some(deadline);
    ctx.wait_armed_secs = timeout_secs;
    let notify = ctx.wait_notify.clone();
    let cancel = ctx.cancel.clone();
    // A timer, not work: it carries no context of its own, so the bare
    // spawn is right here.
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
pub(super) fn clear_wait_timer(ctx: &AgentContext) {
    *ctx.wait_until.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether the wait timer is genuinely due right now. The wait select arm's
/// authority: a stored permit is only honored when the armed deadline has
/// passed.
pub(super) fn wait_timer_due(ctx: &AgentContext) -> bool {
    ctx.wait_until
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t <= tokio::time::Instant::now())
        .unwrap_or(false)
}

/// The `[system]` turn injected when the wait timer elapses: the agent decides
/// (keep waiting, finish, or work) — the timer never auto-runs work.
pub(super) fn wait_elapsed_text(armed_secs: u64) -> String {
    format!(
        "[system] wait timer elapsed (armed {armed_secs}s). Continue waiting with break({{\"until\":\"wait\"}}), \
         finish with break({{\"until\":\"idle\"}}), or work."
    )
}

/// Whether a transient retry is genuinely due right now. The retry select arm's
/// authority: a stored permit is only honored when the armed deadline has passed.
pub(super) fn transient_retry_due(ctx: &AgentContext) -> bool {
    ctx.retry_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t <= tokio::time::Instant::now())
        .unwrap_or(false)
}
/// Arm the wake that ends a quota park: the earliest window end on the chain,
/// plus jitter. The window is read from the table here, so a fact another
/// instance recorded paces this wake too; a chain with no readable fact falls
/// back to `MIN_RETRY_INTERVAL_SECS` counted from now. The fallback only ever
/// sets the alarm — it writes nothing. Mirrors the other timers' best-effort,
/// cancel-aware spawn; the pacing arm's due guard is authoritative.
pub(super) fn arm_pacing_wake(ctx: &AgentContext, profile_id: &str) -> tokio::time::Instant {
    let now = timefmt::now_epoch();
    let wake_epoch = pacing_wake_epoch(ctx, profile_id, now);
    let wait = std::time::Duration::from_secs(wake_epoch.saturating_sub(now));
    let deadline = tokio::time::Instant::now() + wait + crate::retry::jitter_now();
    *ctx.pacing_wake_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(deadline);
    let notify = ctx.pacing_notify.clone();
    let cancel = ctx.cancel.clone();
    // A timer, not work: it carries no context of its own, so the bare
    // spawn is right here.
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => notify.notify_one(),
            _ = cancel.cancelled() => {}
        }
    });
    deadline
}

/// The moment a parked quota round may send again: the earliest window end
/// among the profile it stopped on and the candidates ahead of it.
///
/// A wake starts on the profile the round stopped on; the chain only
/// moves forward from there. A skipped marked candidate leaves the
/// index where it was, so a park can sit mid-chain with profiles ahead.
/// Sleeping on the stopping profile alone would outlast a window that
/// opens sooner.
fn pacing_wake_epoch(ctx: &AgentContext, profile_id: &str, now: u64) -> u64 {
    let Some(table) = ctx.profile_pacing.as_deref() else {
        return now.saturating_add(MIN_RETRY_INTERVAL_SECS);
    };
    let mut earliest = table
        .paced_for(profile_id, now)
        .map(|fact| window_end(&fact.block));
    let mut offset = 1;
    while let Some(candidate) = ctx.failover.candidate_profile(offset) {
        if let Some(end) = table
            .paced_for(&candidate.id, now)
            .map(|fact| window_end(&fact.block))
        {
            earliest = Some(earliest.map_or(end, |open| open.min(end)));
        }
        offset += 1;
    }
    earliest.unwrap_or_else(|| now.saturating_add(MIN_RETRY_INTERVAL_SECS))
}

/// Clear an armed pacing wake. Called on the prompt and notify wakes, and by
/// the pacing arm itself when it spends the permit, so a stale stored permit
/// (a sleep that fired after a different arm won the race) cannot start a
/// round nobody asked for, the same discipline `clear_transient_retry`
/// carries.
pub(super) fn clear_pacing_wake(ctx: &AgentContext) {
    *ctx.pacing_wake_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether the pacing wake is genuinely due right now. The pacing arm's
/// authority: a stored permit is honored only when the armed deadline passed.
pub(super) fn pacing_wake_due(ctx: &AgentContext) -> bool {
    ctx.pacing_wake_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t <= tokio::time::Instant::now())
        .unwrap_or(false)
}
