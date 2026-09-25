//! The direct (local, non-relay) status driver. The chat topics are consumed
//! where they belong — at the local SSE endpoint, which merges the three bus
//! topics over `crate::sse::merge_direct_frames`. Status is a snapshot
//! pump driver instance publishing `StatusSnapshot` onto the bus under the
//! same event-driven policy as the relay pump (the declared unification):
//! wakes on registry invalidations and turn signals with a short debounce,
//! and the fallback ticker only bounds staleness.
//! The projector (see [`crate::external`]) is the sole writer of chat
//! content; this module owns no `chat_history` and stamps nothing.

use std::sync::Weak;

use tracing::{debug, info};

use crate::bus::{SignalFrame, StatusSnapshot};
use crate::pump_driver::{
    SnapshotPumpConfig, SnapshotPumpWakes, SnapshotSink, SnapshotSource, run_snapshot_pump,
};
use crate::relay::status_pump::{STATUS_DEBOUNCE, STATUS_FALLBACK, needs_post, snapshot_status};
use crate::state::AppState;

/// Captures the aggregate status snapshot (the shared pure function).
/// `None` when the `AppState` is gone: the tagma is shutting down.
struct DirectStatusSource(Weak<AppState>);

impl SnapshotSource for DirectStatusSource {
    type Snapshot = StatusSnapshot;

    async fn capture(&self) -> Option<StatusSnapshot> {
        let state = self.0.upgrade()?;
        let payload = snapshot_status(&*state.registry.read().await, &state.token_budget);
        Some(StatusSnapshot(payload))
    }
}

/// Publishes the snapshot onto the bus's status topic. The direct face has
/// no upstream, so every emit counts as delivered and the differential
/// bookkeeping always advances.
struct DirectStatusSink(Weak<AppState>);

impl SnapshotSink for DirectStatusSink {
    type Snapshot = StatusSnapshot;

    async fn emit(&mut self, snapshot: StatusSnapshot) -> bool {
        if let Some(state) = self.0.upgrade() {
            match state.bus.publish(snapshot) {
                Ok(seq) => debug!(site = "direct_status", seq, "snapshot published"),
                Err(err) => info!(error = %err, "direct status publish failed; dropping frame"),
            }
        }
        true
    }
}

/// Start the direct status driver. Called once at startup after the
/// `Arc<AppState>` exists; cancelled by a child of `state.shutdown`, so
/// SIGINT/SIGTERM drains it alongside the rest of the tagma.
pub(crate) fn start(state: &Weak<AppState>) {
    let state = state.clone();
    let Some(app) = state.upgrade() else {
        return; // startup raced shutdown: nothing to serve
    };
    let signals = Some(
        app.bus
            .subscribe::<SignalFrame>()
            .expect("signal topic is registered"),
    );
    let invalidations = app.subscribe_invalidations();
    let cancel = app.shutdown.child_token();
    drop(app);
    let config = SnapshotPumpConfig {
        ticker: STATUS_FALLBACK,
        debounce: Some(STATUS_DEBOUNCE),
        activity: None,
        first_shot: false,
    };
    let wakes = SnapshotPumpWakes {
        cancel,
        signals,
        invalidations,
    };
    tokio::spawn(async move {
        info!("direct status pump started");
        let mut sink = DirectStatusSink(state.clone());
        let source = DirectStatusSource(state.clone());
        run_snapshot_pump(config, wakes, source, &mut sink, |last, fresh, _| {
            needs_post(last.map(|(s, _)| &s.0), &fresh.0)
        })
        .await;
    });
}
