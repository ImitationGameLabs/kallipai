//! The tagma's own metrics: the families this process records directly, the
//! heartbeat table `/health` reads, and the registry `/metrics` renders.
//!
//! Source names follow OTel semantics as `kallipai_tagma.<domain>.<name>`; the
//! form a scrape sees is `kallipai_tagma_<domain>_<name>`. Only the scrape form
//! appears in code, because that is the name an operator greps for.
//!
//! **Label discipline.** A label may carry a closed vocabulary defined in this
//! crate (`route`, `method`, `status_class`, `leg`, `outcome`, `task`,
//! `state`, `kind`, `reason`), a route template taken from `MatchedPath`, or
//! the one-off identity labels of `build_info`. Everything else is out: an
//! agent id, a profile or set name, a model name, a provider id, a tool name,
//! a path, a query string, or any message text. Each of those is either
//! unbounded (one series per value) or an identity, and either way it makes a
//! scrape unusable. The vocabulary lives in the enums below, so adding a value
//! is a code change and never a data-driven one.
//!
//! **Who encodes what.** Families this module owns are recorded through
//! `prometheus` collectors registered here. Families whose data belongs to
//! another owner -- the adk's wake/model/tool tallies, the event bus's
//! per-topic counters, the agent roster, the export pipeline's tally -- are
//! encoded by `snapshot::SnapshotCollector` on each gather. That is what keeps
//! a monotonic value a real counter: `rate()` works on it, and nothing has to
//! be mirrored into a gauge.
//!
//!
//! **Model tallies count attempts.** A model counter moves once per send, and
//! the attempt's duration runs from that send to the attempt's end, so a
//! backoff between attempts never lands in it. The wake's own outcome is a
//! separate family.
//!
//! **The scrape face is not counted.** `http_timing` exempts `/metrics` and
//! `/health` by path: a scrape measures the process, so counting scrapes would
//! let a scraper's own cadence dominate `http_requests_total`. The exemption is
//! by path and not by peer, so a refused non-loopback scrape leaves no series
//! either; a refusal's only trace is the debug line in `routes::peer_gate`.
//!
//! Neither path enters a trace either: the router is mounted with
//! `with_server_span_except`, so a scrape is not an entry in the traces it
//! sits beside, and its latency is not read as server work.
//!
//! `build_info` always carries both labels: an instance with no stored tagma id
//! reports `tagma_id="none"`, so enrolling it later changes a value, never the shape.
//! Recording never allocates beyond a fixed label vector and never awaits: the
//! request path pays a handful of atomic adds. A broken metrics family must not
//! break serving, so nothing here returns an error to a caller.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
    TextEncoder,
};

use crate::state::SharedState;

pub mod health;
pub mod routes;
mod snapshot;

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
    /// `relay_reconnects_total` describe it, and a duration for its lifetime
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

/// The process's metrics: the registry, the collectors this crate records
/// directly, the heartbeat table, and the adk handle every agent context
/// carries.
pub struct Metrics {
    registry: Registry,
    http_requests: IntCounterVec,
    http_duration: HistogramVec,
    relays_configured: IntGauge,
    relays_connected: IntGauge,
    relay_reconnects: IntCounter,
    relay_dropped: IntCounterVec,
    outbound_requests: IntCounterVec,
    outbound_duration: HistogramVec,
    build_info: IntGaugeVec,
    beats: BeatTable,
    adk: kallipai_adk::metrics::AdkMetrics,
    collector: snapshot::SnapshotCollector,
}

