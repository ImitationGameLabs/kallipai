//! The in-memory quota ledger: the rpm/tpm window counters.
//!
//! Fixed 60-second windows keyed by matrix dimension: per-tagma (the quota
//! share) and per-profile (the total-quota cap). rpm increments on pass
//! (the request itself is the consumption); tpm inspects the current
//! window and token counts are refilled after the upstream response -- a
//! request's own tokens are unknown until then, so a single request can
//! overshoot its window (accepted boundary, declared in the batch report).
//!
//! Counters live in process memory and reset on restart: the operator's
//! persistence ruling (2026-09-18, posture A) -- audit records and the
//! accumulated budget are the database's business, window counters are
//! not. max_budget is not a window at all: the forwarding path checks it
//! against `crate::audit::spent_cost_micros` (the audit table's cost sum).
//!
//! fail-closed: the ledger itself cannot fail (pure memory math); the
//! database side of the checks propagates `DbErr`, which the forwarding
//! path maps to a 500 -- a quota check that cannot run denies the request.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The fixed window length, named after the limit's per-minute semantics.
const WINDOW: Duration = Duration::from_secs(60);

/// Which side of the matrix a counter belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Dimension {
    /// The tagma's quota share (the per-key side, keyed by tagma id).
    Tagma(String),
    /// The profile total (the proxy-wide cap, keyed by profile id).
    Profile(String),
}

/// A denial from the window checks: which dimension, which limit kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Denial {
    pub dimension: &'static str,
    pub kind: &'static str,
    pub limit: i64,
}

#[derive(Debug, Default)]
struct Window {
    bucket_start: Option<Instant>,
    count: u64,
}

impl Window {
    /// The count for the current window, rolling the bucket if it expired.
    fn current(&mut self, now: Instant) -> u64 {
        match self.bucket_start {
            Some(start) if now.duration_since(start) < WINDOW => self.count,
            _ => {
                self.bucket_start = Some(now);
                self.count = 0;
                0
            }
        }
    }

    fn add(&mut self, now: Instant, delta: u64) {
        self.current(now);
        self.count += delta;
    }
}

/// The rpm/tpm window counters, shared through the app state.
#[derive(Debug, Default)]
pub(crate) struct QuotaLedger {
    rpm: Mutex<HashMap<Dimension, Window>>,
    tpm: Mutex<HashMap<Dimension, Window>>,
}

impl QuotaLedger {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The rpm gate: both matrix dimensions must have room; on pass both
    /// counters increment (the request is the consumption). `None` limits
    /// = unlimited for that dimension.
    pub(crate) fn check_rpm(
        &self,
        tagma: &str,
        tagma_rpm: Option<i64>,
        profile: &str,
        profile_rpm: Option<i64>,
    ) -> Result<(), Denial> {
        self.check_rpm_at(Instant::now(), tagma, tagma_rpm, profile, profile_rpm)
    }

    /// The tpm gate: the current window's token counts must have room. No
    /// increment here -- the request's own tokens are counted after the
    /// upstream response ([`QuotaLedger::record_tokens`]).
    pub(crate) fn check_tpm(
        &self,
        tagma: &str,
        tagma_tpm: Option<i64>,
        profile: &str,
        profile_tpm: Option<i64>,
    ) -> Result<(), Denial> {
        self.check_tpm_at(Instant::now(), tagma, tagma_tpm, profile, profile_tpm)
    }

    /// Refill the tpm windows after the upstream response; usage-less
    /// requests add nothing. Both dimensions count the same request.
    pub(crate) fn record_tokens(&self, tagma: &str, profile: &str, tokens: u64) {
        self.record_tokens_at(Instant::now(), tagma, profile, tokens)
    }

    // The `*_at` variants take the clock so the window rollover is
    // testable without sleeping.

    fn check_rpm_at(
        &self,
        now: Instant,
        tagma: &str,
        tagma_rpm: Option<i64>,
        profile: &str,
        profile_rpm: Option<i64>,
    ) -> Result<(), Denial> {
        let mut rpm = self.rpm.lock().expect("rpm ledger lock");
        check_window(
            &mut rpm,
            Dimension::Tagma(tagma.to_owned()),
            tagma_rpm,
            now,
            "tagma",
            "rpm",
        )?;
        check_window(
            &mut rpm,
            Dimension::Profile(profile.to_owned()),
            profile_rpm,
            now,
            "profile",
            "rpm",
        )?;
        // Both dimensions have room: commit the increments (the check and
        // its commit share the lock, so the gate is atomic). Slots spent
        // here are not refunded when a later gate (tpm/max_budget) denies
        // the same request.
        bump(&mut rpm, Dimension::Tagma(tagma.to_owned()), now);
        bump(&mut rpm, Dimension::Profile(profile.to_owned()), now);
        Ok(())
    }

    fn check_tpm_at(
        &self,
        now: Instant,
        tagma: &str,
        tagma_tpm: Option<i64>,
        profile: &str,
        profile_tpm: Option<i64>,
    ) -> Result<(), Denial> {
        let mut tpm = self.tpm.lock().expect("tpm ledger lock");
        check_window(
            &mut tpm,
            Dimension::Tagma(tagma.to_owned()),
            tagma_tpm,
            now,
            "tagma",
            "tpm",
        )?;
        check_window(
            &mut tpm,
            Dimension::Profile(profile.to_owned()),
            profile_tpm,
            now,
            "profile",
            "tpm",
        )
    }

