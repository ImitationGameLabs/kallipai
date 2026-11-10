//! Families whose value another owner already holds, read at collection.
//!
//! An observable is a callback, not a copy. The reader calls it when it
//! collects, the callback reads whichever owner is attached at that moment,
//! and the value leaves on the same export as every recorded family: a
//! monotonic tally stays a counter, so a reader can still rate it, and a
//! derived number stays a gauge with nothing mirrored between collections.
//!
//! Sources, one per registered instrument:
//!
//! - the relay gauges and the build identity, both owned by [`Metrics`];
//! - the heartbeat table, for the periodic loops of [`super::Beat`];
//! - the export pipeline's tally (`kallipai_common::telemetry::export_stats`);
//! - the event bus's per-topic publishes and lag (`bus::stats`);
//! - the agent roster and the duty store (a read of the registry).
//!
//! A collection never waits on another owner: the roster read is `try_read`,
//! so a registry busy with a mutation yields no roster samples this time (the
//! next collection is a minute later, and a missing gauge reads as staleness
//! rather than as a wrong number). The two owner slots are mutexes around the
//! handle only: taken to clone it, released before anything is observed.
//!
//! Each slot is written after construction, because the instruments register
//! before either owner exists, and written again when the owner is replaced,
//! so the families always follow the owner that is alive.

use std::sync::{Arc, Mutex, MutexGuard, Weak};

use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

use super::{Beat, Metrics};
use crate::UNENROLLED_TAGMA_ID;
use crate::state::{RegistryEntry, SharedState};

/// The identity `build_info` carries: one build, one registration.
#[derive(Clone)]
pub(super) struct BuildInfo {
    pub(super) version: String,
    /// `None` on a local-only tagma, reported under [`UNENROLLED_TAGMA_ID`]:
    /// enrolling the instance later changes a value, never the shape.
    pub(super) registration: Option<String>,
}

/// The two owners the callbacks read through, both attached after the
/// instruments exist.
///
/// A slot takes a second value: a process that replaces its metric set or its
/// tagma state re-attaches, and the collection follows the owner that is
/// alive.
#[derive(Default)]
struct Owners {
    metrics: Mutex<Option<Weak<Metrics>>>,
    state: Mutex<Option<Weak<crate::state::AppState>>>,
}

/// Take one slot's lock, ignoring a poisoned flag: the critical section is one
/// assignment or one clone, so nothing here can panic while holding it, and a
/// panic elsewhere must not turn a collection into an empty one.
fn slot<T>(slot: &Mutex<T>) -> MutexGuard<'_, T> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Owners {
    fn metrics(&self) -> Option<Arc<Metrics>> {
        let held = slot(&self.metrics);
        held.as_ref()?.upgrade()
    }

    fn state(&self) -> Option<SharedState> {
        let held = slot(&self.state);
        held.as_ref()?.upgrade()
    }
}

/// The process-wide export tally, read once per collection.
fn export_snapshot() -> kallipai_common::telemetry::ExportSnapshot {
    kallipai_common::telemetry::export_stats().snapshot()
}

/// The registered observables. Every clone shares the same owners, and the
/// copy [`Metrics`] holds is the one the two `attach` calls write.
#[derive(Clone, Default)]
pub(crate) struct Observable {
    owners: Arc<Owners>,
}

impl Observable {
    /// Register every callback against `meter`.
    ///
    /// The returned handles are dropped on purpose: a callback is registered
    /// with the meter's pipeline at build time, and the reader calls it on
    /// every collection of its own accord.
    pub(crate) fn register(meter: &Meter) -> Self {
        let observable = Self::default();
        observable.relays(meter);
        observable.identity(meter);
        observable.beats(meter);
        observable.export(meter);
        observable.bus(meter);
        observable.roster(meter);
        observable
    }

    /// Point the callbacks at the metric set they belong to, replacing an
    /// owner attached before it.
    pub(crate) fn attach(&self, metrics: &Arc<Metrics>) {
        *slot(&self.owners.metrics) = Some(Arc::downgrade(metrics));
    }

    /// Point the callbacks at the running tagma, replacing an owner attached
    /// before it.
    pub(crate) fn attach_state(&self, state: &SharedState) {
        *slot(&self.owners.state) = Some(Arc::downgrade(state));
    }