impl Metrics {
    /// Build the metric set on a private registry and register every collector
    /// into it: the ones recorded here, and the snapshot collector that encodes
    /// the data other modules own.
    ///
    /// A name collision is a programming error (two collectors claiming one
    /// family), so it panics at construction, exactly as the gateway's
    /// `.expect("a statically named counter")` does.
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();
        let http_requests = IntCounterVec::new(
            Opts::new(
                "kallipai_tagma_http_requests_total",
                "API requests by route template, method and status class; one count per response.",
            ),
            &["route", "method", "status_class"],
        )
        .expect("a statically named counter");
        let http_duration = HistogramVec::new(
            HistogramOpts::new(
                "kallipai_tagma_http_request_duration_seconds",
                "API request duration (received to response finished) by route template.",
            )
            .buckets(HTTP_BUCKETS.to_vec()),
            &["route"],
        )
        .expect("a statically named histogram");
        let relays_configured = IntGauge::new(
            "kallipai_tagma_relays_configured",
            "Relay entries the operator configured; 0 is a local-only tagma.",
        )
        .expect("a statically named gauge");
        let relays_connected = IntGauge::new(
            "kallipai_tagma_relays_connected",
            "Relay entries whose tunnel is open right now.",
        )
        .expect("a statically named gauge");
        let relay_reconnects = IntCounter::new(
            "kallipai_tagma_relay_reconnects_total",
            "Relay tunnel sessions that ended and were reopened.",
        )
        .expect("a statically named counter");
        let relay_dropped = IntCounterVec::new(
            Opts::new(
                "kallipai_tagma_relay_dropped_total",
                "Inbound 1:1 relay frames the tagma received and discarded, by cause. A count here means the two ends disagree; drops that are normal protocol behaviour are not counted.",
            ),
            &["reason"],
        )
        .expect("a statically named counter");
        let outbound_requests = IntCounterVec::new(
            Opts::new(
                "kallipai_tagma_outbound_requests_total",
                "Outbound platform calls by leg and outcome; one count per call. A stream that stays open is not a call: relays_connected and relay_reconnects_total carry the tunnel.",
            ),
            &["leg", "outcome"],
        )
        .expect("a statically named counter");
        let outbound_duration = HistogramVec::new(
            HistogramOpts::new(
                "kallipai_tagma_outbound_request_duration_seconds",
                "Outbound platform call duration by leg.",
            )
            .buckets(OUTBOUND_BUCKETS.to_vec()),
            &["leg"],
        )
        .expect("a statically named histogram");
        let build_info = IntGaugeVec::new(
            Opts::new(
                "kallipai_tagma_build_info",
                "Build metadata; the value is always 1, the labels carry the version and the tagma id.",
            ),
            &["version", "tagma_id"],
        )
        .expect("a statically named gauge");
        let collector = snapshot::SnapshotCollector::new();
        for collected in [
            Box::new(http_requests.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(http_duration.clone()),
            Box::new(relays_configured.clone()),
            Box::new(relays_connected.clone()),
            Box::new(relay_reconnects.clone()),
            Box::new(relay_dropped.clone()),
            Box::new(outbound_requests.clone()),
            Box::new(outbound_duration.clone()),
            Box::new(build_info.clone()),
            Box::new(collector.clone()),
        ] {
            registry
                .register(collected)
                .expect("registering a statically named collector");
        }
        let metrics = Arc::new(Self {
            registry,
            http_requests,
            http_duration,
            relays_configured,
            relays_connected,
            relay_reconnects,
            relay_dropped,
            outbound_requests,
            outbound_duration,
            build_info,
            beats: BeatTable::new(),
            adk: kallipai_adk::metrics::AdkMetrics::default(),
            collector,
        });
        metrics.collector.attach(&metrics);
        metrics
    }

    /// The adk handle for one agent context: the same counters every agent in
    /// this process records into.
    pub fn adk(&self) -> kallipai_adk::metrics::AdkMetrics {
        self.adk.clone()
    }

    /// Point the snapshot collector at the running tagma, for the families it
    /// reads out of live state (the roster, the bus). Called once, after the
    /// state exists; before that those families report nothing.
    pub fn attach_state(&self, state: &SharedState) {
        self.collector.attach_state(state);
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
        self.http_requests
            .with_label_values(&[route, method, status_class])
            .inc();
        self.http_duration
            .with_label_values(&[route])
            .observe(duration.as_secs_f64());
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
        self.relays_configured.get()
    }

    /// How many relay tunnels are open right now.
    pub fn relays_connected(&self) -> i64 {
        self.relays_connected.get()
    }

    /// The operator's relay entries, counted once at boot.
    pub fn set_relays_configured(&self, configured: i64) {
        self.relays_configured.set(configured);
    }

