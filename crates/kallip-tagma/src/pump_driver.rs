//! The snapshot pump driver: the one wake → snapshot → gate → emit loop that
//! every snapshot pump instantiates.
//! The three pumps differ only in policy,
//! and the policy is explicit per instantiation:
//!
//! - relay status: signal wakes + watch invalidations + a fallback ticker;
//!   `needs_post` differential (suppress unchanged captures); publishes
//!   `StatusSnapshot` and POSTs upstream;
//! - projection: signals + watch invalidations + demoted fallback ticker,
//!   1 s debounce, subscription-hint activity gate, tunnel-up first shot;
//!   publishes `ProjectionSnapshot` and PUTs upstream;
//! - direct status: signal wakes + watch invalidations + a fallback ticker;
//!   the same `needs_post` differential as relay (the declared unification);
//!   publishes `StatusSnapshot` only.
//!
//! Complexity guard (same family as the bus core): this module is a
//! loop and two traits, not a framework — no middleware, no priorities, no
//! dynamic dispatch. A new wake source or policy shape is a design-gate
//! question, not a silent extension.

use std::future::pending;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use tokio::sync::broadcast::error::RecvError;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::bus::{SignalFrame, TopicReceiver};

/// Per-pump policy knobs — the shape parameters the three instantiations
/// disagree on. Wake membership is structural (see [`SnapshotPumpWakes`]),
/// not a knob: a pump either has a signal receiver or it does not.
pub(crate) struct SnapshotPumpConfig {
    /// The fallback cadence. Role depends on the pump: primary wake for the
    /// status pumps (2 s), demoted staleness bound for the projection pump
    /// (30 s — watch invalidation carries the discrete mutation classes).
    pub ticker: Duration,
    /// Merge window after a wake: state mutated several times within the
    /// window collapses into one push (projection semantics allow dropping
    /// intermediate states). `None` = emit on every wake.
    pub debounce: Option<Duration>,
    /// Gate that must read `true` for a push to go out (the projection's
    /// subscription hint: an unread projection costs nothing). Checked after
    /// the debounce window, so a flip inside the window still gates the
    /// coalesced push. `None` = always active.
    pub activity: Option<std::sync::Arc<AtomicBool>>,
    /// One unconditional capture+emit before the loop, bypassing the
    /// activity gate and the differential check (the projection's
    /// tunnel-up first shot — the self-heal). A landed first shot
    /// still seeds the differential bookkeeping (`last`), so a
    /// consumer that gates differentially starts from it instead of
    /// re-sending the unchanged snapshot on its next wake. The status
    /// pumps need none: `interval`'s first tick completes immediately,
    /// so the first loop pass emits.
    pub first_shot: bool,
}

/// The structural wake bundle. Cancel always; the rest per pump.
pub(crate) struct SnapshotPumpWakes {
    pub cancel: CancellationToken,
    /// Turn-lifecycle signals (`SignalFrame` topic). `None` for pumps that
    /// do not wake on the chat vocabulary.
    pub signals: Option<TopicReceiver<SignalFrame>>,
    /// Registry invalidation source. Two emission forms feed the same
    /// generation: explicit `AppState::invalidate` calls at write
    /// sites outside the stores (budget-limit, work-schedule,
    /// agent-meta edits, spawn-panic Live→Faulted flips) and senders
    /// embedded in the stores' own mutators (roster:
    /// register/unregister/drain; duty: set/remove). Watch semantics
    /// coalesce mutations that land while the pump is busy.
    pub invalidations: tokio::sync::watch::Receiver<u64>,
}

/// The capture half (preserved asset 4: the shared pure snapshot functions,
/// now driver traits). `None` means the world is gone (`Weak::upgrade`
/// failed — the tagma is shutting down) and the pump exits.
pub(crate) trait SnapshotSource: Send + 'static {
    type Snapshot: Send + 'static;
    fn capture(&self) -> impl Future<Output = Option<Self::Snapshot>> + Send;
}

/// The emit half: publish onto the bus and — while the phase still requires
/// it (the upstream POST/PUT belongs to the flusher) — post upstream.
/// `true` = the emit landed; differential bookkeeping updates only on
/// `true`, mirroring the status pump's updated-only-on-success rule.
pub(crate) trait SnapshotSink: Send + 'static {
    type Snapshot;
    fn emit(&mut self, snapshot: Self::Snapshot) -> impl Future<Output = bool> + Send;
}

/// The one loop. `gate` is the differential policy, explicit per consumer
/// (friction E): the status pumps pass a `needs_post`-style gate (suppress
/// unchanged snapshots); the projection
/// passes `|_, _, _| true` — every wake emits. Bookkeeping: `last` updates
/// only when `emit` reports success, so a failed POST keeps the old value
/// and the next wake retries.
pub(crate) async fn run_snapshot_pump<S, Src, Snk, G>(
    config: SnapshotPumpConfig,
    mut wakes: SnapshotPumpWakes,
    source: Src,
    sink: &mut Snk,
    gate: G,
) where
    S: Send + Clone + 'static,
    Src: SnapshotSource<Snapshot = S>,
    Snk: SnapshotSink<Snapshot = S>,
    G: Fn(Option<&(S, Instant)>, &S, Instant) -> bool + Send,
{
    // Last successfully emitted snapshot + its instant: the differential
    // gate reads it; a wake whose capture equals it is suppressed by
    // policy.
    let mut last: Option<(S, Instant)> = None;
    if config.first_shot {
        match source.capture().await {
            Some(snapshot) => {
                // A landed first shot seeds `last` (the same success
                // rule as the loop's bookkeeping below).
                if sink.emit(snapshot.clone()).await {
                    last = Some((snapshot, Instant::now()));
                }
            }
            None => return,
        }
    }
    let mut ticker = tokio::time::interval(config.ticker);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut signals = wakes.signals;
    loop {
        tokio::select! {
            biased;
            _ = wakes.cancel.cancelled() => return,
            _ = ticker.tick() => {}
            // A signal or a generation bump both mean "state may have
            // changed"; the capture + gate below decide whether anything
            // goes out.
            r = async {
                match signals.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => pending().await,
                }
            } => match r {
                Ok(SignalFrame(_)) => {}
                Err(RecvError::Lagged(_)) => {} // missed frames: re-check
                Err(RecvError::Closed) => {
                    warn!("bus closed; snapshot pump exiting");
                    return;
                }
            },
            r = wakes.invalidations.changed() => {
                // A missed send (receiver lagged the generation) still means
                // "changed"; either way we re-check.
                let _ = r;
            }
        }
        // Merge window: everything that changes during the debounce lands in
        // the same push. Not cancel-selected on purpose (the window is well
        // under the tunnel teardown path's patience).
        if let Some(window) = config.debounce {
            tokio::time::sleep(window).await;
            if let Some(rx) = signals.as_mut() {
                // Drain whatever piled up inside the window (a lagged
                // receiver is dirty by the same argument): one push covers
                // all of it.
                loop {
                    match rx.try_recv() {
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }
        }
        if let Some(active) = &config.activity
            && !active.load(std::sync::atomic::Ordering::Relaxed)
        {
            continue; // nobody is reading: skip the push (push suppression)
        }
        let Some(snapshot) = source.capture().await else {
            return; // the tagma is shutting down
        };
        let now = Instant::now();
        if !gate(last.as_ref(), &snapshot, now) {
            continue;
        }
        if sink.emit(snapshot.clone()).await {
            last = Some((snapshot, Instant::now()));
        }
    }
}
