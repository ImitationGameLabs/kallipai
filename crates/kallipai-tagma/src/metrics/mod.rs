//! The tagma's own metrics: what this process records, the heartbeat table
//! `/health` reads, and the callbacks that report what another owner holds.
//!
//! A family is recorded, never scraped: recording goes through the meter the
//! boot installed, and the periodic reader carries every series out.
//!
//! **Label discipline.** A label may carry a closed vocabulary defined in this
//! crate (`route`, `method`, `status_class`, `leg`, `outcome`, `task`,
//! `state`, `kind`, `reason`), a route template taken from `MatchedPath`, or
//! the one-off identity labels of `build_info`. Everything else is out: an
//! agent id, a profile or set name, a model name, a provider id, a tool name,
//! a path, a query string, or any message text. Each of those is either
//! unbounded (one series per value) or an identity, and either way it makes
//! the series unusable. The vocabulary lives in the enums below, so adding a
//! value is a code change and never a data-driven one.
//!
//! **One recording path.** The request path and the adk record into the OTel
//! instruments here; an agent context carries a handle to that sink, so a
//! wake anywhere in the process lands in the one meter. What leaves is the
//! periodic reader's export, and every observation carries the resource the
//! boot published.
//!
//! **A value another owner holds is read, not copied.** The relay gauges, the
//! build identity, the heartbeat ages, the export tally, the bus counters and
//! the roster reach the meter as callbacks, each reading the owner attached at
//! that moment: a monotonic tally stays a counter, so a reader can rate it, and
//! nothing is mirrored between collections.
//!
//! **Model tallies count attempts.** A model counter moves once per send, and
//! the attempt's duration runs from that send to the attempt's end, so a
//! backoff between attempts never lands in it. The wake's own outcome is a
//! separate family.
//!
//! **`/health` is not measured.** `http_timing` exempts it by path: a monitor's
//! cadence says nothing about the process's work, and counting it would let
//! the probe become the load. The router is mounted with
//! `with_server_span_except`, so a health check is not an entry in the traces
//! it sits beside either.
//!
//! `build_info` always carries both labels: an instance with no stored
//! registration reports the unenrolled id, so enrolling it later changes a
//! value, never the shape.
//!
//! Recording never awaits: the request path pays a handful of atomic adds and
//! one small attribute vector per observation. A broken metrics family must
//! not break serving, so nothing here returns an error to a caller.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::state::SharedState;

mod adk_sink;
pub mod health;
mod observable;
pub mod routes;

pub use health::{HealthReport, HealthStatus, RelayState};

/// HTTP request-duration buckets, in seconds. An API call is a local round
/// trip, so the useful resolution is milliseconds; the tail covers a large
/// listing or an export.
const HTTP_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Outbound call-duration buckets, in seconds. Every leg crosses the network,
/// so nothing below ten milliseconds is worth resolving; a slow leg is a slow
/// platform, which is the thing worth seeing.
const OUTBOUND_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0];

/// The meter every instrument here is created from: one scope name for the
/// whole crate, which a collector reports as the instrumentation scope.
pub(crate) const METER_NAME: &str = "kallipai-tagma";

/// The `route` label of a request that matched no route template. Same
/// convention as the request span (`kallipai_common::telemetry::server`): the
/// fallback is one fixed word, never the raw path.
pub const UNMATCHED_ROUTE: &str = "unmatched";

/// A background loop whose freshness `/health` and
/// `background_task_age_seconds` report.
///
/// Only loops that tick on a fixed schedule belong here. An event-driven pump
/// is legitimately silent on an idle tagma, so its last activity says nothing
/// about whether it is alive; judging it would turn "quiet" into "degraded" and
/// teach an operator to ignore the endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Beat {
    /// The chat-history GC sweep in `run`: one pass every 600 s.
    HistoryGc,
    /// The work-schedule engine's wall-clock tick: every 60 s. The tick arm
    /// runs even with no schedule and no cached deadline.
    Engine,
}

