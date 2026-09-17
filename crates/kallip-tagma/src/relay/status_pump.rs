//! The status pump: snapshots the tagma's aggregate runtime state (agent
//! counts + token budget) and publishes it onto the bus's status topic for
//! the upstream flusher, which serializes it to the lesche. The pump is
//! event-driven: it wakes on registry invalidations (roster and budget-limit
//! mutations, plus the bridge's agent state-transition bumps) and on turn
//! signals, coalesces bursts through a short debounce, and re-checks on a
//! low-frequency fallback ticker as the staleness bound. Unlike the event
//! [`pump`](super::pump), it is bounded to the session (not the KEX epoch):
//! status is plaintext and key-independent, so it needs none of the pump's
//! drain-before-rotation semantics.
//!
//! Status is a periodic full snapshot, not a delta log: a dropped frame just
//! means slightly-stale data until the next wake, so there is no sequence
//! tracking and no replay path. An unchanged snapshot is never re-published:
//! the lesche caches every delivered snapshot and flushes it to a connecting
//! subscriber, and `GET /tagmata/{id}/status` serves the cache on demand.

use std::sync::Weak;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kallip_common::protocol::AgentState;
use kallip_lesche_common::event::TagmaStatusPayload;
use kallip_runtime::token_budget::TokenBudget;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::RelayHandle;
use crate::bus::{SignalFrame, StatusSnapshot};
use crate::pump_driver::{
    SnapshotPumpConfig, SnapshotPumpWakes, SnapshotSink, SnapshotSource, run_snapshot_pump,
};
use crate::state::AgentRegistry;

/// Coalesce bursts of wakes (a turn's signal and its state-flip bumps land
/// together) into one capture. Implementation constant, not protocol.
pub(crate) const STATUS_DEBOUNCE: Duration = Duration::from_secs(1);
/// Fallback tick cadence for the status pumps: the staleness bound for
/// both faces. Single source -- the relay seeds `status_fallback_ms`
/// from this const.
pub(crate) const STATUS_FALLBACK: Duration = Duration::from_secs(30);

/// The suppression decision, factored out pure (the status family's
/// differential policy — relay and direct instantiate it identically):
/// publish when nothing has been published yet or when the payload
/// changed. Keys on publish, not wire ack: the flusher owns retention.
pub(crate) fn needs_post(last: Option<&TagmaStatusPayload>, fresh: &TagmaStatusPayload) -> bool {
    last != Some(fresh)
}

/// Build a status payload from the current registry + token budget. Pure
/// (lock-free) so it can be unit-tested in isolation. Partitions agents into
/// the root (the conversation peer) vs subagents (spawned helpers) in a single
/// pass via `AgentConfig::is_root` (= `created_by == None`). State goes
/// through `RegistryEntry::state_for_summary` so Live and Faulted entries are
/// handled uniformly (Faulted entries have no bridge-owned atomic).
///
/// `root_state` defaults to [`AgentState::Faulted`] as a defensive fallback
/// for the (production-unreachable) case where no root entry is registered
/// when the pump ticks -- the root is created at startup and non-removable
/// while live, so this only fires under a logic bug. `Faulted` is the safe
/// "no live peer" signal either way.
pub(crate) fn snapshot_status(
    registry: &AgentRegistry,
    token_budget: &TokenBudget,
) -> TagmaStatusPayload {
    let mut root_state = AgentState::Faulted;
    let mut sub_total = 0u32;
    let mut sub_active = 0u32;
    for (_, entry) in registry.iter() {
        let state = entry.state_for_summary();
        if entry.identity().config.is_root() {
            root_state = state;
        } else {
            sub_total += 1;
            if state == AgentState::Busy {
                sub_active += 1;
            }
        }
    }
    let budget = token_budget.snapshot();
    TagmaStatusPayload {
        root_state,
        subagents_total: sub_total,
        subagents_active: sub_active,
        token_budget: budget.budget,
        token_consumed: budget.consumed,
        token_budget_unlimited: budget.unlimited,
    }
}
/// Captures the aggregate status snapshot from the live registry + token
/// budget (the shared pure function above — preserved asset 4, now the
/// driver's capture half). `None` when the `AppState` is gone: the tagma is
/// shutting down and the pump exits.
struct StatusSource(Weak<crate::state::AppState>);

impl SnapshotSource for StatusSource {
    type Snapshot = StatusSnapshot;

