//! The snapshot collector: families whose data another owner already keeps.
//!
//! A value owned elsewhere is encoded here, on each gather, as the Prometheus
//! type it really is. Nothing is mirrored into a gauge and nothing periodic is
//! added: `rate()` works on these counters because they are counters.
//!
//! Sources, one per block in [`Collector::collect`]:
//!
//! - the adk's per-wake / model / tool tallies (in-process atomics, cloned
//!   into every agent context);
//! - the event bus's per-topic publishes and lag (`EventBus::stats`);
//! - the agent roster and duty store (a snapshot of the registry);
//! - the heartbeat table (periodic loops only, see [`super::Beat`]);
//! - the export pipeline's tally (`kallipai_common::telemetry::export_stats`).
//!
//! The collector never blocks a scrape on a long lock: the roster read is
//! `try_read`, and a registry busy with a mutation simply yields no roster
//! samples this scrape (the next one is a second later, and a missing gauge is
//! visible as staleness rather than as a wrong number). The two owner slots are
//! mutexes, but only around the handle: held to clone it, released before a
//! family is built.
//!
//! Each slot is written after construction (the collector is registered before
//! either owner exists) and written again when the owner is replaced, so the
//! families always follow the owner that is alive now.

use std::sync::{Arc, Mutex, MutexGuard, Weak};

use prometheus::core::Collector;
use prometheus::proto;

use super::{Beat, Metrics};
use crate::state::{RegistryEntry, SharedState};

/// What the collector reads from: weak handles to the two owners, both attached
/// after construction (the collector is registered before either exists).
///
/// A slot takes a second value: a process that replaces its metric set or its
/// tagma state re-attaches, and the gather follows the owner that is alive.
#[derive(Default)]
struct Sources {
    metrics: Mutex<Option<Weak<Metrics>>>,
    state: Mutex<Option<Weak<crate::state::AppState>>>,
}

/// Take one slot's lock, ignoring a poisoned flag: the critical section is one
/// assignment or one clone, so nothing here can panic while holding it, and a
/// panic elsewhere must not turn the scrape into an empty one.
fn slot<T>(slot: &Mutex<T>) -> MutexGuard<'_, T> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The collector itself. Cheap to clone; every clone shares the same sources,
/// and the registered copy is the one the registry gathers.
#[derive(Clone, Default)]
pub struct SnapshotCollector {
    sources: Arc<Sources>,
}

impl SnapshotCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Point the collector at the metric set it belongs to, replacing an owner
    /// attached before it.
    pub fn attach(&self, metrics: &Arc<Metrics>) {
        *slot(&self.sources.metrics) = Some(Arc::downgrade(metrics));
    }

    /// Point the collector at the running tagma, replacing an owner attached
    /// before it.
    pub fn attach_state(&self, state: &SharedState) {
        *slot(&self.sources.state) = Some(Arc::downgrade(state));
    }

    fn metrics(&self) -> Option<Arc<Metrics>> {
        let held = slot(&self.sources.metrics);
        held.as_ref()?.upgrade()
    }

    fn state(&self) -> Option<SharedState> {
        let held = slot(&self.sources.state);
        held.as_ref()?.upgrade()
    }
}

impl Collector for SnapshotCollector {
    /// No static descriptors: the families are built per gather from live data,
    /// and a descriptor here would be a second copy of the names to keep in
    /// step. Registration therefore cannot detect a duplicate name, which is
    /// why the names are encoded in one place (the functions that build them),
    /// with the scrape documentation's table mirroring that list row by row.
    fn desc(&self) -> Vec<&prometheus::core::Desc> {
        Vec::new()
    }

    fn collect(&self) -> Vec<proto::MetricFamily> {
        let mut families = Vec::new();
        let metrics = self.metrics();
        if let Some(metrics) = &metrics {
            families.extend(adk_families(metrics));
            families.push(background_family(metrics));
        }
        families.extend(export_families());
        if let Some(state) = self.state() {
            families.extend(bus_families(&state));
            if let Some(roster) = roster_samples(&state) {
                families.push(gauges(
                    "kallipai_tagma_agents",
                    "Agents by state: live entries, faulted entries, and entries currently off duty.",
                    roster,
                ));
            }
        }
        families
    }
}