    fn record_tokens_at(&self, now: Instant, tagma: &str, profile: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let mut tpm = self.tpm.lock().expect("tpm ledger lock");
        tpm.entry(Dimension::Tagma(tagma.to_owned()))
            .or_default()
            .add(now, tokens);
        tpm.entry(Dimension::Profile(profile.to_owned()))
            .or_default()
            .add(now, tokens);
    }
}

/// Window room check against one dimension's limit.
fn check_window(
    windows: &mut HashMap<Dimension, Window>,
    key: Dimension,
    limit: Option<i64>,
    now: Instant,
    dimension: &'static str,
    kind: &'static str,
) -> Result<(), Denial> {
    let Some(limit) = limit else {
        return Ok(());
    };
    if limit <= 0 {
        // A non-positive configured limit denies outright: fail-closed on
        // a nonsensical configuration rather than treating it as unlimited.
        return Err(Denial {
            dimension,
            kind,
            limit,
        });
    }
    let used = match windows.entry(key) {
        Entry::Occupied(mut slot) => slot.get_mut().current(now),
        Entry::Vacant(_) => 0,
    };
    if used as i64 >= limit {
        return Err(Denial {
            dimension,
            kind,
            limit,
        });
    }
    Ok(())
}

/// Commit the one-unit increment for a dimension (post-check).
fn bump(windows: &mut HashMap<Dimension, Window>, key: Dimension, now: Instant) {
    windows.entry(key).or_default().add(now, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn later(seconds: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds)
    }

    #[test]
    fn unlimited_dimensions_always_pass() {
        let ledger = QuotaLedger::new();
        for _ in 0..100 {
            ledger
                .check_rpm_at(Instant::now(), "t", None, "p", None)
                .expect("no limits, no denials");
            ledger
                .check_tpm_at(Instant::now(), "t", None, "p", None)
                .expect("no limits, no denials");
        }
    }

    #[test]
    fn rpm_counts_up_and_denies_at_the_limit() {
        let ledger = QuotaLedger::new();
        let now = Instant::now();
        ledger
            .check_rpm_at(now, "t", Some(2), "p", None)
            .expect("first request in");
        ledger
            .check_rpm_at(now, "t", Some(2), "p", None)
            .expect("second request in");
        let denial = ledger
            .check_rpm_at(now, "t", Some(2), "p", None)
            .expect_err("third request over the tagma share");
        assert_eq!(denial.dimension, "tagma");
        assert_eq!(denial.kind, "rpm");
        assert_eq!(denial.limit, 2);
    }

    #[test]
    fn rpm_dimensions_are_independent() {
        let ledger = QuotaLedger::new();
        let now = Instant::now();
        ledger
            .check_rpm_at(now, "t", Some(1), "p", Some(5))
            .expect("first request in");
        let denial = ledger
            .check_rpm_at(now, "t", Some(1), "p", Some(5))
            .expect_err("tagma share exhausted");
        assert_eq!(denial.dimension, "tagma");
    }

    #[test]
    fn rpm_window_rolls_over() {
        let ledger = QuotaLedger::new();
        let now = Instant::now();
        ledger
            .check_rpm_at(now, "t", Some(1), "p", None)
            .expect("request in");
        ledger
            .check_rpm_at(now, "t", Some(1), "p", None)
            .expect_err("window full");
        // Past the window edge the bucket resets.
        ledger
            .check_rpm_at(later(61), "t", Some(1), "p", None)
            .expect("fresh window");
    }

    #[test]
    fn tpm_checks_then_refills() {
        let ledger = QuotaLedger::new();
        let now = Instant::now();
        ledger
            .check_tpm_at(now, "t", Some(100), "p", None)
            .expect("empty window has room");
        ledger.record_tokens_at(now, "t", "p", 100);
        let denial = ledger
            .check_tpm_at(now, "t", Some(100), "p", None)
            .expect_err("window exhausted by the refill");
        assert_eq!(denial.kind, "tpm");
        // A new window accepts again.
        ledger
            .check_tpm_at(later(61), "t", Some(100), "p", None)
            .expect("fresh window");
    }

    #[test]
    fn tpm_usageless_requests_add_nothing() {
        let ledger = QuotaLedger::new();
        let now = Instant::now();
        ledger.record_tokens_at(now, "t", "p", 0);
        ledger
            .check_tpm_at(now, "t", Some(1), "p", None)
            .expect("zero-token refill leaves the window empty");
    }

    #[test]
    fn nonpositive_limits_deny_outright() {
        let ledger = QuotaLedger::new();
        let now = Instant::now();
        for limit in [0, -1] {
            let denial = ledger
                .check_rpm_at(now, "t", Some(limit), "p", None)
                .expect_err("non-positive limit denies");
            assert_eq!(denial.limit, limit);
            let denial = ledger
                .check_tpm_at(now, "t", Some(limit), "p", None)
                .expect_err("non-positive limit denies");
            assert_eq!(denial.kind, "tpm");
        }
    }

    #[test]
    fn different_tagmas_have_independent_windows() {
        let ledger = QuotaLedger::new();
        let now = Instant::now();
        ledger
            .check_rpm_at(now, "a", Some(1), "p", None)
            .expect("tagma a in");
        ledger
            .check_rpm_at(now, "b", Some(1), "p", None)
            .expect("tagma b unaffected by a's window");
    }
}