    /// A tunnel session ended and the reconnect loop is opening the next one.
    /// A counter, not the connected gauge: a flapping tunnel reads as a growing
    /// rate, which is what it is.
    pub fn relay_reconnected(&self) {
        self.relay_reconnects.inc();
    }

    /// A tunnel came up.
    pub fn relay_up(&self) {
        self.relays_connected.inc();
    }

    /// A tunnel went down: stream ended, error, or shutdown.
    pub fn relay_down(&self) {
        self.relays_connected.dec();
    }

    /// Count an inbound frame this tagma discarded, by cause. See [`DropReason`]
    /// for the closed set and for which drops stay uncounted.
    pub fn relay_dropped(&self, reason: DropReason) {
        self.relay_dropped
            .with_label_values(&[reason.as_str()])
            .inc();
    }

    /// The one-off identity series: which build, which tagma. Called once at
    /// boot. `tagma_id` is `None` on a local-only tagma, and the label says so
    /// rather than pretending an identity.
    pub fn set_build_info(&self, version: &str, tagma_id: Option<&str>) {
        self.build_info
            .with_label_values(&[version, tagma_id.unwrap_or("none")])
            .set(1);
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
        self.outbound_requests
            .with_label_values(&[leg.as_str(), outcome.as_str()])
            .inc();
        self.outbound_duration
            .with_label_values(&[leg.as_str()])
            .observe(started.elapsed().as_secs_f64());
        result
    }