/// The adk's tallies, one family per counter or histogram it keeps.
fn adk_families(metrics: &Metrics) -> Vec<proto::MetricFamily> {
    let snapshot = metrics.adk().snapshot();
    vec![
        counters(
            "kallipai_tagma_agent_wakes_total",
            "Agent wakes by terminal outcome; one count per finished wake.",
            one_label("outcome", snapshot.wakes_total),
        ),
        histograms(
            "kallipai_tagma_agent_wake_duration_seconds",
            "Agent wake duration, by terminal outcome.",
            histogram_samples("outcome", snapshot.wake_duration),
        ),
        counters(
            "kallipai_tagma_model_requests_total",
            "Model requests by outcome; one count per send attempt.",
            one_label("outcome", snapshot.model_requests_total),
        ),
        histograms(
            "kallipai_tagma_model_request_duration_seconds",
            "Model request duration, from the send that started the attempt to its end, every outcome together.",
            vec![(Vec::new(), snapshot.model_duration)],
        ),
        histograms(
            "kallipai_tagma_model_first_token_seconds",
            "Wait from sending a request to the first streamed delta, for the stream that was adopted.",
            vec![(Vec::new(), snapshot.first_token)],
        ),
        counters(
            "kallipai_tagma_model_tokens_total",
            "Tokens by kind, as the provider reported them.",
            one_label("kind", snapshot.tokens_total),
        ),
        gauges(
            "kallipai_tagma_model_in_flight",
            "Model requests in flight right now.",
            vec![(Vec::new(), snapshot.model_in_flight as f64)],
        ),
        counters(
            "kallipai_tagma_tool_calls_total",
            "Tool calls by outcome; one count per call.",
            one_label("outcome", snapshot.tool_calls_total),
        ),
        histograms(
            "kallipai_tagma_tool_call_duration_seconds",
            "Tool call duration by outcome.",
            histogram_samples("outcome", snapshot.tool_duration),
        ),
    ]
}

/// How stale each periodic loop is. The age is derived (now minus last beat),
/// so it cannot be recorded as a counter; the gauge is what it honestly is.
fn background_family(metrics: &Metrics) -> proto::MetricFamily {
    gauges(
        "kallipai_tagma_background_task_age_seconds",
        "Seconds since each periodic background loop last started a pass.",
        Beat::ALL
            .iter()
            .map(|beat| {
                (
                    vec![("task", beat.as_str())],
                    metrics.beat_age(*beat).as_secs_f64(),
                )
            })
            .collect(),
    )
}

/// The export pipeline's four families, from the process-wide tally.
fn export_families() -> Vec<proto::MetricFamily> {
    let export = kallipai_common::telemetry::export_stats().snapshot();
    vec![
        gauges(
            "kallipai_tagma_telemetry_export_enabled",
            "1 when an OTLP export layer is installed, 0 when this process keeps its spans local.",
            vec![(Vec::new(), u8::from(export.enabled) as f64)],
        ),
        counters(
            "kallipai_tagma_telemetry_exported_total",
            "Spans the collector accepted; one count per span, not per batch.",
            vec![(Vec::new(), export.exported_spans)],
        ),
        counters(
            "kallipai_tagma_telemetry_export_failed_total",
            "Failed exports by reason; one count per failed batch.",
            vec![
                (vec![("reason", "timeout")], export.failed_timeout),
                (vec![("reason", "unavailable")], export.failed_unavailable),
                (vec![("reason", "other")], export.failed_other),
            ],
        ),
        gauges(
            "kallipai_tagma_telemetry_last_export_timestamp",
            "Unix seconds of the last accepted export; 0 while none has landed.",
            vec![(Vec::new(), export.last_success_unix as f64)],
        ),
    ]
}

/// The bus's per-topic counters.
fn bus_families(state: &SharedState) -> Vec<proto::MetricFamily> {
    let stats = state.bus.stats();
    vec![
        counters(
            "kallipai_tagma_bus_events_total",
            "Events published per topic.",
            stats
                .iter()
                .map(|topic| (vec![("topic", topic.topic)], topic.publishes))
                .collect(),
        ),
        counters(
            "kallipai_tagma_bus_lagged_total",
            "Events a subscriber missed per topic (broadcast lag).",
            stats
                .iter()
                .map(|topic| (vec![("topic", topic.topic)], topic.lagged))
                .collect(),
        ),
    ]
}

