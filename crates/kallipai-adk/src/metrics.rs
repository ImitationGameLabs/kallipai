//! Wake, model and tool counters as plain atomics.
//!
//! A library does not choose a reporting format. These values are a snapshot
//! the owner encodes into whatever exporter it runs -- Prometheus text today,
//! an OTLP meter if the uphill path moves there -- so nothing here depends on
//! an exporter crate, and a swap touches only the owner.
//!
//! Scope is process lifetime, like [`crate::usage_stats`]: counters start at
//! zero at boot and never reset. Clones share one `Arc`, and recording is a
//! relaxed atomic add, so any task on the hot path may record without a lock.
//!
//! Durations are held as nanoseconds in an `AtomicU64` (saturating) and
//! exposed in seconds. A histogram keeps one counter per bucket plus sum and
//! count; [`AdkSnapshot`] folds the per-bucket counts into the cumulative form
//! an exporter expects.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

/// How one agent wake ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WakeOutcome {
    /// The wake completed and left a persisted turn behind.
    Completed,
    /// The wake ended in an error.
    Failed,
    /// The wake was cancelled from outside.
    Cancelled,
}

impl WakeOutcome {
    /// Every value, in label order.
    pub const ALL: [Self; 3] = [Self::Completed, Self::Failed, Self::Cancelled];

    /// The label value an exporter reports this outcome under.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Completed => 0,
            Self::Failed => 1,
            Self::Cancelled => 2,
        }
    }
}

/// How one model attempt ended. `Cancelled` is a caller-side stop (the wake's
/// cancellation, shutdown), distinct from a provider-side failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ModelOutcome {
    /// The attempt produced a stream that then ran to its end.
    Ok,
    /// The provider or the transport failed.
    Error,
    /// The attempt deadline fired.
    Timeout,
    /// The attempt was cancelled while its stream was in flight.
    Cancelled,
}

impl ModelOutcome {
    /// Every value, in label order.
    pub const ALL: [Self; 4] = [Self::Ok, Self::Error, Self::Timeout, Self::Cancelled];

    /// The label value an exporter reports this outcome under.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Ok => 0,
            Self::Error => 1,
            Self::Timeout => 2,
            Self::Cancelled => 3,
        }
    }
}

/// Which counter of the token totals a usage record feeds. The four mirror
/// [`crate::usage_stats::UsageEntry`]: reasoning tokens are part of the
/// completion total the provider bills, not a fifth kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TokenKind {
    /// Prompt tokens sent.
    Input,
    /// Completion tokens returned.
    Output,
    /// Prompt tokens served from the provider cache.
    CacheRead,
    /// Prompt tokens written into the provider cache.
    CacheWrite,
}

impl TokenKind {
    /// Every value, in label order.
    pub const ALL: [Self; 4] = [Self::Input, Self::Output, Self::CacheRead, Self::CacheWrite];

    /// The label value an exporter reports this kind under.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::CacheRead => "cache_read",
            Self::CacheWrite => "cache_write",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Input => 0,
            Self::Output => 1,
            Self::CacheRead => 2,
            Self::CacheWrite => 3,
        }
    }
}

/// How one tool call ended. The vocabulary is the one the tool span's
/// `kallipai.tool.outcome` attribute uses (`tool_execution.rs`), so a trace
/// and a counter read against each other.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ToolOutcome {
    /// The tool answered successfully.
    Success,
    /// The tool answered with a failure.
    Failed,
    /// This runner's timeout cut the call off.
    Timeout,
    /// The wake was cancelled while the call was running.
    Cancelled,
    /// The call is waiting on an approval.
    Deferred,
    /// The call never ran because an earlier call did not succeed.
    Skipped,
}

impl ToolOutcome {
    /// Every value, in label order.
    pub const ALL: [Self; 6] = [
        Self::Success,
        Self::Failed,
        Self::Timeout,
        Self::Cancelled,
        Self::Deferred,
        Self::Skipped,
    ];

    /// The label value an exporter reports this outcome under.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Deferred => "deferred",
            Self::Skipped => "skipped",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Success => 0,
            Self::Failed => 1,
            Self::Timeout => 2,
            Self::Cancelled => 3,
            Self::Deferred => 4,
            Self::Skipped => 5,
        }
    }
}

/// Bucket edges for agent wakes: seconds from wake start to wake end. A
/// short wake is one or two tool calls; the tail is a long shell command or a
/// reasoning model thinking.
const WAKE_BUCKETS: &[f64] = &[1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0];

