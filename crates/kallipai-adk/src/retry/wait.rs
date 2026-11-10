//! How long the wait before the next send is, and the sleep itself.
//!
//! Every number a sleep is computed from: the jitter bound, the backoff
//! schedule ([`backoff_schedule`]), the retry wait clamped to what is left of
//! the episode's deadline ([`retry_after`]), the outer re-run's wait
//! ([`backoff_delay`]), and the draw that decorrelates retries across agent
//! instances started together ([`jitter_now`]).
//!
//! None of this reads a `Facts`: the wait is arithmetic over the policy and the
//! numbers the caller passes in. The retry core's verdict stays the only place
//! that decides; the sleeping belongs to the loop in `stream`, and the outer
//! re-run's wait stays in its own shell.

use std::sync::Once;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use tracing::warn;

use super::RetryPolicy;

/// The upper bound a wait's jitter is drawn from. The draw itself is
/// [`jitter_now`], here: the schedule stays a pure function of the value it
/// receives.
pub(crate) const RETRY_JITTER_MAX: Duration = Duration::from_secs(1);

/// The backoff schedule before clamping: `min(base * 2^n, max_delay) + jitter`,
/// with `n` the exponent the caller supplies: the sends already made in the
/// episode for a retry, or the outer re-run index for the round-level wait.
/// The jitter rides outside the cap, so the capped plateau carries it too;
/// `max_delay` stays the schedule's own upper bound.
pub(crate) fn backoff_schedule(policy: &RetryPolicy, exponent: u32, jitter: Duration) -> Duration {
    let exp_delay = policy
        .base_delay
        .saturating_mul(1u32.checked_shl(exponent).unwrap_or(u32::MAX));
    exp_delay.min(policy.max_delay).saturating_add(jitter)
}

/// The wait a retry performs: the schedule, clamped to what is left of the
/// episode's deadline. The clamp lives here, not in the shell, so no caller can
/// sleep past the deadline — and `saturating_sub` keeps a backwards clock from
/// underflowing it.
/// The three numbers it reads come in as values, so the wait face takes a
/// policy and three scalars, not the verdict's input.
pub(super) fn retry_after(
    elapsed: Duration,
    jitter: Duration,
    attempts: u32,
    policy: &RetryPolicy,
) -> Duration {
    let exponent = attempts.saturating_sub(1);
    backoff_schedule(policy, exponent, jitter).min(policy.retry_timeout.saturating_sub(elapsed))
}

/// The wait between two executions — the outer re-run, which sits outside any
/// episode: the backoff schedule with this attempt's jitter.
pub(crate) fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    backoff_schedule(policy, attempt, jitter_now())
}

/// The jitter a wait carries, drawn from the operating system's random source and
/// capped at [`RETRY_JITTER_MAX`]: mixing it in decorrelates retries
/// across agent instances started together.
///
/// The draw takes the source's 32-bit value modulo the cap. The cap does not
/// divide 2^32 evenly, so the lowest 29.5% of the range comes up once in 2^32
/// draws more often than the rest: 25% likelier there, and under 7% of the draws
/// overall. That is immaterial next to what the jitter is for, and removing it
/// would cost a rejection loop.
///
/// A source that fails falls back to zero jitter, which leaves the plain
/// schedule.
///
/// The retry core never reads the clock or a random source: it receives the
/// jitter as a value.
pub(crate) fn jitter_now() -> Duration {
    // A failed draw is reported once: this sits on the retry path, so a line per
    // attempt would repeat one fact.
    static REPORTED: Once = Once::new();
    let cap_nanos = RETRY_JITTER_MAX.as_nanos() as u64;
    match getrandom::u32() {
        Ok(draw) => Duration::from_nanos(u64::from(draw) % cap_nanos),
        Err(error) => {
            REPORTED.call_once(|| {
                warn!(%error, "random source unavailable; jitter falls back to zero");
            });
            Duration::ZERO
        }
    }
}

/// Sleep for `after`, unless `cancel` fires first: `true` when the wait was
/// allowed to finish, `false` when the cancel cut it short. The loop in `stream`
/// sleeps through this one function, so "the wait that actually happened" has a
/// single shape; the outer re-run's wait stays in its own shell.
#[must_use]
pub(super) async fn sleep_unless_cancelled(after: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(after) => true,
        _ = cancel.cancelled() => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    #[test]
    fn backoff_delay_increases_exponentially() {
        let policy = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            retry_timeout: Duration::from_secs(120),
        };

        let d0 = backoff_delay(&policy, 0);
        let d1 = backoff_delay(&policy, 1);
        let d2 = backoff_delay(&policy, 2);

        // Rough exponential growth (jitter makes exact comparison impossible).
        assert!(d0 >= Duration::from_secs(1));
        assert!(d1 >= Duration::from_secs(2));
        assert!(d2 >= Duration::from_secs(4));
    }

    #[test]
    fn backoff_delay_capped_at_max() {
        let policy = RetryPolicy {
            max_retries: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            retry_timeout: Duration::from_secs(120),
        };

        let d = backoff_delay(&policy, 100);
        // 1 * 2^100 would overflow; cap applies.
        assert!(d <= Duration::from_secs(5) + RETRY_JITTER_MAX);
    }

    #[test]
    fn the_drawn_jitter_stays_under_the_cap_and_varies() {
        // The cap is what the wait's upper bound rests on, so every draw has to sit
        // below it whatever the random source hands back. The draws also have to
        // differ from one another: a constant, or a fallback to zero, would leave
        // every retry waiting the same and the decorrelation unpaid for.
        let draws: Vec<Duration> = (0..64).map(|_| jitter_now()).collect();
        assert!(draws.iter().all(|d| *d < RETRY_JITTER_MAX));
        assert!(
            draws.iter().any(|d| *d != draws[0]),
            "every draw came back the same: the source is not varying the wait"
        );
    }

    #[test]
    fn retry_after_follows_the_schedule_under_the_deadline() {
        let policy = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            retry_timeout: Duration::from_secs(300),
        };

        // Third attempt: 1s * 2^2, plus the jitter the caller hands in.
        let after = retry_after(
            Duration::from_secs(1),
            Duration::from_millis(250),
            3,
            &policy,
        );
        assert_eq!(after, Duration::from_millis(4250));
    }

    #[test]
    fn retry_after_clamps_to_the_remaining_deadline() {
        let policy = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            retry_timeout: Duration::from_secs(10),
        };

        // The schedule would ask for 4s, but only 500ms of the deadline is left.
        let after = retry_after(Duration::from_millis(9500), Duration::ZERO, 3, &policy);
        assert_eq!(after, Duration::from_millis(500));
    }
}