/// How one pass of a periodic loop ended. A loop that ran and failed is not
/// the same fact as a loop that stopped: the first one is degraded, the second
/// one is late.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PassOutcome {
    /// The pass ran to its end.
    Completed,
    /// The pass ran and reported a failure; `/health` is degraded until a later
    /// pass succeeds.
    Failed,
}

impl Beat {
    /// Every beat, in the order the label values are reported.
    pub const ALL: [Beat; 2] = [Beat::HistoryGc, Beat::Engine];

    pub fn as_str(self) -> &'static str {
        match self {
            Beat::HistoryGc => "history_gc",
            Beat::Engine => "engine",
        }
    }

    fn index(self) -> usize {
        match self {
            Beat::HistoryGc => 0,
            Beat::Engine => 1,
        }
    }

    /// How stale this loop may get before `/health` calls it late: three missed
    /// periods, so one slow pass (a GC sweep over a large store) is not a
    /// degradation.
    pub fn max_age(self) -> Duration {
        match self {
            Beat::HistoryGc => Duration::from_secs(1800),
            Beat::Engine => Duration::from_secs(180),
        }
    }
}

/// An outbound call site: which leg of the tagma's platform traffic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Leg {
    /// Attachment upload and download against the files service.
    Files,
    /// A profile refresh against the model gateway.
    Profiles,
    /// A lesche client call: a listing, a frame post, or opening a stream. A
    /// tunnel that then stays open is not a call -- `relays_connected` and
    /// `relay_reconnects` describe it, and a duration for its lifetime
    /// would measure uptime rather than an answer.
    ///
    /// One observed call is one caller-level attempt: a client's own retry
    /// loop stays inside it (the envelope POST rides out 503s for about half a
    /// minute), so that one call's duration can be the loop's total.
    Lesche,
    /// An archeion client call (enrollment, management).
    Archeion,
}

impl Leg {
    pub fn as_str(self) -> &'static str {
        match self {
            Leg::Files => "files",
            Leg::Profiles => "profiles",
            Leg::Lesche => "lesche",
            Leg::Archeion => "archeion",
        }
    }
}

/// What one outbound call ended as: the `outcome` label.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegOutcome {
    Ok,
    Error,
    Timeout,
}

impl LegOutcome {
    fn as_str(self) -> &'static str {
        match self {
            LegOutcome::Ok => "ok",
            LegOutcome::Error => "error",
            LegOutcome::Timeout => "timeout",
        }
    }
}

/// Why an inbound relay frame was discarded. A closed set: every value names a
/// disagreement -- keys, shape, or vocabulary -- whose non-zero count is itself
/// the signal. Drops that are normal protocol behaviour (a replay after a
/// client retry, an envelope arriving before the key exchange) are not counted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropReason {
    /// The session key did not authenticate the ciphertext.
    Decrypt,
    /// The payload authenticated but did not parse into what this build serves.
    Decode,
    /// The payload parsed, and its `op` is not one this build serves.
    UnknownOp,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Decrypt => "decrypt",
            DropReason::Decode => "decode",
            DropReason::UnknownOp => "unknown_op",
        }
    }
}

/// Heartbeat table: process uptime (millis) of each loop's last pass, 0 when it
/// has not beaten yet, and whether that pass failed. `started` turns a stored
/// value into an age, and makes a loop that never beat report the process age --
/// a loop dead from boot shows up once it is past its window instead of looking
/// fresh forever.
struct BeatTable {
    started: Instant,
    at_ms: [AtomicU64; Beat::ALL.len()],
    failed: [AtomicBool; Beat::ALL.len()],
}

impl BeatTable {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            at_ms: std::array::from_fn(|_| AtomicU64::new(0)),
            failed: std::array::from_fn(|_| AtomicBool::new(false)),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn beat(&self, task: Beat, outcome: PassOutcome) {
        self.at_ms[task.index()].store(self.elapsed_ms(), Ordering::Relaxed);
        self.failed[task.index()].store(outcome == PassOutcome::Failed, Ordering::Relaxed);
    }
    /// Record that the loop is alive without judging a pass: a tick that only
    /// compared a cached deadline has not run one.
    fn touch(&self, task: Beat) {
        self.at_ms[task.index()].store(self.elapsed_ms(), Ordering::Relaxed);
    }