/// Bucket edges for one model attempt: from its send to its end. A local
/// model opens a stream under a second and then streams; a reasoning model
/// thinks for minutes, and a send that fails before it opens sits at the
/// short end.
const MODEL_BUCKETS: &[f64] = &[
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

/// Bucket edges for the wait to the first streamed delta: what the user
/// actually feels. Below the first edge is instant; past the last is a
/// long-thinking model.
const FIRST_TOKEN_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0];

/// Bucket edges for one tool call: a builtin read is milliseconds, a shell
/// command or a code edit is minutes.
const TOOL_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 600.0];

/// One histogram over a fixed edge list: per-bucket counts, a saturating
/// nanosecond sum, and the sample count. Every bucket exists from
/// construction, so a snapshot has a stable shape from the first scrape.
struct DurationHistogram {
    edges: &'static [f64],
    /// One counter per bucket; the last one collects everything above the
    /// highest edge.
    buckets: Box<[AtomicU64]>,
    sum_nanos: AtomicU64,
    count: AtomicU64,
}

impl DurationHistogram {
    fn new(edges: &'static [f64]) -> Self {
        Self {
            edges,
            buckets: (0..=edges.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_nanos: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, duration: Duration) {
        let seconds = duration.as_secs_f64();
        let bucket = self
            .edges
            .iter()
            .position(|edge| seconds <= *edge)
            .unwrap_or(self.edges.len());
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> HistogramSnapshot {
        let mut cumulative = Vec::with_capacity(self.buckets.len());
        let mut running = 0u64;
        for bucket in self.buckets.iter() {
            running += bucket.load(Ordering::Relaxed);
            cumulative.push(running);
        }
        HistogramSnapshot {
            edges: self.edges.to_vec(),
            cumulative,
            sum_seconds: self.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9,
            count: self.count.load(Ordering::Relaxed),
        }
    }
}

/// A histogram read out of its atomics, in the form an exporter encodes.
///
/// `cumulative[i]` counts the samples at or below `edges[i]`; the final entry
/// (one longer than `edges`) is every sample, the `+Inf` bucket, and equals
/// [`Self::count`].
#[derive(Clone, Debug, PartialEq)]
pub struct HistogramSnapshot {
    /// The bucket edges, in ascending order.
    pub edges: Vec<f64>,
    /// Cumulative counts, one longer than `edges`.
    pub cumulative: Vec<u64>,
    /// Sum of every sample, in seconds.
    pub sum_seconds: f64,
    /// Number of samples.
    pub count: u64,
}

impl HistogramSnapshot {
    /// The `+Inf` bucket: every sample.
    pub fn total(&self) -> u64 {
        self.cumulative.last().copied().unwrap_or_default()
    }
}

/// Process-lifetime counters for the wake, model and tool boundaries.
///
/// One value is shared by the tagma state and every
/// [`crate::agent_task::AgentContext`]; a context built outside a running
/// tagma (tests, tools) keeps its own and simply never reaches a scrape.
///
/// Why one process-lifetime value rather than a field on the shared state:
/// the counters are recorded deep inside the acquisition loop, below the
/// layers that hold that state. A recorder threaded down would be skipped on
/// every early exit and would force each intermediate type to carry it. The
/// exporter reads the same value at scrape time, so both ends meet without a
/// channel between them.
#[derive(Clone)]
pub struct AdkMetrics {
    inner: Arc<AdkMetricsInner>,
}

impl Default for AdkMetrics {
    fn default() -> Self {
        Self::new()
    }
}

struct AdkMetricsInner {
    wakes: [AtomicU64; 3],
    wake_duration: [DurationHistogram; 3],
    model_requests: [AtomicU64; 4],
    model_duration: DurationHistogram,
    first_token: DurationHistogram,
    tokens: [AtomicU64; 4],
    model_in_flight: AtomicI64,
    tool_calls: [AtomicU64; 6],
    tool_duration: [DurationHistogram; 6],
}

impl AdkMetrics {
    /// An empty set. Every bucket exists, so a scrape before the first wake
    /// still sees the full shape.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AdkMetricsInner {
                wakes: std::array::from_fn(|_| AtomicU64::new(0)),
                wake_duration: std::array::from_fn(|_| DurationHistogram::new(WAKE_BUCKETS)),
                model_requests: std::array::from_fn(|_| AtomicU64::new(0)),
                model_duration: DurationHistogram::new(MODEL_BUCKETS),
                first_token: DurationHistogram::new(FIRST_TOKEN_BUCKETS),
                tokens: std::array::from_fn(|_| AtomicU64::new(0)),
                model_in_flight: AtomicI64::new(0),
                tool_calls: std::array::from_fn(|_| AtomicU64::new(0)),
                tool_duration: std::array::from_fn(|_| DurationHistogram::new(TOOL_BUCKETS)),
            }),
        }
    }

    /// Record one finished wake: its outcome and how long it took.
    pub fn record_wake(&self, outcome: WakeOutcome, duration: Duration) {
        self.inner.wakes[outcome.index()].fetch_add(1, Ordering::Relaxed);
        self.inner.wake_duration[outcome.index()].observe(duration);
    }

    /// Mark one model request in flight. The returned guard decrements on
    /// drop, so a cancelled or panicking path cannot leave the gauge stuck.
    pub fn model_in_flight(&self) -> InFlight<'_> {
        self.inner.model_in_flight.fetch_add(1, Ordering::Relaxed);
        InFlight {
            counter: &self.inner.model_in_flight,
        }
    }

    /// Record one model attempt: its outcome, and the time from the send that
    /// started it to the attempt's end. A backoff before a later attempt is the
    /// retry policy's time and is never part of `duration`.
    pub fn record_model_call(&self, outcome: ModelOutcome, duration: Duration) {
        self.inner.model_requests[outcome.index()].fetch_add(1, Ordering::Relaxed);
        self.inner.model_duration.observe(duration);
    }

    /// Record the wait from the request being sent to the first delta of the
    /// stream that was finally adopted. A stream abandoned mid-way never
    /// reaches here: a retry's first delta is the one the user saw.
    pub fn record_first_token(&self, duration: Duration) {
        self.inner.first_token.observe(duration);
    }

    /// Record one usage report's token counts.
    pub fn record_tokens(&self, kind: TokenKind, tokens: u64) {
        self.inner.tokens[kind.index()].fetch_add(tokens, Ordering::Relaxed);
    }

    /// Record one usage report across the token kinds.
    ///
    /// A counter the provider did not report is not recorded at all: "no
    /// cache was used" and "the provider said nothing about cache" are
    /// different facts, and the span's usage attributes draw the same line.
    pub fn record_usage(&self, usage: &just_llm_client::types::generation::Usage) {
        self.record_tokens(TokenKind::Input, u64::from(usage.prompt_tokens));
        self.record_tokens(TokenKind::Output, u64::from(usage.completion_tokens));
        if let Some(tokens) = usage.cache_read_tokens {
            self.record_tokens(TokenKind::CacheRead, u64::from(tokens));
        }
        if let Some(tokens) = usage.cache_write_tokens {
            self.record_tokens(TokenKind::CacheWrite, u64::from(tokens));
        }
    }

    /// Record one finished tool call.
    pub fn record_tool_call(&self, outcome: ToolOutcome, duration: Duration) {
        self.inner.tool_calls[outcome.index()].fetch_add(1, Ordering::Relaxed);
        self.inner.tool_duration[outcome.index()].observe(duration);
    }

    /// Read every counter out, in label order.
    pub fn snapshot(&self) -> AdkSnapshot {
        AdkSnapshot {
            wakes_total: label_values(WakeOutcome::ALL.map(WakeOutcome::as_str), &self.inner.wakes),
            wake_duration: WakeOutcome::ALL
                .iter()
                .map(|outcome| {
                    (
                        outcome.as_str(),
                        self.inner.wake_duration[outcome.index()].snapshot(),
                    )
                })
                .collect(),
            model_requests_total: label_values(
                ModelOutcome::ALL.map(ModelOutcome::as_str),
                &self.inner.model_requests,
            ),
            model_duration: self.inner.model_duration.snapshot(),
            first_token: self.inner.first_token.snapshot(),
            tokens_total: label_values(TokenKind::ALL.map(TokenKind::as_str), &self.inner.tokens),
            model_in_flight: self.inner.model_in_flight.load(Ordering::Relaxed),
            tool_calls_total: label_values(
                ToolOutcome::ALL.map(ToolOutcome::as_str),
                &self.inner.tool_calls,
            ),
            tool_duration: ToolOutcome::ALL
                .iter()
                .map(|outcome| {
                    (
                        outcome.as_str(),
                        self.inner.tool_duration[outcome.index()].snapshot(),
                    )
                })
                .collect(),
        }
    }
}