    async fn capture(&self) -> Option<StatusSnapshot> {
        let state = self.0.upgrade()?;
        let payload = snapshot_status(&*state.registry.read().await, &state.token_budget);
        Some(StatusSnapshot(payload))
    }
}

/// Publishes each fresh snapshot onto the bus's status topic. The wire
/// belongs to the upstream flusher: publish is this sink's whole
/// job, so every emit counts as delivered and the differential
/// bookkeeping always advances — the flusher owns POST retention.
struct StatusRelaySink(Weak<crate::state::AppState>);

impl SnapshotSink for StatusRelaySink {
    type Snapshot = StatusSnapshot;

    async fn emit(&mut self, snapshot: StatusSnapshot) -> bool {
        if let Some(state) = self.0.upgrade() {
            match state.bus.publish(snapshot) {
                Ok(seq) => debug!(site = "relay_status_pump", seq, "snapshot published"),
                Err(err) => warn!(error = %err, "status snapshot publish failed; dropping frame"),
            }
        }
        true
    }
}

impl RelayHandle {
    /// Ensure the status pump is running. Idempotent: a no-op if one is
    /// already live. Started on tunnel-up; stopped on tunnel-down so a
    /// reconnect installs a fresh pump against the new session.
    pub(super) async fn start_status_pump(&self) {
        self.inner
            .status_pump
            .get_or_spawn(|cancel| self.clone().run_status_pump(cancel))
            .await;
    }

    /// Stop and await the status pump if it is running, clearing the slot so a
    /// later `start_status_pump` can install a fresh one.
    pub(super) async fn stop_status_pump(&self) {
        self.inner.status_pump.stop_and_await().await;
    }