    /// How many relay entries the operator configured, and how many tunnels
    /// are open. Read side by side: 0 configured is a local-only tagma, which
    /// is a different fact from a configured tunnel that is down.
    fn relays(&self, meter: &Meter) {
        let owners = Arc::clone(&self.owners);
        meter
            .i64_observable_gauge("kallipai_tagma.relays_configured")
            .with_description("Relay entries the operator configured; 0 is a local-only tagma.")
            .with_callback(move |observer| {
                if let Some(metrics) = owners.metrics() {
                    observer.observe(metrics.relays_configured(), &[]);
                }
            })
            .build();
        let owners = Arc::clone(&self.owners);
        meter
            .i64_observable_gauge("kallipai_tagma.relays_connected")
            .with_description("Relay entries whose tunnel is open right now.")
            .with_callback(move |observer| {
                if let Some(metrics) = owners.metrics() {
                    observer.observe(metrics.relays_connected(), &[]);
                }
            })
            .build();
    }

    /// Which build, which registration: one series, value 1, nothing until the
    /// boot publishes the identity.
    fn identity(&self, meter: &Meter) {
        let owners = Arc::clone(&self.owners);
        meter
            .i64_observable_gauge("kallipai_tagma.build_info")
            .with_description(
                "Build metadata; the value is always 1, the labels carry the version and the registration id.",
            )
            .with_callback(move |observer| {
                let Some(metrics) = owners.metrics() else {
                    return;
                };
                let Some(info) = metrics.build_info() else {
                    return;
                };
                let registration = info
                    .registration
                    .clone()
                    .unwrap_or_else(|| UNENROLLED_TAGMA_ID.to_owned());
                observer.observe(
                    1,
                    &[
                        KeyValue::new("version", info.version.clone()),
                        KeyValue::new("tagma_registration", registration),
                    ],
                );
            })
            .build();
    }

    /// How stale each periodic loop is. The age is derived (now minus the last
    /// beat), so it cannot be a counter; the gauge is what it honestly is.
    fn beats(&self, meter: &Meter) {
        let owners = Arc::clone(&self.owners);
        meter
            .f64_observable_gauge("kallipai_tagma.background_task_age")
            .with_unit("s")
            .with_description("Seconds since each periodic background loop last started a pass.")
            .with_callback(move |observer| {
                if let Some(metrics) = owners.metrics() {
                    for beat in Beat::ALL {
                        observer.observe(
                            metrics.beat_age(beat).as_secs_f64(),
                            &[KeyValue::new("task", beat.as_str())],
                        );
                    }
                }
            })
            .build();
    }

    /// The export pipeline's four families, from the process-wide tally. Every
    /// failure reason is a series from the first collection: an operator has to
    /// be able to tell a zero from a label that never existed.
    fn export(&self, meter: &Meter) {
        meter
            .i64_observable_gauge("kallipai_tagma.telemetry_export_enabled")
            .with_description(
                "1 when an OTLP export layer is installed, 0 when this process keeps its telemetry local.",
            )
            .with_callback(|observer| {
                observer.observe(i64::from(export_snapshot().enabled), &[]);
            })
            .build();
        meter
            .u64_observable_counter("kallipai_tagma.telemetry_exported")
            .with_description("Spans the collector accepted; one count per span, not per batch.")
            .with_callback(|observer| {
                observer.observe(export_snapshot().exported_spans, &[]);
            })
            .build();
        meter
            .u64_observable_counter("kallipai_tagma.telemetry_export_failed")
            .with_description("Failed exports by reason; one count per failed batch.")
            .with_callback(|observer| {
                let export = export_snapshot();
                for (reason, count) in [
                    ("timeout", export.failed_timeout),
                    ("unavailable", export.failed_unavailable),
                    ("unauthorized", export.failed_unauthorized),
                    ("other", export.failed_other),
                ] {
                    observer.observe(count, &[KeyValue::new("reason", reason)]);
                }
            })
            .build();
        meter
            .u64_observable_gauge("kallipai_tagma.telemetry_last_export_timestamp")
            .with_description("Unix seconds of the last accepted export; 0 while none has landed.")
            .with_callback(|observer| {
                observer.observe(export_snapshot().last_success_unix, &[]);
            })
            .build();
    }