    fn failed(&self, task: Beat) -> bool {
        self.failed[task.index()].load(Ordering::Relaxed)
    }

    fn age(&self, task: Beat) -> Duration {
        let now = self.elapsed_ms();
        let at = self.at_ms[task.index()].load(Ordering::Relaxed);
        let ms = if at == 0 { now } else { now.saturating_sub(at) };
        Duration::from_millis(ms)
    }
}

/// The process's metrics: the instruments this crate records into, the
/// callbacks that report what another owner holds, the heartbeat table, and
/// the adk handle every agent context carries.
pub struct Metrics {
    http_requests: Counter<u64>,
    http_duration: Histogram<f64>,
    relays_configured: AtomicI64,
    relays_connected: AtomicI64,
    relay_reconnects: Counter<u64>,
    relay_dropped: Counter<u64>,
    outbound_requests: Counter<u64>,
    outbound_duration: Histogram<f64>,
    build_info: Mutex<Option<observable::BuildInfo>>,
    beats: BeatTable,
    adk: Arc<adk_sink::OtelAdkSink>,
    observable: observable::Observable,
}

impl Metrics {
    /// Build the metric set: the instruments the request path records into,
    /// and the callbacks for the families another owner holds.
    /// The instruments come from the process meter, which the boot's provider
    /// publishes; a process with no provider keeps the SDK's no-op meter and
    /// records into nothing.
    pub fn new() -> Arc<Self> {
        #[cfg(test)]
        test_meter::installed();
        Self::with_meter(&opentelemetry::global::meter(METER_NAME))
    }

    /// [`Metrics::new`] over a meter of the caller's own, so a test can read
    /// what it recorded without touching the process meter.
    pub fn with_meter(meter: &Meter) -> Arc<Self> {
        let http_requests = meter
            .u64_counter("kallipai_tagma.http_requests")
            .with_description(
                "API requests by route template, method and status class; one count per response.",
            )
            .build();
        let http_duration = meter
            .f64_histogram("kallipai_tagma.http_request_duration")
            .with_unit("s")
            .with_description(
                "API request duration (received to response finished) by route template.",
            )
            .with_boundaries(HTTP_BUCKETS.to_vec())
            .build();
        let relay_reconnects = meter
            .u64_counter("kallipai_tagma.relay_reconnects")
            .with_description("Relay tunnel sessions that ended and were reopened.")
            .build();
        let relay_dropped = meter
            .u64_counter("kallipai_tagma.relay_dropped")
            .with_description(
                "Inbound 1:1 relay frames the tagma received and discarded, by cause. A count here means the two ends disagree; drops that are normal protocol behaviour are not counted.",
            )
            .build();
        let outbound_requests = meter
            .u64_counter("kallipai_tagma.outbound_requests")
            .with_description(
                "Outbound platform calls by leg and outcome; one count per call. A stream that stays open is not a call: relays_connected and relay_reconnects carry the tunnel.",
            )
            .build();
        let outbound_duration = meter
            .f64_histogram("kallipai_tagma.outbound_request_duration")
            .with_unit("s")
            .with_description("Outbound platform call duration by leg.")
            .with_boundaries(OUTBOUND_BUCKETS.to_vec())
            .build();
        let observable = observable::Observable::register(meter);
        let metrics = Arc::new(Self {
            http_requests,
            http_duration,
            relays_configured: AtomicI64::new(0),
            relays_connected: AtomicI64::new(0),
            relay_reconnects,
            relay_dropped,
            outbound_requests,
            outbound_duration,
            build_info: Mutex::new(None),
            beats: BeatTable::new(),
            adk: adk_sink::OtelAdkSink::new(meter),
            observable,
        });
        metrics.observable.attach(&metrics);
        metrics
    }

    /// The adk handle for one agent context: the same sink every agent in this
    /// process records into, so a wake anywhere reaches the one meter.
    pub fn adk(&self) -> kallipai_adk::metrics::AdkMetrics {
        kallipai_adk::metrics::AdkMetrics::with_sink(self.adk.clone())
    }