/// The samples of one gauge family: `(labels, value)` per series, the shape
/// every encoder helper below takes.
type GaugeSamples = (Vec<(&'static str, &'static str)>, f64);

/// The roster, as three counts. `None` when a mutation holds the registry: a
/// scrape skips the family rather than waiting behind it.
fn roster_samples(state: &SharedState) -> Option<Vec<GaugeSamples>> {
    let registry = state.registry.try_read().ok()?;
    let mut live = 0u64;
    let mut faulted = 0u64;
    let mut off_duty = 0u64;
    for (id, entry) in registry.iter() {
        match entry {
            RegistryEntry::Live(_) => live += 1,
            RegistryEntry::Faulted(_) => faulted += 1,
        }
        if state.duty.is_off_duty(id) {
            off_duty += 1;
        }
    }
    Some(vec![
        (vec![("state", "live")], live as f64),
        (vec![("state", "faulted")], faulted as f64),
        (vec![("state", "off_duty")], off_duty as f64),
    ])
}

/// Attach one label name to a list of `(value, count)` pairs.
fn one_label(
    name: &'static str,
    pairs: Vec<(&'static str, u64)>,
) -> Vec<(Vec<(&'static str, &'static str)>, u64)> {
    pairs
        .into_iter()
        .map(|(value, count)| (vec![(name, value)], count))
        .collect()
}

/// Attach one label name to a list of `(value, histogram)` pairs.
fn histogram_samples(
    name: &'static str,
    pairs: Vec<(&'static str, kallipai_adk::metrics::HistogramSnapshot)>,
) -> Vec<(
    Vec<(&'static str, &'static str)>,
    kallipai_adk::metrics::HistogramSnapshot,
)> {
    pairs
        .into_iter()
        .map(|(value, snapshot)| (vec![(name, value)], snapshot))
        .collect()
}

fn empty_family(name: &str, help: &str, kind: proto::MetricType) -> proto::MetricFamily {
    let mut family = proto::MetricFamily::new();
    family.set_name(name.to_owned());
    family.set_help(help.to_owned());
    family.set_field_type(kind);
    family
}

fn metric_with_labels(labels: Vec<(&'static str, &'static str)>) -> proto::Metric {
    let mut metric = proto::Metric::new();
    if !labels.is_empty() {
        metric.set_label(
            labels
                .into_iter()
                .map(|(name, value)| {
                    let mut pair = proto::LabelPair::new();
                    pair.set_name(name.to_owned());
                    pair.set_value(value.to_owned());
                    pair
                })
                .collect(),
        );
    }
    metric
}

fn counters(
    name: &str,
    help: &str,
    samples: Vec<(Vec<(&'static str, &'static str)>, u64)>,
) -> proto::MetricFamily {
    let mut family = empty_family(name, help, proto::MetricType::COUNTER);
    for (labels, value) in samples {
        let mut metric = metric_with_labels(labels);
        let mut counter = proto::Counter::new();
        counter.set_value(value as f64);
        metric.set_counter(counter);
        family.mut_metric().push(metric);
    }
    family
}

fn gauges(
    name: &str,
    help: &str,
    samples: Vec<(Vec<(&'static str, &'static str)>, f64)>,
) -> proto::MetricFamily {
    let mut family = empty_family(name, help, proto::MetricType::GAUGE);
    for (labels, value) in samples {
        let mut metric = metric_with_labels(labels);
        let mut gauge = proto::Gauge::new();
        gauge.set_value(value);
        metric.set_gauge(gauge);
        family.mut_metric().push(metric);
    }
    family
}

fn histograms(
    name: &str,
    help: &str,
    samples: Vec<(
        Vec<(&'static str, &'static str)>,
        kallipai_adk::metrics::HistogramSnapshot,
    )>,
) -> proto::MetricFamily {
    let mut family = empty_family(name, help, proto::MetricType::HISTOGRAM);
    for (labels, snapshot) in samples {
        let mut histogram = proto::Histogram::new();
        histogram.set_sample_count(snapshot.count);
        histogram.set_sample_sum(snapshot.sum_seconds);
        let mut buckets = Vec::with_capacity(snapshot.edges.len());
        for (edge, count) in snapshot.edges.iter().zip(snapshot.cumulative.iter()) {
            let mut bucket = proto::Bucket::new();
            bucket.set_upper_bound(*edge);
            bucket.set_cumulative_count(*count);
            buckets.push(bucket);
        }
        // The +Inf bucket is implicit: the text encoder appends it from the
        // sample count, so building one here would print it twice.
        histogram.set_bucket(buckets);
        let mut metric = metric_with_labels(labels);
        metric.set_histogram(histogram);
        family.mut_metric().push(metric);
    }
    family
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_adk::metrics::HistogramSnapshot;
    use prometheus::proto::MetricType;

    fn histogram() -> HistogramSnapshot {
        HistogramSnapshot {
            edges: vec![1.0, 5.0],
            // One longer than the edges: the trailing entry is the +Inf count.
            cumulative: vec![2, 3, 4],
            sum_seconds: 7.5,
            count: 4,
        }
    }

    fn scrape(metrics: &Metrics) -> String {
        metrics.render().expect("the registry renders")
    }

    fn declares(families: &[proto::MetricFamily], name: &str) -> bool {
        families.iter().any(|family| family.name() == name)
    }

    #[test]
    fn a_monotone_count_is_encoded_as_a_counter() {
        let family = counters(
            "kallipai_tagma_things_total",
            "help",
            one_label("kind", vec![("a", 3)]),
        );
        assert_eq!(family.get_field_type(), MetricType::COUNTER);
        let metric = &family.get_metric()[0];
        let counter = metric.get_counter().as_ref().expect("a counter message");
        assert_eq!(counter.value(), 3.0);
        assert_eq!(metric.get_label()[0].name(), "kind");
        assert_eq!(metric.get_label()[0].value(), "a");
    }

    #[test]
    fn a_derived_value_is_encoded_as_a_gauge() {
        let family = gauges(
            "kallipai_tagma_things",
            "help",
            vec![(vec![("state", "live")], 2.0)],
        );
        assert_eq!(family.get_field_type(), MetricType::GAUGE);
        let gauge = family.get_metric()[0]
            .get_gauge()
            .as_ref()
            .expect("a gauge message");
        assert_eq!(gauge.value(), 2.0);
    }

    #[test]
    fn a_histogram_keeps_only_the_finite_buckets() {
        let family = histograms(
            "kallipai_tagma_thing_seconds",
            "help",
            vec![(Vec::new(), histogram())],
        );
        assert_eq!(family.get_field_type(), MetricType::HISTOGRAM);
        let encoded = family.get_metric()[0]
            .get_histogram()
            .as_ref()
            .expect("a histogram message");
        let buckets = encoded.get_bucket();
        assert_eq!(buckets.len(), 2, "the +Inf bucket is the encoder's to add");
        assert_eq!(buckets[0].upper_bound(), 1.0);
        assert_eq!(buckets[0].cumulative_count(), 2);
        assert_eq!(buckets[1].upper_bound(), 5.0);
        assert_eq!(buckets[1].cumulative_count(), 3);
        assert_eq!(encoded.sample_count(), 4);
        assert_eq!(encoded.sample_sum(), 7.5);
    }

    #[test]
    fn a_family_with_no_samples_is_still_declared() {
        let family = counters("kallipai_tagma_things_total", "help", Vec::new());
        assert_eq!(family.get_field_type(), MetricType::COUNTER);
        assert!(family.get_metric().is_empty());
    }
    /// The scrape face end to end: observations go into the owners, the
    /// registry gathers, and the text a collector reads carries each family
    /// once, under the right type, with the values that were recorded.
    #[test]
    fn a_scrape_renders_each_family_once_with_its_type_and_value() {
        use kallipai_adk::metrics::{ModelOutcome, TokenKind, WakeOutcome};

        let metrics = Metrics::new();
        let adk = metrics.adk();
        adk.record_wake(WakeOutcome::Completed, std::time::Duration::from_secs(2));
        adk.record_model_call(ModelOutcome::Ok, std::time::Duration::from_millis(300));
        adk.record_tokens(TokenKind::Output, 7);

        let text = metrics.render().expect("the registry renders");

        fn declared_type(text: &str, name: &str) -> Option<String> {
            let prefix = format!("# TYPE {name} ");
            text.lines()
                .find(|line| line.starts_with(&prefix))
                .map(|line| line[prefix.len()..].trim().to_string())
        }

        assert_eq!(
            declared_type(&text, "kallipai_tagma_agent_wakes_total").as_deref(),
            Some("counter")
        );
        assert_eq!(
            declared_type(&text, "kallipai_tagma_agent_wake_duration_seconds").as_deref(),
            Some("histogram")
        );
        assert_eq!(
            declared_type(&text, "kallipai_tagma_model_requests_total").as_deref(),
            Some("counter")
        );
        assert!(
            text.contains("kallipai_tagma_agent_wakes_total{outcome=\"completed\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("kallipai_tagma_model_requests_total{outcome=\"ok\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("kallipai_tagma_model_tokens_total{kind=\"output\"} 7"),
            "{text}"
        );
        assert!(
            text.contains(
                "kallipai_tagma_agent_wake_duration_seconds_count{outcome=\"completed\"} 1"
            ),
            "{text}"
        );

        // A name declared twice is a scrape a collector cannot read back into
        // one series, and the registry cannot see it: the families are built
        // per gather and none of them is registered as a descriptor.
        let mut names: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("# TYPE "))
            .filter_map(|rest| rest.split(' ').next())
            .collect();
        let declared = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), declared, "each family name is declared once");
    }

    /// Every block of the collector is wired: a family whose owner lives in
    /// this process appears on its own, and the state-owned ones appear only
    /// once a tagma state is attached.
    #[test]
    fn every_attached_owner_puts_its_families_in_the_scrape() {
        let metrics = Metrics::new();
        let detached = metrics.render().expect("the registry renders");
        for family in [
            "kallipai_tagma_background_task_age_seconds",
            "kallipai_tagma_telemetry_export_enabled",
        ] {
            assert!(
                detached.contains(&format!("# TYPE {family} ")),
                "{family} needs no state and must be there: {detached}"
            );
        }
        // With no state attached the owner is absent, so the families that
        // read through it are absent too rather than silently zero.
        assert!(!detached.contains("kallipai_tagma_agents"));
        assert!(!detached.contains("kallipai_tagma_bus_events_total"));

        // The collector holds the state weakly, so the handle has to outlive
        // the borrow taken here.
        let state = crate::test_helpers::make_state();
        metrics.attach_state(&state);
        let attached = metrics.render().expect("the registry renders");
        for family in [
            "kallipai_tagma_agents",
            "kallipai_tagma_bus_events_total",
            "kallipai_tagma_bus_lagged_total",
        ] {
            assert!(
                attached.contains(&format!("# TYPE {family} ")),
                "{family} is missing from the scrape: {attached}"
            );
        }
    }

    /// A replaced owner is followed. The slot overwrites, so an owner that goes
    /// away takes its families with it and the next one brings them back; a slot
    /// that only took its first value would answer for a dead owner forever.
    #[test]
    fn a_replaced_state_owner_puts_its_families_back() {
        let metrics = Metrics::new();
        let first = crate::test_helpers::make_state();
        metrics.attach_state(&first);
        assert!(scrape(&metrics).contains("kallipai_tagma_agents"));

        drop(first);
        assert!(!scrape(&metrics).contains("kallipai_tagma_agents"));

        let second = crate::test_helpers::make_state();
        metrics.attach_state(&second);
        assert!(
            scrape(&metrics).contains("kallipai_tagma_agents"),
            "the second owner's roster is on the scrape"
        );
    }

    /// The same for the metric set, gathered straight from the collector so
    /// that two owners can exist without two registries.
    #[test]
    fn a_replaced_metric_owner_puts_its_families_back() {
        let collector = SnapshotCollector::new();
        let first = Metrics::new();
        collector.attach(&first);
        assert!(declares(
            &collector.collect(),
            "kallipai_tagma_model_in_flight"
        ));

        drop(first);
        assert!(!declares(
            &collector.collect(),
            "kallipai_tagma_model_in_flight"
        ));

        let second = Metrics::new();
        collector.attach(&second);
        assert!(
            declares(&collector.collect(), "kallipai_tagma_model_in_flight"),
            "the second owner's tallies are on the scrape"
        );
    }
}
