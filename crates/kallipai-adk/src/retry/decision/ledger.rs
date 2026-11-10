//! The attempt ledger of one episode: how many sends it has spent, whether its
//! shape slot is gone, and which window it claims.
//!
//! The shell reads and replaces this between sends: the budget is an input to a
//! verdict, not part of one. What replaces it, and when, is the type contract
//! below.

use std::time::Duration;

use kallipai_common::facts::Block;

use super::attempt::Attempt;
use super::failure_class::ShapeKey;
use super::verdict::Facts;

/// The attempt ledger of one episode: one "first attempt → verdict" stretch of a
/// call chain.
///
/// The ledger spans the connect and mid-stream phases of the same chain (a
/// mid-stream re-entry continues it). It is replaced at every tool round, at
/// every failover advance, and before a rejected request is re-sent without its
/// images. It is a plain value on the agent context — no handle, no lock.
///
/// `attempts` is both the send counter and the retry budget unit, which is what
/// makes "sends per episode ≤ `max_retries + 1`" derivable: `Episode::after_attempt`
/// is the only counting entry point, so a send that skips the count is not a
/// shape the code can express.
///
/// Alongside the budget it carries the episode's shape memory: the key of the
/// last shape-class failure, which is what makes a repeat of the same shape an
/// endpoint-level exit rather than another retry.
#[derive(Clone, Debug)]
pub struct Episode {
    attempts: u32,
    started: u64,
    previous_shape: Option<ShapeKey>,
    /// The window this episode took, if any: a claim answers once per
    /// episode, however many sends the episode makes.
    claim: Option<ClaimedWindow>,
}

/// The window one episode took: the profile it was taken on and the record
/// the claim wrote there. The record is `None` when the claim wrote none —
/// a profile with no fact to take over — which leaves nothing to give back
/// and nothing to recognise.
#[derive(Clone, Debug)]
pub(crate) struct ClaimedWindow {
    profile: String,
    record: Option<Block>,
}

impl ClaimedWindow {
    /// The profile the window belongs to: the record it wrote is keyed
    /// there, and that is where giving it back points.
    pub(crate) fn profile(&self) -> &str {
        &self.profile
    }

    /// The record the claim wrote, when it wrote one.
    pub(crate) fn record(&self) -> Option<&Block> {
        self.record.as_ref()
    }
}

impl Episode {
    /// Start a ledger at `now` (epoch seconds). The caller reads the clock
    /// (`timefmt::now_epoch()`); this module never does.
    pub fn new(now: u64) -> Self {
        Self {
            attempts: 0,
            started: now,
            previous_shape: None,
            claim: None,
        }
    }

    /// Requests sent inside this episode.
    pub(crate) fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Charge one send and hand back the updated ledger.
    pub(crate) fn after_attempt(&self) -> Self {
        Self {
            attempts: self.attempts.saturating_add(1),
            started: self.started,
            previous_shape: self.previous_shape,
            claim: self.claim.clone(),
        }
    }

    /// Record a shape-class failure: the one entry point for the shape memory,
    /// read back by [`shape_sends_allowed`](super::failure_class::shape_sends_allowed).
    pub(crate) fn shape_failed(&self, key: ShapeKey) -> Self {
        Self {
            attempts: self.attempts,
            started: self.started,
            previous_shape: Some(key),
            claim: self.claim.clone(),
        }
    }

    /// Whether this episode already took its window. One episode takes at
    /// most one, however many sends it makes.
    pub(crate) fn claimed(&self) -> bool {
        self.claim.is_some()
    }

    /// The record a claim wrote, when it wrote one. That record is the
    /// episode's own — the one window it may spend — not a fact to obey.
    pub(crate) fn claimed_record(&self) -> Option<&Block> {
        self.claim.as_ref().and_then(ClaimedWindow::record)
    }

    /// The window this episode holds, if any: what it took, on which
    /// profile, and where to give it back.
    pub(crate) fn claimed_window(&self) -> Option<&ClaimedWindow> {
        self.claim.as_ref()
    }

    /// Mark the window as taken, so the next send in this episode does not
    /// claim it again. `record` is the one the claim wrote on the fact table,
    /// which is what makes it the episode's to spend — and, if the episode
    /// never sends, the episode's to give back.
    pub(crate) fn after_claim(&self, profile: &str, record: Option<Block>) -> Self {
        Self {
            claim: Some(ClaimedWindow {
                profile: profile.to_owned(),
                record,
            }),
            ..self.clone()
        }
    }

    /// Drop the window this episode holds without spending it: the ledger
    /// has nothing left to account for on the fact table.
    pub(crate) fn after_release(&self) -> Self {
        Self {
            claim: None,
            ..self.clone()
        }
    }

    /// Time on trial: `now - started`. A backwards clock (`now < started`) counts
    /// as zero rather than underflowing.
    pub(crate) fn elapsed(&self, now: u64) -> Duration {
        Duration::from_secs(now.saturating_sub(self.started))
    }

    /// The verdict input: this ledger's read-only projection plus the attempt's
    /// injected values. Today the only constructor of [`Facts`]: the
    /// `Facts { .. }` literal appears in this one place. The fields are readable
    /// across facets only so this projection can be written here.
    pub(crate) fn facts(&self, attempt: &Attempt) -> Facts {
        Facts {
            attempts: self.attempts,
            elapsed: self.elapsed(attempt.now),
            previous_shape: self.previous_shape,
            phase: attempt.phase,
            failure: attempt.failure,
            shape: attempt.shape,
            jitter: attempt.jitter,
            origin: attempt.origin,
            blocked: attempt.reading.blocked.clone(),
            alternative: attempt.reading.alternative,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn episode_counts_sends_and_measures_time_on_trial() {
        let episode = Episode::new(1_000);
        assert_eq!(episode.attempts(), 0);
        assert_eq!(episode.elapsed(1_005), Duration::from_secs(5));

        let charged = episode.after_attempt();
        assert_eq!(charged.attempts(), 1);
        assert_eq!(
            charged.elapsed(1_005),
            Duration::from_secs(5),
            "the deadline runs from the episode's start, not the last attempt"
        );
        assert_eq!(episode.attempts(), 0, "charging returns a new ledger");
    }

    #[test]
    fn episode_elapsed_treats_a_backwards_clock_as_zero() {
        let episode = Episode::new(1_000);
        assert_eq!(episode.elapsed(999), Duration::ZERO);
    }

    #[test]
    fn episode_attempt_count_saturates() {
        let episode = Episode {
            attempts: u32::MAX,
            started: 0,
            previous_shape: None,
            claim: None,
        };
        assert_eq!(episode.after_attempt().attempts(), u32::MAX);
    }
}