    /// Point the callbacks at the running tagma, for the families they read
    /// reads out of live state (the roster, the bus). Called once, after the
    /// state exists; before that those families report nothing.
    pub fn attach_state(&self, state: &SharedState) {
        self.observable.attach_state(state);
    }

    /// One finished API request. `route` is the matched template or
    /// [`UNMATCHED_ROUTE`]; `status` is the response status.
    pub fn record_http(&self, route: &str, method: &str, status: u16, duration: Duration) {
        let status_class = match status {
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            500..=599 => "5xx",
            _ => "other",
        };
        self.http_requests.add(
            1,
            &[
                KeyValue::new("route", route.to_string()),
                KeyValue::new("method", method.to_string()),
                KeyValue::new("status_class", status_class),
            ],
        );
        self.http_duration.record(
            duration.as_secs_f64(),
            &[KeyValue::new("route", route.to_string())],
        );
    }

    /// One pass of a periodic loop, with how it ended: a pass that then takes
    /// minutes still reads as fresh until the next one was due, and one that
    /// failed keeps `/health` degraded until a later pass reports success.
    pub fn beat(&self, task: Beat, outcome: PassOutcome) {
        self.beats.beat(task, outcome);
    }
    /// Record that the loop is alive without touching its pass verdict: a tick
    /// that only compared a cached deadline must not clear what the last real
    /// pass reported.
    pub fn beat_alive(&self, task: Beat) {
        self.beats.touch(task);
    }

    /// Whether the loop's last pass failed. Freshness and this are separate
    /// questions: a loop can be on time and still be failing.
    pub fn beat_failed(&self, task: Beat) -> bool {
        self.beats.failed(task)
    }

    /// How long ago the loop last beat; a loop that never beat reports the
    /// process age.
    pub fn beat_age(&self, task: Beat) -> Duration {
        self.beats.age(task)
    }

    /// Whether a periodic loop is within [`Beat::max_age`].
    pub fn beat_fresh(&self, task: Beat) -> bool {
        self.beats.age(task) <= task.max_age()
    }

    /// How many relay entries the operator configured: 0 is a local-only tagma.
    pub fn relays_configured(&self) -> i64 {
        self.relays_configured.load(Ordering::Relaxed)
    }

    /// How many relay tunnels are open right now.
    pub fn relays_connected(&self) -> i64 {
        self.relays_connected.load(Ordering::Relaxed)
    }

    /// The operator's relay entries, counted once at boot.
    pub fn set_relays_configured(&self, configured: i64) {
        self.relays_configured.store(configured, Ordering::Relaxed);
    }

    /// A tunnel session ended and the reconnect loop is opening the next one.
    /// A counter, not the connected gauge: a flapping tunnel reads as a growing
    /// rate, which is what it is.
    pub fn relay_reconnected(&self) {
        self.relay_reconnects.add(1, &[]);
    }

    /// A tunnel came up.
    pub fn relay_up(&self) {
        self.relays_connected.fetch_add(1, Ordering::Relaxed);
    }

    /// A tunnel went down: stream ended, error, or shutdown.
    pub fn relay_down(&self) {
        self.relays_connected.fetch_sub(1, Ordering::Relaxed);
    }

    /// Count an inbound frame this tagma discarded, by cause. See [`DropReason`]
    /// for the closed set and for which drops stay uncounted.
    pub fn relay_dropped(&self, reason: DropReason) {
        self.relay_dropped
            .add(1, &[KeyValue::new("reason", reason.as_str())]);
    }

    /// The one-off identity: which build, which registration. Called once at
    /// boot, and a later call replaces it, one process having one identity.
    /// The registration id gets a label of its own because `tagma_id` in the
    /// stores carries the door's slug; two meanings under one name merge into
    /// a single joined value the moment an instance's slug and its
    /// registration differ. `registration` is `None` on a local-only tagma,
    /// and the label says so rather than pretending an identity.
    pub fn set_build_info(&self, version: &str, registration: Option<&str>) {
        let info = observable::BuildInfo {
            version: version.to_owned(),
            registration: registration.map(str::to_owned),
        };
        *self
            .build_info
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(info);
    }

