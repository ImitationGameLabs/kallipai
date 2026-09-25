//! The projection pump: pushes the tagma's manage-plane projection (full
//! roster + aggregate status) to the lesche whenever the state may have
//! changed and someone is actually reading it.
//!
//! Three sources wake the pump:
//! - tunnel-up: an unconditional full first snapshot (the self-heal — the
//!   lesche's in-memory projection dies with its process, and a reconnecting
//!   tagma must repopulate it regardless of any buffered hint, which by
//!   definition cannot have survived either side's restart);
//! - a `SignalFrame` on the typed topic bus (the third subscriber:
//!   turn-state transitions nudge an immediate re-snapshot, debounced);
//! - registry invalidations (a watch the AppState bumps on the discrete
//!   mutation classes the Signal vocabulary cannot see — roster changes
//!   (spawn/remove), duty flips, budget-limit writes, work-schedule edits);
//! - a low-frequency fallback ticker as the staleness bound: it
//!   catches anything the invalidation source missed.
//!
//! Everything is suppressed while `projection_active` is false (no subscriber
//! on the lesche side), except the tunnel-up first shot. Pushes are
//! best-effort publishes: the upstream flusher owns wire retention and
//! retry (see relay/flusher.rs).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use kallipai_lesche_common::projection::ProjectionSnapshot;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::RelayHandle;
use super::status_pump::snapshot_status;
use crate::bus::SignalFrame;
use crate::pump_driver::{
    SnapshotPumpConfig, SnapshotPumpWakes, SnapshotSink, SnapshotSource, run_snapshot_pump,
};
use crate::state::AppState;

/// Merge window after a wake-up: state mutated several times within the
/// window collapses into one push (projection semantics allow dropping
/// intermediate states). Implementation constant, not protocol.
const PROJECTION_DEBOUNCE: Duration = Duration::from_secs(1);

/// Build the full projection snapshot from the current registry + token
/// budget + work-schedule singleton. Lock-free like `snapshot_status`: the
/// registry read-guard is dropped before any await happens (the caller
/// holds no lock across the awaits).
fn snapshot_projection(
    state: &AppState,
    registry: &crate::state::AgentRegistry,
    push_seq: u64,
    work_schedule: Option<kallipai_lesche_common::projection::WorkScheduleProjection>,
) -> ProjectionSnapshot {
    ProjectionSnapshot {
        agents: registry
            .iter()
            .map(|(id, entry)| state.summarize(id, entry))
            .collect(),
        status: snapshot_status(registry, &state.token_budget),
        push_seq,
        work_schedule,
    }
}

/// Project the tagma's work-schedule singleton into its prompt-free wire
/// form (`wake_prompt`/`final_warn_prompt` are dropped here).
fn work_schedule_projection(
    ws: &crate::work_schedule::WorkSchedule,
) -> kallipai_lesche_common::projection::WorkScheduleProjection {
    use kallipai_lesche_common::projection::WorkScheduleProjection;
    WorkScheduleProjection {
        id: ws.id.clone(),
        spec: serde_json::to_value(&ws.spec).expect("spec serializes"),
        pre_warn_minutes: ws.pre_warn_minutes,
        final_warn_minutes: ws.final_warn_minutes,
        status: serde_json::to_value(ws.status).expect("status serializes"),
        created_at: ws.created_at,
    }
}

/// Captures the full projection snapshot (roster + status + work schedule)
/// under the registry read-lock, stamping the connection-lifetime push seq
/// (the lesche keys same-generation replay rejection on it; never reset by a
/// pump restart). The guard is dropped before any await (lock discipline
/// discipline); a store read failure degrades that push to no-schedule
/// rather than blocking the whole snapshot. `None` when the `AppState` is
/// gone: the tagma is shutting down.
struct ProjectionSource {
    state: Weak<AppState>,
    push_seq: Arc<std::sync::atomic::AtomicU64>,
}

impl SnapshotSource for ProjectionSource {
    type Snapshot = crate::bus::ProjectionSnapshot;