    /// Snapshot total/active agent counts + the token budget: the driver loop
    /// wakes on registry invalidations (roster and budget-limit mutations,
    /// plus the bridge's state-transition bumps) and on turn signals,
    /// coalesces bursts through [`STATUS_DEBOUNCE`], re-checks on the
    /// fallback ticker, gates by [`needs_post`] (unchanged suppression), and
    /// publishes each fresh snapshot onto the bus for the upstream flusher
    /// to drain.
    ///
    /// `Weak::upgrade() == None` (AppState dropped) ends the loop: the tagma
    /// is shutting down. The registry read-guard drops inside the capture,
    /// before the publish, so no `.await` is held under the lock (lock
    /// discipline); capture is lock-free (atomics).
    async fn run_status_pump(self, cancel: CancellationToken) {
        info!(tagma = %self.inner.tagma_id, "relay status pump started");
        let Some(state) = self.inner.state.upgrade() else {
            return; // the tagma is shutting down
        };
        let signals = Some(
            state
                .bus
                .subscribe::<SignalFrame>()
                .expect("signal topic is registered"),
        );
        let invalidations = state.subscribe_invalidations();
        drop(state); // the source re-upgrades per capture; no strong ref held
        let mut sink = StatusRelaySink(self.inner.state.clone());
        let source = StatusSource(self.inner.state.clone());
        let fallback = Duration::from_millis(self.inner.status_fallback_ms.load(Ordering::Relaxed));
        let config = SnapshotPumpConfig {
            ticker: fallback,
            debounce: Some(STATUS_DEBOUNCE),
            activity: None,
            first_shot: false,
        };
        let wakes = SnapshotPumpWakes {
            cancel,
            signals,
            invalidations,
        };
        run_snapshot_pump(config, wakes, source, &mut sink, |last, fresh, _| {
            needs_post(last.map(|(s, _)| &s.0), &fresh.0)
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RegistryEntry;
    use crate::test_helpers::{
        add_faulted_root, add_faulted_sub, add_root, add_sub, make_entry, make_state,
    };
    use kallip_common::agentid::AgentId;
    use kallip_e2ee::DeviceKey;
    use kallip_lesche_client::LescheClient;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// Build a relay wired to a real AppState and no lesche traffic: the
    /// pump is publish-only (the wire belongs to the flusher,
    /// tested in relay/flusher.rs), so these legs observe the bus topic.
    /// Returns the handle and the state strong ref (the pump holds a Weak
    /// the caller must keep alive).
    async fn setup_pump() -> (RelayHandle, crate::state::SharedState) {
        let state = make_state();
        let root = AgentId::from("root".to_string());
        {
            let mut registry = state.registry.write().await;
            registry
                .register_root(
                    root.clone(),
                    RegistryEntry::Live(make_entry(None, "tok".into())),
                )
                .expect("register root");
        }
        // Never dialed: publish-only.
        let client = LescheClient::builder("http://127.0.0.1:1", "tok")
            .build()
            .unwrap();
        let handle = RelayHandle::new(
            client,
            "test".to_string(),
            kallip_archeion_common::ids::TagmaId::from("tagma".to_string()),
            "Tagma".into(),
            DeviceKey::generate(),
            root,
            Arc::downgrade(&state),
        );
        (handle, state)
    }

    /// The pump publishes a snapshot on its first tick (tokio interval
    /// fires at t=0; the debounce window defers the capture ~1s);
    /// `stop_status_pump` cleanly reaps the task.
    #[tokio::test]
    async fn status_pump_posts_snapshot_on_first_tick() {
        let (handle, state) = setup_pump().await;
        let mut rx = state.bus.subscribe::<StatusSnapshot>().expect("topic");
        handle.start_status_pump().await;
        let frame = tokio::time::timeout(Duration::from_millis(2000), rx.recv())
            .await
            .expect("first tick within 2s (debounced)")
            .expect("topic open");
        handle.stop_status_pump().await;
        assert_eq!(frame.0.root_state, AgentState::Idle, "root is idle");
        assert_eq!(
            (frame.0.subagents_total, frame.0.subagents_active),
            (0, 0),
            "no subagents"
        );
    }

    /// Watch invalidation (per-class leg, roster class, end-to-end
    /// through the driver): a roster mutation wakes the pump between
    /// fallback ticks — the second publish lands well inside the 30 s
    /// cadence, which the ticker alone could not have produced.
    #[tokio::test]
    async fn status_pump_wakes_on_a_roster_invalidation() {
        let (handle, state) = setup_pump().await;
        let mut rx = state.bus.subscribe::<StatusSnapshot>().expect("topic");
        handle.start_status_pump().await;
        // First tick (t=0, capture deferred by the debounce window).
        let first = tokio::time::timeout(Duration::from_millis(2000), rx.recv())
            .await
            .expect("first tick within 2s (debounced)")
            .expect("topic open");
        assert_eq!(first.0.subagents_total, 0, "no subs yet");
        // A roster mutation: the watch wake drives an immediate re-snapshot
        // (the new sub changes the payload, so the differential passes).
        let root = AgentId::from("root".to_string());
        {
            let mut registry = state.registry.write().await;
            add_sub(&mut registry, &AgentId::from("late".to_string()), &root);
        }
        let second = tokio::time::timeout(Duration::from_millis(2000), rx.recv())
            .await
            .expect("invalidation wake within 2s (debounced)")
            .expect("topic open");
        handle.stop_status_pump().await;
        assert_eq!(
            second.0.subagents_total, 1,
            "the new sub is in the snapshot"
        );
    }

    /// The root is reported separately from subagents. `subagents_total`
    /// counts Live + Faulted subs (matching `list_agents`); `subagents_active`
    /// counts only subs whose `state_for_summary` is `Busy`. `root_state`
    /// tracks the root's own state. Guards against regressing to a raw-atomic
    /// read, which would panic on Faulted entries (they have no atomic).
    #[tokio::test]
    async fn snapshot_partitions_root_from_subagents() {
        let state = make_state();
        let root = AgentId::from("root".to_string());
        let busy_sub = AgentId::from("busy".to_string());
        let idle_sub = AgentId::from("idle".to_string());
        let faulted_sub = AgentId::from("faulted".to_string());
        {
            let mut registry = state.registry.write().await;
            add_root(&mut registry, &root); // idle by default
            add_sub(&mut registry, &busy_sub, &root);
            add_sub(&mut registry, &idle_sub, &root);
            add_faulted_sub(&mut registry, &faulted_sub, &root, "boom");
            // Flip the root and `busy_sub` to BUSY via their bridge-owned
            // atomics.
            for id in [&root, &busy_sub] {
                registry
                    .get(id)
                    .expect("entry present")
                    .as_live()
                    .expect("live")
                    .agent
                    .state
                    .store(AgentState::BUSY, Ordering::Relaxed);
            }
        }
        let registry = state.registry.read().await;
        let payload = snapshot_status(&registry, &state.token_budget);
        assert_eq!(payload.root_state, AgentState::Busy);
        // busy + idle + faulted = 3 subs; only `busy_sub` is active.
        assert_eq!((payload.subagents_total, payload.subagents_active), (3, 1));
    }

    /// A faulted ROOT with live subs -- the exact disambiguation case the
    /// root/sub split exists for ("root is down but helpers are still
    /// running"): `root_state == Faulted` while subs still count normally.
    #[tokio::test]
    async fn snapshot_faulted_root_with_live_subs() {
        let state = make_state();
        let root = AgentId::from("root".to_string());
        let busy_sub = AgentId::from("busy".to_string());
        {
            let mut registry = state.registry.write().await;
            add_faulted_root(&mut registry, &root, "kaboom");
            add_sub(&mut registry, &busy_sub, &root);
            registry
                .get(&busy_sub)
                .expect("busy_sub present")
                .as_live()
                .expect("busy_sub live")
                .agent
                .state
                .store(AgentState::BUSY, Ordering::Relaxed);
        }
        let registry = state.registry.read().await;
        let payload = snapshot_status(&registry, &state.token_budget);
        assert_eq!(payload.root_state, AgentState::Faulted);
        assert_eq!((payload.subagents_total, payload.subagents_active), (1, 1));
    }

    /// An empty registry (the transient zero-root window) reports
    /// `root_state = Faulted` and zero subs, not a panic.
    #[tokio::test]
    async fn snapshot_empty_registry_reports_faulted_root() {
        let state = make_state();
        let registry = state.registry.read().await;
        let payload = snapshot_status(&registry, &state.token_budget);
        assert_eq!(payload.root_state, AgentState::Faulted);
        assert_eq!((payload.subagents_total, payload.subagents_active), (0, 0));
    }
    /// Silence: with no invalidation wake and no fallback tick in the
    /// window, the pump captures nothing and publishes nothing -- the
    /// 30s fallback, not a fixed interval, schedules the next look.
    ///
    #[tokio::test]
    async fn status_pump_stays_silent_without_wakes() {
        let (handle, state) = setup_pump().await;
        let mut rx = state.bus.subscribe::<StatusSnapshot>().expect("topic");
        handle.start_status_pump().await;
        let _ = tokio::time::timeout(Duration::from_millis(2000), rx.recv())
            .await
            .expect("first tick within 2s (debounced)")
            .expect("topic open");
        // No event wake and no fallback tick inside the window.
        let second = tokio::time::timeout(Duration::from_millis(2500), rx.recv()).await;
        handle.stop_status_pump().await;
        assert!(second.is_err(), "no wake means no capture: silence");
    }

    /// A registry change alters the payload, so the invalidation wake
    /// publishes again — the flip-to-visible contract is what the
    /// suppression must keep.
    #[tokio::test]
    async fn status_pump_posts_again_after_a_change() {
        let (handle, state) = setup_pump().await;
        let mut rx = state.bus.subscribe::<StatusSnapshot>().expect("topic");
        handle.start_status_pump().await;
        // The first tick is immediate; the debounce defers its landing.
        let _ = tokio::time::timeout(Duration::from_millis(2000), rx.recv())
            .await
            .expect("first tick within 2s (debounced)")
            .expect("topic open");
        {
            let registry = state.registry.read().await;
            registry
                .get(&AgentId::from("root".to_string()))
                .expect("root present")
                .as_live()
                .expect("live")
                .agent
                .state
                .store(AgentState::BUSY, Ordering::Relaxed);
        }
        // Production wiring: the bridge bumps invalidations on state
        // flips; mirror that here (the raw atomic write alone is
        // invisible to the pump).
        state.invalidate();
        let second = tokio::time::timeout(Duration::from_millis(2500), rx.recv())
            .await
            .expect("changed snapshot must publish on the wake")
            .expect("topic open");
        handle.stop_status_pump().await;
        assert_eq!(second.0.root_state, AgentState::Busy, "changed snapshot");
    }

    /// Unit test for the suppression decision: an unchanged payload
    /// suppresses; a changed payload (or a first observation) publishes.
    #[test]
    fn needs_post_suppresses_only_unchanged_snapshots() {
        let payload = TagmaStatusPayload {
            root_state: AgentState::Idle,
            subagents_total: 0,
            subagents_active: 0,
            token_budget: 1000,
            token_consumed: 0,
            token_budget_unlimited: false,
        };
        assert!(needs_post(None, &payload), "nothing posted yet");
        assert!(!needs_post(Some(&payload), &payload), "unchanged duplicate");
        let changed = TagmaStatusPayload {
            token_consumed: 1,
            ..payload.clone()
        };
        assert!(needs_post(Some(&payload), &changed), "changed payload");
    }
}