    /// The identity `build_info` reports, or `None` before the boot names it.
    pub(in crate::metrics) fn build_info(&self) -> Option<observable::BuildInfo> {
        self.build_info
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Time one outbound platform call and count its outcome, returning the
    /// call's own result untouched. Wrapping is the whole contract: a leg that
    /// fails still fails the caller's way, and a timeout is labelled rather
    /// than retried here.
    ///
    /// The timeout split is read off the error's `Display` text: every client on
    /// these legs (reqwest, the archeion and lesche clients) reports a deadline
    /// that way, and the alternative -- a typed predicate across four unrelated
    /// error types -- would put metric plumbing into each client's error enum.
    pub async fn observe_outbound<T, E: std::fmt::Display>(
        &self,
        leg: Leg,
        call: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let started = Instant::now();
        let result = call.await;
        let outcome = match &result {
            Ok(_) => LegOutcome::Ok,
            Err(error) if looks_like_timeout(error) => LegOutcome::Timeout,
            Err(_) => LegOutcome::Error,
        };
        self.outbound_requests.add(
            1,
            &[
                KeyValue::new("leg", leg.as_str()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
        self.outbound_duration.record(
            started.elapsed().as_secs_f64(),
            &[KeyValue::new("leg", leg.as_str())],
        );
        result
    }
}

/// Whether an outbound error reads as a deadline. Case-insensitive substring
/// match on the rendered error; see [`Metrics::observe_outbound`] for why it is
/// text and not a type.
fn looks_like_timeout(error: &impl std::fmt::Display) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    ["timed out", "timeout", "deadline"]
        .iter()
        .any(|needle| message.contains(needle))
}

/// One counter's value as the exporter last saw it. Tests that assert an
/// increment at a call site (rather than inside [`Metrics::observe_outbound`])
/// read what a collector would receive.
#[cfg(test)]
pub(crate) fn counter_value(name: &str, attributes: &[(&str, &str)]) -> u64 {
    test_meter::counter_value(name, attributes)
}

/// The process's metrics.
///
/// One set per process, like the export pipeline: it describes the single
/// `app`. A test that wants isolation builds [`Metrics::new`] over a meter of
/// its own; a test that just serves the router takes the shared one, whose
/// meter is the process-wide test capture.
pub fn metrics() -> &'static Metrics {
    static GLOBAL: OnceLock<Arc<Metrics>> = OnceLock::new();
    GLOBAL.get_or_init(Metrics::new)
}

/// Records what the OTel instruments report, for tests.
///
/// A reader hands every collection to a capture, so an assertion reads the
/// series an exporter sees rather than a rendering of them. The shared
/// [`metrics`] instance takes the process-wide capture; a test that wants
/// isolation builds its own.
#[cfg(test)]
pub(crate) mod test_meter {
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use opentelemetry::KeyValue;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
    use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};

    /// One series as a collection left it.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Recorded {
        Counter {
            name: String,
            attributes: Vec<(String, String)>,
            value: u64,
        },
        Histogram {
            name: String,
            attributes: Vec<(String, String)>,
            count: u64,
            bounds: Vec<f64>,
        },
        /// A signed sum or a gauge: an up-down counter exports as a gauge, and
        /// a callback-registered gauge arrives the same way.
        Gauge {
            name: String,
            attributes: Vec<(String, String)>,
            value: f64,
        },
    }

    /// The last collection, flattened to the series a test asserts on.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct Capture(Arc<Mutex<Vec<Recorded>>>);

    impl Capture {
        pub(crate) fn snapshot(&self) -> Vec<Recorded> {
            self.0.lock().expect("the capture is not poisoned").clone()
        }