    async fn capture(&self) -> Option<crate::bus::ProjectionSnapshot> {
        let state = self.state.upgrade()?;
        let push_seq = self.push_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let work_schedule = match state.work_schedules.get() {
            Some(store) => match store.get_singleton().await {
                Ok(Some(ws)) => Some(work_schedule_projection(&ws)),
                Ok(None) => None,
                Err(e) => {
                    // The projection is prompt-free by construction; a store
                    // read failure degrades this push to no-schedule rather
                    // than blocking the whole snapshot.
                    warn!(error = ?e, "work-schedule read failed; pushing without it");
                    None
                }
            },
            None => None,
        };
        let snapshot = {
            let registry = state.registry.read().await;
            snapshot_projection(&state, &registry, push_seq, work_schedule)
        };
        Some(crate::bus::ProjectionSnapshot(snapshot))
    }
}

/// Publishes each fresh projection onto the bus's projection topic. The
/// wire belongs to the upstream flusher: publish is this sink's
/// whole job, so every emit counts as delivered and the differential
/// bookkeeping always advances — the flusher owns PUT retention.
struct ProjectionSink(Weak<AppState>);

impl SnapshotSink for ProjectionSink {
    type Snapshot = crate::bus::ProjectionSnapshot;

    async fn emit(&mut self, snapshot: crate::bus::ProjectionSnapshot) -> bool {
        if let Some(state) = self.0.upgrade()
            && let Err(err) = state.bus.publish(snapshot)
        {
            // A no-subscriber publish is benign Ok (the flusher and the
            // local SSE hold receivers in production); this arm only
            // fires on a registry/slot bug.
            warn!(error = %err, "projection publish failed; dropping frame");
        }
        true
    }
}

impl RelayHandle {
    /// Ensure the projection pump is running. Idempotent. Started on
    /// tunnel-up (which also fires the unconditional full first shot) and on
    /// a `false -> true` subscription-hint flip; stopped on tunnel-down and
    /// on `true -> false` so an unread projection costs nothing.
    pub(super) async fn start_projection_pump(&self) {
        self.inner
            .projection_pump
            .get_or_spawn(|cancel| self.clone().run_projection_pump(cancel))
            .await;
    }

    /// Stop and await the projection pump if it is running, clearing the slot.
    pub(super) async fn stop_projection_pump(&self) {
        self.inner.projection_pump.stop_and_await().await;
    }

    /// Consume the projection face of an `OwnerSub` frame from the tunnel
    /// consumption: the hint only starts/stops the pump -- the pump itself
    /// decides, per push, whether the flag allows a POST).
    pub(super) async fn handle_projection_hint(&self, active: bool) {
        let was = self.inner.projection_active.swap(active, Ordering::Relaxed);
        if was == active {
            return; // includes the reconnect `true -> true` re-send
        }
        if active {
            self.start_projection_pump().await;
        } else {
            self.stop_projection_pump().await;
        }
    }