    /// The bus's per-topic counters. A listener that lags loses events, so the
    /// miss travels beside the publishes it missed.
    fn bus(&self, meter: &Meter) {
        let owners = Arc::clone(&self.owners);
        meter
            .u64_observable_counter("kallipai_tagma.bus_events")
            .with_description("Events published per topic.")
            .with_callback(move |observer| {
                if let Some(state) = owners.state() {
                    for topic in state.bus.stats() {
                        observer.observe(topic.publishes, &[KeyValue::new("topic", topic.topic)]);
                    }
                }
            })
            .build();
        let owners = Arc::clone(&self.owners);
        meter
            .u64_observable_counter("kallipai_tagma.bus_lagged")
            .with_description("Events a subscriber missed per topic (broadcast lag).")
            .with_callback(move |observer| {
                if let Some(state) = owners.state() {
                    for topic in state.bus.stats() {
                        observer.observe(topic.lagged, &[KeyValue::new("topic", topic.topic)]);
                    }
                }
            })
            .build();
    }

    /// The roster as three counts: live, faulted, off duty. A registry held by
    /// a mutation yields no samples this collection, so the family is missing
    /// rather than reported as an empty tagma.
    fn roster(&self, meter: &Meter) {
        let owners = Arc::clone(&self.owners);
        meter
            .i64_observable_gauge("kallipai_tagma.agents")
            .with_description(
                "Agents by state: live entries, faulted entries, and entries currently off duty.",
            )
            .with_callback(move |observer| {
                let Some(state) = owners.state() else {
                    return;
                };
                let Ok(registry) = state.registry.try_read() else {
                    return;
                };
                let mut live = 0;
                let mut faulted = 0;
                let mut off_duty = 0;
                for (id, entry) in registry.iter() {
                    match entry {
                        RegistryEntry::Live(_) => live += 1,
                        RegistryEntry::Faulted(_) => faulted += 1,
                    }
                    if state.duty.is_off_duty(id) {
                        off_duty += 1;
                    }
                }
                for (label, count) in [("live", live), ("faulted", faulted), ("off_duty", off_duty)]
                {
                    observer.observe(count, &[KeyValue::new("state", label)]);
                }
            })
            .build();
    }
}

#[cfg(test)]
mod tests {

    use opentelemetry_sdk::metrics::SdkMeterProvider;

    use super::*;
    use crate::metrics::PassOutcome;
    use crate::metrics::test_meter::{
        Capture, Recorded, counter_series, gauge, gauge_series, local,
    };

    /// One collection of a metric set, after a forced flush.
    fn collected(capture: &Capture, provider: &SdkMeterProvider) -> Vec<Recorded> {
        provider.force_flush().expect("the reader flushes");
        capture.snapshot()
    }

    fn declares(samples: &[Recorded], name: &str) -> bool {
        samples.iter().any(|sample| match sample {
            Recorded::Counter { name: series, .. }
            | Recorded::Histogram { name: series, .. }
            | Recorded::Gauge { name: series, .. } => series == name,
        })
    }

    #[test]
    fn the_relay_gauges_carry_what_the_metric_set_holds() {
        let (metrics, capture, provider) = local();
        metrics.set_relays_configured(2);
        metrics.relay_up();
        metrics.relay_up();
        metrics.relay_down();
        let samples = collected(&capture, &provider);
        assert_eq!(
            gauge(&samples, "kallipai_tagma.relays_configured", &[]),
            2.0
        );
        assert_eq!(gauge(&samples, "kallipai_tagma.relays_connected", &[]), 1.0);
    }

    #[test]
    fn build_info_reports_one_identity_after_the_boot_names_it() {
        let (metrics, capture, provider) = local();
        let unmounted = collected(&capture, &provider);
        assert!(
            !declares(&unmounted, "kallipai_tagma.build_info"),
            "an identity nobody published is not a zero"
        );

        metrics.set_build_info("1.2.3", None);
        let local_only = collected(&capture, &provider);
        assert_eq!(
            gauge(
                &local_only,
                "kallipai_tagma.build_info",
                &[
                    ("version", "1.2.3"),
                    ("tagma_registration", UNENROLLED_TAGMA_ID)
                ]
            ),
            1.0
        );

        // One process has one identity: a later publication replaces the
        // series rather than leaving a second one claiming the same build.
        metrics.set_build_info("1.2.3", Some("t-9"));
        let enrolled = collected(&capture, &provider);
        assert_eq!(
            gauge(
                &enrolled,
                "kallipai_tagma.build_info",
                &[("version", "1.2.3"), ("tagma_registration", "t-9")]
            ),
            1.0
        );
        let series = enrolled
            .iter()
            .filter(|sample| matches!(sample, Recorded::Gauge { name, .. } if name == "kallipai_tagma.build_info"))
            .count();
        assert_eq!(series, 1, "one build, one series");
    }