        fn store(&self, batch: &ResourceMetrics) {
            let mut seen = self.0.lock().expect("the capture is not poisoned");
            seen.clear();
            for scope in batch.scope_metrics() {
                for metric in scope.metrics() {
                    let name = metric.name().to_string();
                    match metric.data() {
                        AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                            for point in sum.data_points() {
                                seen.push(Recorded::Counter {
                                    name: name.clone(),
                                    attributes: pairs(point.attributes()),
                                    value: point.value(),
                                });
                            }
                        }
                        AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                            for point in histogram.data_points() {
                                seen.push(Recorded::Histogram {
                                    name: name.clone(),
                                    attributes: pairs(point.attributes()),
                                    count: point.count(),
                                    bounds: point.bounds().collect(),
                                });
                            }
                        }
                        AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                            for point in sum.data_points() {
                                seen.push(Recorded::Gauge {
                                    name: name.clone(),
                                    attributes: pairs(point.attributes()),
                                    value: point.value() as f64,
                                });
                            }
                        }
                        AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                            for point in gauge.data_points() {
                                seen.push(Recorded::Gauge {
                                    name: name.clone(),
                                    attributes: pairs(point.attributes()),
                                    value: point.value() as f64,
                                });
                            }
                        }
                        AggregatedMetrics::I64(MetricData::Gauge(gauge)) => {
                            for point in gauge.data_points() {
                                seen.push(Recorded::Gauge {
                                    name: name.clone(),
                                    attributes: pairs(point.attributes()),
                                    value: point.value() as f64,
                                });
                            }
                        }
                        AggregatedMetrics::F64(MetricData::Gauge(gauge)) => {
                            for point in gauge.data_points() {
                                seen.push(Recorded::Gauge {
                                    name: name.clone(),
                                    attributes: pairs(point.attributes()),
                                    value: point.value(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn pairs<'a>(attributes: impl Iterator<Item = &'a KeyValue>) -> Vec<(String, String)> {
        attributes
            .map(|pair| (pair.key.to_string(), pair.value.to_string()))
            .collect()
    }

    impl PushMetricExporter for Capture {
        async fn export(&self, batch: &ResourceMetrics) -> OTelSdkResult {
            self.store(batch);
            Ok(())
        }

        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            Ok(())
        }

        fn temporality(&self) -> Temporality {
            Temporality::Cumulative
        }
    }

    static INSTALLED: OnceLock<(Capture, SdkMeterProvider)> = OnceLock::new();

    /// The process-wide capture, installed as the meter provider the first
    /// time a test needs one. Tests that assert on what the shared
    /// [`super::metrics`] instance recorded read from here.
    pub(crate) fn installed() -> &'static (Capture, SdkMeterProvider) {
        INSTALLED.get_or_init(|| {
            let capture = Capture::default();
            let reader = PeriodicReader::builder(capture.clone())
                .with_interval(Duration::from_secs(3600))
                .build();
            let provider = SdkMeterProvider::builder().with_reader(reader).build();
            opentelemetry::global::set_meter_provider(provider.clone());
            (capture, provider)
        })
    }

    pub(crate) fn counter_value(name: &str, attributes: &[(&str, &str)]) -> u64 {
        let (capture, provider) = installed();
        provider.force_flush().expect("the test reader flushes");
        counter(&capture.snapshot(), name, attributes)
    }

    /// The counter series carrying exactly these attributes, if it exists.
    pub(crate) fn counter_series(
        samples: &[Recorded],
        name: &str,
        attributes: &[(&str, &str)],
    ) -> Option<u64> {
        samples.iter().find_map(|sample| match sample {
            Recorded::Counter {
                name: series,
                attributes: seen,
                value,
            } if series == name && carries(seen, attributes) => Some(*value),
            _ => None,
        })
    }

    /// The value of the counter series carrying exactly these attributes.
    pub(crate) fn counter(samples: &[Recorded], name: &str, attributes: &[(&str, &str)]) -> u64 {
        counter_series(samples, name, attributes).unwrap_or(0)
    }

    /// The value of every counter series carrying this one attribute, summed.
    pub(crate) fn counter_matching(name: &str, attribute: (&str, &str)) -> u64 {
        let (capture, provider) = installed();
        provider.force_flush().expect("the test reader flushes");
        capture
            .snapshot()
            .iter()
            .filter_map(|sample| match sample {
                Recorded::Counter {
                    name: series,
                    attributes,
                    value,
                } if series == name
                    && attributes
                        .iter()
                        .any(|(k, v)| k == attribute.0 && v == attribute.1) =>
                {
                    Some(*value)
                }
                _ => None,
            })
            .sum()
    }

    /// The gauge series carrying exactly these attributes, if it exists.
    pub(crate) fn gauge_series(
        samples: &[Recorded],
        name: &str,
        attributes: &[(&str, &str)],
    ) -> Option<f64> {
        samples.iter().find_map(|sample| match sample {
            Recorded::Gauge {
                name: series,
                attributes: seen,
                value,
            } if series == name && carries(seen, attributes) => Some(*value),
            _ => None,
        })
    }

    /// The value of the gauge series carrying exactly these attributes.
    pub(crate) fn gauge(samples: &[Recorded], name: &str, attributes: &[(&str, &str)]) -> f64 {
        gauge_series(samples, name, attributes).unwrap_or(0.0)
    }

    /// A metric set over a reader of its own, with the capture that receives
    /// what it records: a test reads its own series without touching the
    /// process-wide ones.
    pub(crate) fn local() -> (Arc<super::Metrics>, Capture, SdkMeterProvider) {
        use opentelemetry::metrics::MeterProvider as _;

        let capture = Capture::default();
        let reader = PeriodicReader::builder(capture.clone())
            .with_interval(Duration::from_secs(3600))
            .build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let metrics = super::Metrics::with_meter(&provider.meter(super::METER_NAME));
        (metrics, capture, provider)
    }

    /// How many observations the histogram series carries.
    pub(crate) fn histogram_count(samples: &[Recorded], name: &str) -> u64 {
        samples
            .iter()
            .filter_map(|sample| match sample {
                Recorded::Histogram {
                    name: series,
                    count,
                    ..
                } if series == name => Some(*count),
                _ => None,
            })
            .sum()
    }

    /// The bucket boundaries the histogram series travels with.
    pub(crate) fn histogram_bounds(samples: &[Recorded], name: &str) -> Option<Vec<f64>> {
        samples.iter().find_map(|sample| match sample {
            Recorded::Histogram {
                name: series,
                bounds,
                ..
            } if series == name => Some(bounds.clone()),
            _ => None,
        })
    }

    fn carries(seen: &[(String, String)], wanted: &[(&str, &str)]) -> bool {
        seen.len() == wanted.len()
            && wanted
                .iter()
                .all(|(key, value)| seen.iter().any(|(k, v)| k == key && v == value))
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

    use super::test_meter::{Capture, Recorded, counter, histogram_bounds, histogram_count};
    use super::*;

    /// A meter whose reader keeps what was recorded: a flush hands the
    /// aggregated data to the capture, and the assertions read it the way a
    /// collector would.
    fn test_metrics() -> (Arc<Metrics>, Capture, SdkMeterProvider) {
        let capture = Capture::default();
        let reader = PeriodicReader::builder(capture.clone())
            .with_interval(Duration::from_secs(3600))
            .build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let metrics = Metrics::with_meter(&provider.meter(METER_NAME));
        (metrics, capture, provider)
    }

    /// What the instruments recorded, after a forced flush.
    fn recorded(capture: &Capture, provider: &SdkMeterProvider) -> Vec<Recorded> {
        provider.force_flush().expect("the reader flushes");
        capture.snapshot()
    }

    /// A request reaches the counter with its status class, and the histogram
    /// carries the buckets this crate declares.
    #[test]
    fn a_request_lands_in_the_counter_with_its_status_class() {
        let (metrics, capture, provider) = test_metrics();
        metrics.record_http("/tasks", "POST", 503, Duration::from_millis(20));
        let samples = recorded(&capture, &provider);
        assert_eq!(
            counter(
                &samples,
                "kallipai_tagma.http_requests",
                &[
                    ("route", "/tasks"),
                    ("method", "POST"),
                    ("status_class", "5xx")
                ]
            ),
            1
        );
        assert_eq!(
            histogram_count(&samples, "kallipai_tagma.http_request_duration"),
            1,
            "one observation per request"
        );
        assert_eq!(
            histogram_bounds(&samples, "kallipai_tagma.http_request_duration").as_deref(),
            Some(HTTP_BUCKETS),
            "the declared buckets travel with the instrument"
        );
    }

    #[test]
    fn a_request_that_matched_no_route_is_its_own_series() {
        let (metrics, capture, provider) = test_metrics();
        metrics.record_http(UNMATCHED_ROUTE, "GET", 404, Duration::from_millis(1));
        let samples = recorded(&capture, &provider);
        assert_eq!(
            counter(
                &samples,
                "kallipai_tagma.http_requests",
                &[
                    ("route", UNMATCHED_ROUTE),
                    ("method", "GET"),
                    ("status_class", "4xx")
                ]
            ),
            1
        );
    }

    #[tokio::test]
    async fn each_outbound_outcome_is_counted_under_its_leg() {
        let (metrics, capture, provider) = test_metrics();
        let _ = metrics
            .observe_outbound(Leg::Files, async { Err::<(), &'static str>("boom") })
            .await;
        let _ = metrics
            .observe_outbound(Leg::Lesche, async { Ok::<(), &'static str>(()) })
            .await;
        let _ = metrics
            .observe_outbound(Leg::Archeion, async {
                Err::<(), &'static str>("read timed out")
            })
            .await;
        let samples = recorded(&capture, &provider);
        assert_eq!(
            counter(
                &samples,
                "kallipai_tagma.outbound_requests",
                &[("leg", "files"), ("outcome", "error")]
            ),
            1
        );
        assert_eq!(
            counter(
                &samples,
                "kallipai_tagma.outbound_requests",
                &[("leg", "lesche"), ("outcome", "ok")]
            ),
            1
        );
        assert_eq!(
            counter(
                &samples,
                "kallipai_tagma.outbound_requests",
                &[("leg", "archeion"), ("outcome", "timeout")]
            ),
            1
        );
        assert_eq!(
            histogram_count(&samples, "kallipai_tagma.outbound_request_duration"),
            3,
            "one duration per observed call"
        );
    }

    #[test]
    fn a_beat_makes_its_loop_fresh_and_an_unbeaten_one_reports_the_process_age() {
        let (metrics, _capture, _provider) = test_metrics();
        metrics.beat(Beat::Engine, PassOutcome::Completed);
        assert!(metrics.beat_fresh(Beat::Engine));
        assert!(metrics.beat_age(Beat::Engine) < Beat::Engine.max_age());
        assert!(metrics.beat_fresh(Beat::HistoryGc));
        // An unbeaten loop ages from the collector's birth, not from zero.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(metrics.beat_age(Beat::HistoryGc) >= std::time::Duration::from_millis(5));
    }

    /// A tick is not a pass: it says the loop is alive and leaves the verdict
    /// alone, so a ticking but failing loop still reports the failure.
    #[test]
    fn a_tick_refreshes_freshness_and_keeps_the_failed_verdict() {
        let (metrics, _capture, _provider) = test_metrics();
        metrics.beat(Beat::HistoryGc, PassOutcome::Failed);
        assert!(metrics.beat_failed(Beat::HistoryGc));

        metrics.beat_alive(Beat::HistoryGc);
        assert!(
            metrics.beat_failed(Beat::HistoryGc),
            "a tick must not clear the failure a real pass reported"
        );
        assert!(metrics.beat_fresh(Beat::HistoryGc));

        metrics.beat(Beat::HistoryGc, PassOutcome::Completed);
        assert!(!metrics.beat_failed(Beat::HistoryGc));
    }

    /// The needle list is the whole timeout classifier: a rendered deadline
    /// from any of these clients lands in `timeout`, and a refusal does not.
    #[test]
    fn a_rendered_deadline_reads_as_a_timeout_and_a_refusal_does_not() {
        for deadline in ["read timed out", "request timeout", "deadline exceeded"] {
            assert!(looks_like_timeout(&deadline), "{deadline} is a deadline");
        }
        assert!(!looks_like_timeout(&"connection reset by peer"));
        assert!(!looks_like_timeout(&"400 Bad Request: model not found"));
    }
}