    /// Subscribe to the signal topic and push the projection on Signal, on a
    /// registry invalidation, on the fallback tick, and once unconditionally
    /// at tunnel-up (the driver's first shot). Exits when the tunnel dies
    /// (`cancel`), the AppState drops, or the bus closes.
    async fn run_projection_pump(self, cancel: CancellationToken) {
        info!(tagma = %self.inner.tagma_id, "relay projection pump started");
        let Some(state) = self.inner.state.upgrade() else {
            return; // the tagma is shutting down
        };
        // Both chat topics are registered at construction, so the only
        // failure here would be a broken registry build.
        let signals = Some(
            state
                .bus
                .subscribe::<SignalFrame>()
                .expect("signal topic is registered"),
        );
        let invalidations = state.subscribe_invalidations();
        drop(state); // the source re-upgrades per capture; no strong ref held
        let fallback =
            Duration::from_millis(self.inner.projection_fallback_ms.load(Ordering::Relaxed));
        let mut sink = ProjectionSink(self.inner.state.clone());
        let source = ProjectionSource {
            state: self.inner.state.clone(),
            push_seq: Arc::clone(&self.inner.projection_push_seq),
        };
        let config = SnapshotPumpConfig {
            ticker: fallback,
            debounce: Some(PROJECTION_DEBOUNCE),
            activity: Some(Arc::clone(&self.inner.projection_active)),
            first_shot: true,
        };
        let wakes = SnapshotPumpWakes {
            cancel,
            signals,
            invalidations,
        };
        run_snapshot_pump(config, wakes, source, &mut sink, |_, _, _| true).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::SignalFrame;
    use crate::external::ExternalProjector;
    use crate::relay::MessageLimits;
    use crate::state::RegistryEntry;
    use crate::test_helpers::{make_entry, make_state};
    use kallipai_common::protocol::SignalEvent;
    use kallipai_e2ee::DeviceKey;
    use kallipai_lesche_client::LescheClient;
    use std::sync::Arc;
    use std::time::Duration;

    /// Build a RelayHandle wired to a real AppState (root registered,
    /// external projector installed) and no lesche traffic: the pump is
    /// publish-only (the wire belongs to the flusher), so these
    /// legs observe the bus topic. Returns the handle and the state strong
    /// ref (the pump holds a Weak).
    async fn setup() -> (RelayHandle, crate::state::SharedState) {
        let state = make_state();
        let root = kallipai_common::agentid::AgentId::from("root".to_string());
        {
            let mut registry = state.registry.write().await;
            registry
                .register_root(
                    root.clone(),
                    RegistryEntry::Live(make_entry(None, "tok".into())),
                )
                .unwrap();
        }
        let projector = ExternalProjector::new(
            Arc::downgrade(&state),
            None,
            None,
            None,
            None,
            MessageLimits::default(),
        );
        let _ = state.external.set(projector); // single install per test state
        let client = LescheClient::builder("http://127.0.0.1:1", "tok")
            .build()
            .unwrap();
        let handle = RelayHandle::new(
            client,
            "test".to_string(),
            kallipai_archeion_common::ids::TagmaId::from("tagma".to_string()),
            "Tagma".into(),
            DeviceKey::generate(),
            root,
            Arc::downgrade(&state),
        );
        (handle, state)
    }

    /// Collect the next `n` published projections (wire payloads), each
    /// within a generous window (debounce is 1 s; a push lands inside two
    /// windows plus scheduler slack).
    async fn wait_pushes(
        rx: &mut crate::bus::TopicReceiver<crate::bus::ProjectionSnapshot>,
        n: usize,
    ) -> Vec<ProjectionSnapshot> {
        let mut got = Vec::new();
        for _ in 0..n {
            let frame = tokio::time::timeout(Duration::from_millis(1800), rx.recv())
                .await
                .expect("push within window")
                .expect("topic open");
            got.push(frame.0);
        }
        got
    }

    /// Push suppression + hierarchical hint consumption: with the pump
    /// started but the last hint `false`, wakes do not push; the `false ->
    /// true` flip restarts the pump (whose first shot lands); `true -> false`
    /// stops it again.
    #[tokio::test]
    async fn hint_flip_gates_pushes() {
        let (handle, state) = setup().await;
        let mut rx = state
            .bus
            .subscribe::<crate::bus::ProjectionSnapshot>()
            .expect("topic");
        // Simulate "the lesche told us nobody is listening": the pump stays
        // stopped and nothing is pushed.
        handle.handle_projection_hint(false).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "false hint: no pushes"
        );
        // The flip to `true` starts the pump, whose first shot lands.
        handle.handle_projection_hint(true).await;
        let got = wait_pushes(&mut rx, 1).await;
        assert_eq!(got.len(), 1, "true hint starts the pump + first shot");
        // And back to `false`: the pump stops, so no further pushes.
        handle.handle_projection_hint(false).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "false hint stops pushes"
        );
        handle.stop_projection_pump().await;
    }
    /// Self-heal: tunnel-up (pump start) pushes one full snapshot
    /// unconditionally, and a stop/start cycle (the tagma's tunnel
    /// reconnect, or a full restart) pushes another one without any hint
    /// traffic.
    #[tokio::test]
    async fn tunnel_up_first_shot_and_restart_repushes() {
        let (handle, state) = setup().await;
        let mut rx = state
            .bus
            .subscribe::<crate::bus::ProjectionSnapshot>()
            .expect("topic");
        handle.start_projection_pump().await;
        let got = wait_pushes(&mut rx, 1).await;
        assert_eq!(got.len(), 1, "tunnel-up first shot");
        assert_eq!(got[0].push_seq, 1, "first shot is seq 1");
        assert_eq!(
            got[0].agents.len(),
            1,
            "the registered root is in the roster"
        );
        assert_eq!(
            got[0].status.root_state,
            kallipai_common::protocol::AgentState::Idle
        );
        // Simulate a reconnect: tunnel-down stops the pump, tunnel-up
        // starts a fresh one whose first shot repopulates the (possibly
        // restarted) lesche's projection.
        handle.stop_projection_pump().await;
        handle.start_projection_pump().await;
        let got = wait_pushes(&mut rx, 1).await;
        assert_eq!(got.len(), 1, "restart pushes a fresh full snapshot");
        // The counter is monotonic across a pump restart -- the second
        // shot continues the sequence (2) rather than replaying 1, which
        // the lesche's same-generation check would reject.
        assert_eq!(got[0].push_seq, 2, "seq continues across restart");
        handle.stop_projection_pump().await;
    }

    /// A burst of external-bus Signals collapses into a single debounced
    /// push (the merge window): three signals inside the 1 s merge window
    /// are a single state change, so exactly one extra frame lands.
    #[tokio::test]
    async fn signal_burst_debounces_into_one_push() {
        let (handle, state) = setup().await;
        let mut rx = state
            .bus
            .subscribe::<crate::bus::ProjectionSnapshot>()
            .expect("topic");
        handle.handle_projection_hint(true).await;
        let got = wait_pushes(&mut rx, 1).await;
        assert_eq!(got.len(), 1, "first shot before the burst");
        for _ in 0..3 {
            state
                .bus
                .publish(SignalFrame(SignalEvent::Idle))
                .expect("SignalFrame is registered at the construction-time single site");
        }
        // One debounce window later exactly one push must have landed.
        let got = wait_pushes(&mut rx, 1).await;
        assert_eq!(got.len(), 1, "3-signal burst -> exactly one extra push");
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "no third push from a single burst"
        );
        handle.stop_projection_pump().await;
    }

    /// Quality m-3: with a shortened fallback interval, the tick alone (no
    /// signals) drives additional pushes while the hint is active -- the
    /// coverage for registry changes that never reach the external bus
    /// (roster/duty/budget/schedule).
    #[tokio::test]
    async fn fallback_tick_drives_a_push_without_signals() {
        let (handle, state) = setup().await;
        let mut rx = state
            .bus
            .subscribe::<crate::bus::ProjectionSnapshot>()
            .expect("topic");
        handle
            .inner
            .projection_fallback_ms
            .store(150, Ordering::Relaxed);
        handle.handle_projection_hint(true).await;
        let got = wait_pushes(&mut rx, 1).await;
        assert_eq!(got.len(), 1, "first shot on pump start");
        let got = wait_pushes(&mut rx, 1).await;
        assert_eq!(got.len(), 1, "fallback tick pushed once more");
        handle.stop_projection_pump().await;
    }

    /// Content nail: the work-schedule projection is prompt-free by
    /// construction -- `wake_prompt`/`final_warn_prompt` never reach the
    /// wire, and the projected fields round-trip faithfully.
    #[tokio::test]
    async fn work_schedule_projection_is_prompt_free() {
        let ws: crate::work_schedule::WorkSchedule = serde_json::from_value(serde_json::json!({
            "id": "ws-1",
            "spec": { "mode": "always" },
            "pre_warn_minutes": 5,
            "final_warn_minutes": 2,
            "final_warn_prompt": "SECRET-FINAL",
            "wake_prompt": "SECRET-WAKE",
            "status": "paused",
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .expect("schedule parses");
        let projected = super::work_schedule_projection(&ws);
        let json = serde_json::to_value(&projected).expect("projection serializes");
        let obj = json.as_object().expect("projection is an object");
        for forbidden in ["wake_prompt", "final_warn_prompt", "message"] {
            assert!(!obj.contains_key(forbidden), "{forbidden} must not leak");
        }
        assert_eq!(obj["id"], "ws-1");
        assert_eq!(obj["spec"]["mode"], "always");
        assert_eq!(obj["pre_warn_minutes"], 5);
        assert_eq!(obj["final_warn_minutes"], 2);
        assert_eq!(obj["status"], "paused");
    }
}