    /// The Prometheus text of every family gathered right now. The one failure
    /// mode is the encoder's (a family that could not be rendered), reported to
    /// the caller rather than swallowed: a scrape that cannot render should
    /// answer 5xx, not an empty 200.
    pub fn render(&self) -> Result<String, String> {
        let families = self.registry.gather();
        TextEncoder::new()
            .encode_to_string(&families)
            .map_err(|error| error.to_string())
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

/// One rendered counter's value from the process metrics. Tests that assert
/// an increment at a call site (rather than inside [`Metrics::observe_outbound`])
/// read the scrape text, the way an operator would.
#[cfg(test)]
pub(crate) fn counter_value(name: &str, labels: &str) -> u64 {
    let text = metrics().render().expect("metrics render");
    let needle = format!("{name}{{{labels}}} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&needle)?.trim().parse().ok())
        .unwrap_or(0)
}

/// The process's metrics.
///
/// One set per process, like the export pipeline: it is published on the single
/// listener and describes the single `app`. A test that wants isolation builds
/// [`Metrics::new`] directly and asserts against that instance; a test that just
/// serves the router takes the shared one, which starts empty.
pub fn metrics() -> &'static Metrics {
    static GLOBAL: OnceLock<Arc<Metrics>> = OnceLock::new();
    GLOBAL.get_or_init(Metrics::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::proto::{MetricFamily, MetricType};

    fn family<'a>(families: &'a [MetricFamily], name: &str) -> &'a MetricFamily {
        families
            .iter()
            .find(|family| family.name() == name)
            .unwrap_or_else(|| {
                let gathered: Vec<&str> = families.iter().map(|family| family.name()).collect();
                panic!("{name} is gathered; got {gathered:?}")
            })
    }

    /// The sample carrying exactly these labels, if there is one.
    fn sample(family: &MetricFamily, labels: &[(&str, &str)]) -> Option<f64> {
        family.get_metric().iter().find_map(|metric| {
            let pairs = metric.get_label();
            let exact = pairs.len() == labels.len()
                && labels.iter().all(|(name, value)| {
                    pairs
                        .iter()
                        .any(|pair| pair.name() == *name && pair.value() == *value)
                });
            exact.then(|| match family.get_field_type() {
                MetricType::COUNTER => metric
                    .get_counter()
                    .as_ref()
                    .map_or(f64::NAN, |counter| counter.value()),
                _ => metric
                    .get_gauge()
                    .as_ref()
                    .map_or(f64::NAN, |gauge| gauge.value()),
            })
        })
    }

    #[test]
    fn every_family_reports_the_type_its_data_is() {
        let metrics = Metrics::new();
        // A vector family gathers nothing until it has a child, so each one
        // gets a sample before its type can be read.
        metrics.record_http("/tasks", "GET", 200, Duration::from_millis(1));
        metrics.set_build_info("1.2.3", None);
        let families = metrics.registry.gather();
        assert_eq!(
            family(&families, "kallipai_tagma_http_requests_total").get_field_type(),
            MetricType::COUNTER
        );
        assert_eq!(
            family(&families, "kallipai_tagma_http_request_duration_seconds").get_field_type(),
            MetricType::HISTOGRAM
        );
        assert_eq!(
            family(&families, "kallipai_tagma_relays_configured").get_field_type(),
            MetricType::GAUGE
        );
        assert_eq!(
            family(&families, "kallipai_tagma_build_info").get_field_type(),
            MetricType::GAUGE
        );
    }

    #[test]
    fn a_request_lands_in_the_counter_with_its_status_class() {
        let metrics = Metrics::new();
        metrics.record_http("/tasks", "POST", 503, Duration::from_millis(20));
        let families = metrics.registry.gather();
        let requests = family(&families, "kallipai_tagma_http_requests_total");
        assert_eq!(
            sample(
                requests,
                &[
                    ("route", "/tasks"),
                    ("method", "POST"),
                    ("status_class", "5xx")
                ]
            ),
            Some(1.0)
        );
        let duration = family(&families, "kallipai_tagma_http_request_duration_seconds");
        assert_eq!(
            duration.get_metric().len(),
            1,
            "one series per route, however many requests it served"
        );
        assert_eq!(
            duration.get_metric()[0].get_histogram().get_sample_count(),
            1
        );
    }

    #[test]
    fn a_request_that_matched_no_route_is_its_own_series() {
        let metrics = Metrics::new();
        metrics.record_http(UNMATCHED_ROUTE, "GET", 404, Duration::from_millis(1));
        let families = metrics.registry.gather();
        let requests = family(&families, "kallipai_tagma_http_requests_total");
        assert_eq!(
            sample(
                requests,
                &[
                    ("route", UNMATCHED_ROUTE),
                    ("method", "GET"),
                    ("status_class", "4xx")
                ]
            ),
            Some(1.0)
        );
    }

    #[test]
    fn build_info_names_the_version_and_only_a_known_identity() {
        let metrics = Metrics::new();
        metrics.set_build_info("1.2.3", None);
        metrics.set_build_info("1.2.3", Some("t-9"));
        let families = metrics.registry.gather();
        let build = family(&families, "kallipai_tagma_build_info");
        assert_eq!(
            sample(build, &[("version", "1.2.3"), ("tagma_id", "none")]),
            Some(1.0)
        );
        assert_eq!(
            sample(build, &[("version", "1.2.3"), ("tagma_id", "t-9")]),
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn each_outbound_outcome_is_counted_under_its_leg() {
        let metrics = Metrics::new();
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
        let families = metrics.registry.gather();
        let outbound = family(&families, "kallipai_tagma_outbound_requests_total");
        assert_eq!(
            sample(outbound, &[("leg", "files"), ("outcome", "error")]),
            Some(1.0)
        );
        assert_eq!(
            sample(outbound, &[("leg", "lesche"), ("outcome", "ok")]),
            Some(1.0)
        );
        assert_eq!(
            sample(outbound, &[("leg", "archeion"), ("outcome", "timeout")]),
            Some(1.0)
        );
    }

    #[test]
    fn a_beat_makes_its_loop_fresh_and_an_unbeaten_one_reports_the_process_age() {
        let metrics = Metrics::new();
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
        let metrics = Metrics::new();
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

    #[test]
    fn the_encoder_adds_the_infinite_bucket_exactly_once() {
        let metrics = Metrics::new();
        metrics.record_http("/tasks", "GET", 200, Duration::from_millis(2));
        let text = metrics.render().expect("the snapshot renders");
        assert!(text.contains("kallipai_tagma_http_requests_total{"));
        assert_eq!(
            text.lines()
                .filter(|line| line
                    .starts_with("kallipai_tagma_http_request_duration_seconds_bucket")
                    && line.contains("le=\"+Inf\""))
                .count(),
            1,
            "the observed route gets exactly one +Inf bucket: {text}"
        );
    }
}
