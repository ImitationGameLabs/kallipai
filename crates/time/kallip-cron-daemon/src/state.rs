//! Shared application state for the daemon's HTTP layer.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::store::ScheduleStore;

/// Daemon-wide state. Cloned cheaply (the store handle is internally `Arc`'d).
#[derive(Clone)]
pub struct AppState {
    pub store: ScheduleStore,
    /// Tagma base URL, for verifying a caller's `(agent_id, bearer)` per
    /// management request (HTTP itself goes through `kallip-client`, which
    /// owns the shared reqwest pool + the verify timeout).
    pub tagma_url: String,
    /// Heartbeat stamps from the scheduler + deliverer, so `/health` and the
    /// `healthy` field reflect real progress rather than a static `true`.
    pub liveness: Liveness,
}

pub type SharedState = Arc<AppState>;

/// Per-loop heartbeat stamps shared with the HTTP layer. The scheduler and
/// deliverer stamp at the top of each tick; if either loop dies (panic) or
/// wedges (a tick that never returns), its stamp ages past `*_max_age` and
/// `/health` flips to 503. `Instant` (monotonic) is correct here — clock skew
/// must not turn a live daemon "stale".
///
/// Locks are held only for a single `Instant` copy/assign and never across an
/// `.await`, so `std::sync::Mutex` is correct (not `tokio::sync::Mutex`).
#[derive(Clone)]
pub struct Liveness {
    inner: Arc<LivenessInner>,
}

struct LivenessInner {
    last_tick: Mutex<Instant>,
    last_deliver: Mutex<Instant>,
    tick_max_age: Duration,
    deliver_max_age: Duration,
}

/// Which loop is stale, for the 503 body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale {
    Scheduler,
    Deliverer,
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            Stale::Scheduler => "scheduler stale",
            Stale::Deliverer => "deliverer stale",
        };
        f.write_str(s)
    }
}

impl Liveness {
    /// New cell with both stamps set to now (fresh at startup). `tick_max_age`
    /// and `deliver_max_age` bound how long a loop may go quiet before `/health`
    /// reports it stale.
    pub fn new(tick_max_age: Duration, deliver_max_age: Duration) -> Self {
        let now = Instant::now();
        Self {
            inner: Arc::new(LivenessInner {
                last_tick: Mutex::new(now),
                last_deliver: Mutex::new(now),
                tick_max_age,
                deliver_max_age,
            }),
        }
    }

    /// Record that the scheduler started a tick.
    pub fn saw_tick(&self) {
        self.stamp(&self.inner.last_tick);
    }

    /// Record that the deliverer started a sweep.
    pub fn saw_deliver(&self) {
        self.stamp(&self.inner.last_deliver);
    }

    /// "Heartbeat observed": refresh `slot` to now. The single mutation site
    /// keeps the "stamp before work" contract in one place.
    fn stamp(&self, slot: &Mutex<Instant>) {
        *slot.lock().unwrap() = Instant::now();
    }

    /// `Ok(())` if both loops are fresh relative to `now`, else the stale one.
    /// Takes `now` so staleness is deterministic in tests (`Instant::now() +
    /// Duration` simulates time passing without sleeping).
    pub fn check_at(&self, now: Instant) -> Result<(), Stale> {
        let last_tick = *self.inner.last_tick.lock().unwrap();
        let last_deliver = *self.inner.last_deliver.lock().unwrap();
        if now.duration_since(last_tick) > self.inner.tick_max_age {
            return Err(Stale::Scheduler);
        }
        if now.duration_since(last_deliver) > self.inner.deliver_max_age {
            return Err(Stale::Deliverer);
        }
        Ok(())
    }

    /// Convenience wrapper for the HTTP layer.
    pub fn check(&self) -> Result<(), Stale> {
        self.check_at(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_cell_is_healthy() {
        let live = Liveness::new(Duration::from_secs(3), Duration::from_secs(30));
        // Fresh relative to now.
        assert!(live.check_at(Instant::now()).is_ok());
    }

    #[test]
    fn stale_tick_reports_scheduler() {
        let live = Liveness::new(Duration::from_secs(3), Duration::from_secs(30));
        // Simulate 100s of silence from both loops: tick (3s max) goes stale
        // first and is reported.
        let future = Instant::now() + Duration::from_secs(100);
        assert_eq!(live.check_at(future), Err(Stale::Scheduler));
    }

    #[test]
    fn recent_stamp_clears_staleness() {
        let live = Liveness::new(Duration::from_secs(3), Duration::from_secs(30));
        live.saw_tick();
        live.saw_deliver();
        // Both just stamped; check 1s later so both are still fresh.
        assert!(
            live.check_at(Instant::now() + Duration::from_secs(1))
                .is_ok()
        );
    }

    #[test]
    fn stale_deliver_reported_when_tick_is_fresh() {
        // Tick max age large, deliver small + ancient deliver stamp: only the
        // deliverer is stale, but the scheduler arm is checked first and must
        // not mask it. Build asymmetry by giving the tick a huge window.
        let live = Liveness::new(Duration::from_secs(3600), Duration::from_secs(1));
        // Do not stamp deliver; let the constructor's `now` age past 1s.
        let future = Instant::now() + Duration::from_secs(5);
        live.saw_tick(); // keep scheduler fresh
        assert_eq!(live.check_at(future), Err(Stale::Deliverer));
    }
}