    #[test]
    fn every_periodic_loop_reports_its_age_under_its_own_label() {
        let (metrics, capture, provider) = local();
        metrics.beat(Beat::Engine, PassOutcome::Completed);
        let samples = collected(&capture, &provider);
        for beat in Beat::ALL {
            assert!(
                gauge_series(
                    &samples,
                    "kallipai_tagma.background_task_age",
                    &[("task", beat.as_str())]
                )
                .is_some(),
                "{} is a series",
                beat.as_str()
            );
        }
        let engine = gauge(
            &samples,
            "kallipai_tagma.background_task_age",
            &[("task", "engine")],
        );
        assert!(engine < Beat::Engine.max_age().as_secs_f64());
    }

    #[test]
    fn every_export_failure_reason_is_a_series_from_the_first_collection() {
        let (_metrics, capture, provider) = local();
        let samples = collected(&capture, &provider);
        assert_eq!(
            gauge(&samples, "kallipai_tagma.telemetry_export_enabled", &[]),
            0.0,
            "a process with no export layer says so"
        );
        assert!(
            gauge_series(
                &samples,
                "kallipai_tagma.telemetry_last_export_timestamp",
                &[]
            )
            .is_some()
        );
        for reason in ["timeout", "unavailable", "unauthorized", "other"] {
            assert!(
                counter_series(
                    &samples,
                    "kallipai_tagma.telemetry_export_failed",
                    &[("reason", reason)]
                )
                .is_some(),
                "reason {reason} is a series before any failure"
            );
        }
    }

    #[test]
    fn an_attached_state_puts_the_roster_and_the_bus_on_the_export() {
        let (metrics, capture, provider) = local();
        let detached = collected(&capture, &provider);
        assert!(!declares(&detached, "kallipai_tagma.agents"));
        assert!(!declares(&detached, "kallipai_tagma.bus_events"));

        // The callbacks hold the state weakly, so the handle has to outlive
        // the borrow taken here.
        let state = crate::test_helpers::make_state();
        metrics.attach_state(&state);
        let attached = collected(&capture, &provider);
        for roster_state in ["live", "faulted", "off_duty"] {
            assert_eq!(
                gauge(
                    &attached,
                    "kallipai_tagma.agents",
                    &[("state", roster_state)]
                ),
                0.0,
                "a fresh tagma reports {roster_state} as zero, not as a missing series"
            );
        }
        assert!(declares(&attached, "kallipai_tagma.bus_events"));
        assert!(declares(&attached, "kallipai_tagma.bus_lagged"));
    }

    #[test]
    fn a_replaced_state_owner_puts_its_families_back() {
        let (metrics, capture, provider) = local();
        let first = crate::test_helpers::make_state();
        metrics.attach_state(&first);
        assert!(declares(
            &collected(&capture, &provider),
            "kallipai_tagma.agents"
        ));

        drop(first);
        assert!(!declares(
            &collected(&capture, &provider),
            "kallipai_tagma.agents"
        ));

        let second = crate::test_helpers::make_state();
        metrics.attach_state(&second);
        assert!(
            declares(&collected(&capture, &provider), "kallipai_tagma.agents"),
            "the second owner's roster is on the export"
        );
    }

    #[test]
    fn a_metric_set_that_goes_away_stops_reporting() {
        let (metrics, capture, provider) = local();
        metrics.set_relays_configured(1);
        assert!(declares(
            &collected(&capture, &provider),
            "kallipai_tagma.relays_configured"
        ));

        // The callbacks hold the set weakly: a metric set that is gone takes
        // its families with it instead of answering for a dead owner.
        drop(metrics);
        assert!(!declares(
            &collected(&capture, &provider),
            "kallipai_tagma.relays_configured"
        ));
    }
}