fn label_values<const N: usize>(
    labels: [&'static str; N],
    counters: &[AtomicU64; N],
) -> Vec<(&'static str, u64)> {
    labels
        .iter()
        .zip(counters)
        .map(|(label, counter)| (*label, counter.load(Ordering::Relaxed)))
        .collect()
}

/// One model request in flight, as a drop guard.
pub struct InFlight<'a> {
    counter: &'a AtomicI64,
}

impl InFlight<'_> {
    /// Record the request's outcome and duration, and end the in-flight span.
    pub fn finish(self, metrics: &AdkMetrics, outcome: ModelOutcome, started: std::time::Instant) {
        metrics.record_model_call(outcome, started.elapsed());
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Everything [`AdkMetrics`] holds, read out. Labels are the label values an
/// exporter reports, in each enum's declared order.
#[derive(Clone, Debug, PartialEq)]
pub struct AdkSnapshot {
    /// Wakes by outcome.
    pub wakes_total: Vec<(&'static str, u64)>,
    /// Wake duration by outcome.
    pub wake_duration: Vec<(&'static str, HistogramSnapshot)>,
    /// Model requests by outcome, one per send attempt.
    pub model_requests_total: Vec<(&'static str, u64)>,
    /// Model request duration from the starting send to the attempt's end,
    /// all outcomes together.
    pub model_duration: HistogramSnapshot,
    /// Wait from the send to the first streamed delta of the adopted stream.
    pub first_token: HistogramSnapshot,
    /// Tokens by kind.
    pub tokens_total: Vec<(&'static str, u64)>,
    /// Model requests in flight right now.
    pub model_in_flight: i64,
    /// Tool calls by outcome.
    pub tool_calls_total: Vec<(&'static str, u64)>,
    /// Tool call duration by outcome.
    pub tool_duration: Vec<(&'static str, HistogramSnapshot)>,
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn count(snapshot: &AdkSnapshot, family: Families, label: &str) -> u64 {
        let pairs = match family {
            Families::Wakes => &snapshot.wakes_total,
            Families::Model => &snapshot.model_requests_total,
            Families::Tokens => &snapshot.tokens_total,
            Families::Tools => &snapshot.tool_calls_total,
        };
        pairs
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, value)| *value)
            .expect("label is part of the vocabulary")
    }

    #[derive(Clone, Copy)]
    enum Families {
        Wakes,
        Model,
        Tokens,
        Tools,
    }

    #[test]
    fn snapshot_starts_empty_with_a_full_shape() {
        let snapshot = AdkMetrics::new().snapshot();
        assert_eq!(snapshot.wakes_total.len(), WakeOutcome::ALL.len());
        assert!(snapshot.wakes_total.iter().all(|(_, value)| *value == 0));
        assert_eq!(snapshot.wake_duration.len(), WakeOutcome::ALL.len());
        assert_eq!(snapshot.model_requests_total.len(), ModelOutcome::ALL.len());
        assert_eq!(snapshot.tokens_total.len(), TokenKind::ALL.len());
        assert_eq!(snapshot.tool_calls_total.len(), ToolOutcome::ALL.len());
        assert_eq!(snapshot.tool_duration.len(), ToolOutcome::ALL.len());
        assert_eq!(snapshot.model_in_flight, 0);
        // Every histogram carries its edges and one more cumulative slot than
        // edges, all zero, so a scrape before the first wake is well formed.
        assert_eq!(
            snapshot.first_token.edges.len() + 1,
            snapshot.first_token.cumulative.len()
        );
        assert_eq!(snapshot.first_token.total(), 0);
        assert_eq!(snapshot.model_duration.count, 0);
    }

    #[test]
    fn wakes_accumulate_by_outcome() {
        let metrics = AdkMetrics::new();
        metrics.record_wake(WakeOutcome::Completed, Duration::from_millis(500));
        metrics.record_wake(WakeOutcome::Completed, Duration::from_secs(90));
        metrics.record_wake(WakeOutcome::Failed, Duration::from_secs(2));
        let snapshot = metrics.snapshot();
        assert_eq!(count(&snapshot, Families::Wakes, "completed"), 2);
        assert_eq!(count(&snapshot, Families::Wakes, "failed"), 1);
        assert_eq!(count(&snapshot, Families::Wakes, "cancelled"), 0);
        let completed = snapshot
            .wake_duration
            .iter()
            .find(|(label, _)| *label == "completed")
            .map(|(_, histogram)| histogram)
            .expect("completed is part of the vocabulary");
        assert_eq!(completed.count, 2);
        assert_eq!(completed.total(), 2);
        // 500ms lands in the first bucket, 90s in the 120s bucket: the
        // cumulative counts are 1 at 1s and 2 from 120s on.
        assert_eq!(completed.cumulative[0], 1);
        assert_eq!(*completed.cumulative.last().expect("an overflow bucket"), 2);
        assert!((completed.sum_seconds - 90.5).abs() < 1e-6);
    }

    #[test]
    fn durations_above_the_last_edge_land_in_the_overflow_bucket() {
        let metrics = AdkMetrics::new();
        metrics.record_first_token(Duration::from_secs(200));
        let snapshot = metrics.snapshot();
        let histogram = &snapshot.first_token;
        assert_eq!(histogram.count, 1);
        // Only the +Inf slot holds the sample.
        assert_eq!(histogram.cumulative[histogram.edges.len() - 1], 0);
        assert_eq!(histogram.total(), 1);
    }

    #[test]
    fn bucket_edges_are_inclusive() {
        let metrics = AdkMetrics::new();
        metrics.record_first_token(Duration::from_millis(50));
        metrics.record_first_token(Duration::from_millis(51));
        let histogram = metrics.snapshot().first_token;
        assert_eq!(histogram.cumulative[0], 1);
        assert_eq!(histogram.cumulative[1], 2);
        assert_eq!(histogram.count, 2);
    }

    #[test]
    fn the_in_flight_guard_always_decrements() {
        let metrics = AdkMetrics::new();
        {
            let _first = metrics.model_in_flight();
            let _second = metrics.model_in_flight();
            assert_eq!(metrics.snapshot().model_in_flight, 2);
        }
        assert_eq!(metrics.snapshot().model_in_flight, 0);
    }

    #[test]
    fn finishing_an_in_flight_request_records_it() {
        let metrics = AdkMetrics::new();
        let started = Instant::now();
        let guard = metrics.model_in_flight();
        guard.finish(&metrics, ModelOutcome::Ok, started);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.model_in_flight, 0);
        assert_eq!(count(&snapshot, Families::Model, "ok"), 1);
        assert_eq!(snapshot.model_duration.count, 1);
    }

    #[test]
    fn every_tool_outcome_is_recorded_under_its_own_label() {
        let metrics = AdkMetrics::new();
        for outcome in ToolOutcome::ALL {
            metrics.record_tool_call(outcome, Duration::from_millis(10));
        }
        let snapshot = metrics.snapshot();
        for outcome in ToolOutcome::ALL {
            assert_eq!(count(&snapshot, Families::Tools, outcome.as_str()), 1);
        }
        let total: u64 = snapshot
            .tool_calls_total
            .iter()
            .map(|(_, value)| *value)
            .sum();
        assert_eq!(total, ToolOutcome::ALL.len() as u64);
    }

    #[test]
    fn tokens_accumulate_per_kind() {
        let metrics = AdkMetrics::new();
        metrics.record_tokens(TokenKind::Input, 100);
        metrics.record_tokens(TokenKind::Input, 50);
        metrics.record_tokens(TokenKind::Output, 7);
        let snapshot = metrics.snapshot();
        assert_eq!(count(&snapshot, Families::Tokens, "input"), 150);
        assert_eq!(count(&snapshot, Families::Tokens, "output"), 7);
        assert_eq!(count(&snapshot, Families::Tokens, "cache_read"), 0);
    }

    #[test]
    fn clones_share_one_counter_set() {
        let metrics = AdkMetrics::new();
        let clone = metrics.clone();
        clone.record_wake(WakeOutcome::Completed, Duration::from_millis(1));
        assert_eq!(count(&metrics.snapshot(), Families::Wakes, "completed"), 1);
    }

    #[test]
    fn labels_are_unique_and_ordered() {
        let wakes: Vec<&str> = WakeOutcome::ALL
            .iter()
            .map(|outcome| outcome.as_str())
            .collect();
        assert_eq!(wakes, ["completed", "failed", "cancelled"]);
        let models: Vec<&str> = ModelOutcome::ALL
            .iter()
            .map(|outcome| outcome.as_str())
            .collect();
        assert_eq!(models, ["ok", "error", "timeout", "cancelled"]);
        let tools: Vec<&str> = ToolOutcome::ALL
            .iter()
            .map(|outcome| outcome.as_str())
            .collect();
        assert_eq!(
            tools,
            [
                "success",
                "failed",
                "timeout",
                "cancelled",
                "deferred",
                "skipped"
            ]
        );
    }
}
