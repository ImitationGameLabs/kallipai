//! The exporter half: configuration, the layer that feeds a collector, and the
//! bootstrap entry point.
//!
//! The module document in [`super`] covers the shape of a deployment. Nothing
//! here sits in a request path; a service that never sets the endpoint does not
//! compile this file at all (feature `telemetry`) and still gets the
//! propagation half.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{
    RetryPolicy, SpanExporter as OtlpExporter, WithExportConfig as _, WithHttpConfig as _,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, Sampler, SdkTracerProvider, SpanData, SpanExporter,
};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::Registry;

use crate::logging;

/// The collector's OTLP/HTTP base URL; the traces path is appended.
pub const ENDPOINT_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
/// The header an intake reads a bearer token from.
pub(super) const AUTHORIZATION_HEADER: &str = "authorization";
/// The sampler override; absent means parent-based always-on.
pub const SAMPLER_VAR: &str = "OTEL_TRACES_SAMPLER";
/// The argument the ratio samplers read.
pub const SAMPLER_ARG_VAR: &str = "OTEL_TRACES_SAMPLER_ARG";

/// Spans held for export before the queue starts dropping them.
const QUEUE_SIZE: usize = 2048;
/// How many spans one export call carries.
const BATCH_SIZE: usize = 512;
/// How often a batch leaves, quiet traffic or not.
const SCHEDULED_DELAY: Duration = Duration::from_secs(5);
/// The budget for one export call: the batch processor cancels the call when
/// it runs out, so a stalled collector cannot pin the queue.
///
/// This is the outer of three bounds. One HTTP attempt is bounded by the
/// client the exporter builds; this budget bounds the whole call the
/// processor makes, retries included; and [`EXPORT_DEADLINE`] sits one step
/// inside it, because a call the processor cancels is a call nothing can
/// report on.
pub(crate) const EXPORT_BUDGET: Duration = Duration::from_secs(3);
/// The deadline the exporter and the tally hold themselves to: the budget
/// less the margin the processor needs to cancel and record the call.
///
/// The processor cancels by dropping the export future, and every tally in
/// [`CountingExporter`] is written after that future returns -- so the one
/// failure an operator most needs counted, a collector that stopped
/// answering, would be the one that left no mark and no log line. Two things
/// therefore stop here: the counting wrapper's own timeout, so it books the
/// expiry itself, and the exporter's retry deadline, so the dependency ends
/// its attempt before the ceiling rather than being cut off by it.
pub(crate) const EXPORT_DEADLINE: Duration = Duration::from_millis(2750);
/// Passed to `shutdown_with_timeout`, which the SDK ignores: a shutdown flushes
/// and then waits for one more export, so the real bound is about twice
/// `EXPORT_BUDGET` -- six seconds, inside the ten a default container stop
/// before the process is killed.
pub(super) const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);

/// The resource namespace every service reports, so one deployment's traces
/// share a namespace whatever their service names. A deployment that runs
/// several instances of one service tells them apart with
/// `OTEL_RESOURCE_ATTRIBUTES=service.instance.id=<slug>`, which the SDK's
/// environment detector folds into this resource.
pub(crate) const RESOURCE_NAMESPACE: &str = "kallipai";

pub use crate::SPAN_TARGET;

/// The path the OTLP HTTP exporter posts traces to; a configured base gains it.
const TRACES_PATH: &str = "/v1/traces";

/// The export configuration: where spans go and which sampler decides.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub(crate) endpoint: String,
    pub(crate) sampler: SamplerSpec,
}

impl Config {
    /// The collector endpoint, verbatim from the environment: a base URL whose
    /// path gains `/v1/traces` unless it already ends there, and a query stays
    /// on the end. The specification defines the variable as a base, and the
    /// SDK appends the same path when it reads the variable itself.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The sampler this configuration selects.
    pub fn sampler(&self) -> SamplerSpec {
        self.sampler
    }
}

/// The sampler families `OTEL_TRACES_SAMPLER` names, mapped onto the SDK's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SamplerSpec {
    /// Roots are sampled and children follow the parent (the default).
    ParentBasedAlwaysOn,
    /// Roots are dropped and children follow the parent.
    ParentBasedAlwaysOff,
    /// Roots take a fixed fraction and children follow the parent.
    ParentBasedTraceIdRatio(f64),
    /// Every span is sampled.
    AlwaysOn,
    /// No span is sampled.
    AlwaysOff,
    /// A fixed fraction of trace ids, independent of the parent.
    TraceIdRatio(f64),
}

impl SamplerSpec {
    /// Parse the variable's value; an unknown name or a ratio that does not
    /// parse yields `None`, and the caller keeps the default.
    fn parse(name: &str, arg: Option<&str>) -> Option<Self> {
        // A ratio outside [0, 1] or not a number would pin sampling on or off
        // for good, so such a value counts as unparseable and the caller keeps
        // its default.
        let ratio = || {
            arg.and_then(|raw| raw.trim().parse::<f64>().ok())
                .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "always_on" => Some(Self::AlwaysOn),
            "always_off" => Some(Self::AlwaysOff),
            "traceidratio" => ratio().map(Self::TraceIdRatio),
            "parentbased_always_on" => Some(Self::ParentBasedAlwaysOn),
            "parentbased_always_off" => Some(Self::ParentBasedAlwaysOff),
            "parentbased_traceidratio" => ratio().map(Self::ParentBasedTraceIdRatio),
            _ => None,
        }
    }

    /// The SDK sampler this spec stands for.
    fn sampler(self) -> Sampler {
        match self {
            Self::AlwaysOn => Sampler::AlwaysOn,
            Self::AlwaysOff => Sampler::AlwaysOff,
            Self::TraceIdRatio(ratio) => Sampler::TraceIdRatioBased(ratio),
            Self::ParentBasedAlwaysOn => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
            Self::ParentBasedAlwaysOff => Sampler::ParentBased(Box::new(Sampler::AlwaysOff)),
            Self::ParentBasedTraceIdRatio(ratio) => {
                Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
            }
        }
    }
}

/// Read the export configuration from a variable lookup. An unset or blank
/// endpoint means no export layer at all; a sampler value the parser does not
/// recognize falls back to the default instead of failing the start.
pub fn config_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Config> {
    let endpoint = lookup(ENDPOINT_VAR)?;
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return None;
    }
    let sampler = lookup(SAMPLER_VAR)
        .and_then(|name| {
            let arg = lookup(SAMPLER_ARG_VAR);
            SamplerSpec::parse(&name, arg.as_deref())
        })
        .unwrap_or(SamplerSpec::ParentBasedAlwaysOn);
    Some(Config {
        endpoint: endpoint.to_string(),
        sampler,
    })
}

/// Read the export configuration from the process environment.
pub fn config_from_env() -> Option<Config> {
    config_from_lookup(|key| std::env::var(key).ok())
}

/// The traces URL to post to, resolved from the configured base.
///
/// The variable is a base, and both roads to it must mean the same thing: the
/// SDK appends the signal path when it reads the variable itself, so a value
/// handed over here gains the same `/v1/traces`. A value whose path already
/// ends with it, a trailing `/` aside, is kept as written so a full-signal
/// value does not double, and a query stays on the end, behind the path.
pub(crate) fn traces_endpoint(base: &str) -> String {
    let (endpoint, query) = match base.find('?') {
        Some(at) => (&base[..at], &base[at..]),
        None => (base, ""),
    };
    let endpoint = endpoint.trim_end_matches('/');
    if path_of(endpoint).ends_with(TRACES_PATH) {
        return format!("{endpoint}{query}");
    }
    format!("{endpoint}{TRACES_PATH}{query}")
}

/// The path an endpoint names, or `""` when it names none.
pub(super) fn path_of(endpoint: &str) -> &str {
    let authority = match endpoint.find("://") {
        Some(sep) => &endpoint[sep + 3..],
        None => endpoint,
    };
    authority.find('/').map_or("", |at| &authority[at..])
}
/// The resource both signals report: the service, the namespace every
/// kallipai service shares, and the caller's own attributes.
pub(super) fn resource(service: &str, attrs: &[(&str, &str)]) -> Resource {
    Resource::builder()
        .with_service_name(service.to_string())
        .with_attribute(KeyValue::new("service.namespace", RESOURCE_NAMESPACE))
        .with_attributes(
            attrs
                .iter()
                .map(|(key, value)| KeyValue::new((*key).to_string(), (*value).to_string())),
        )
        .build()
}

/// Holds the tracer provider for the life of the process and flushes the
/// queue once on the way out. Inert when nothing is exported.
///
/// Bind it (`let _telemetry = ...`): a bare statement drops it on the spot and
/// shuts the exporter down against a process that keeps recording spans.
#[must_use = "dropping the guard shuts the exporter down at the end of the statement"]
pub struct Guard {
    provider: Option<SdkTracerProvider>,
}

impl Guard {
    /// A guard for a process that exports nothing.
    fn inert() -> Self {
        Self { provider: None }
    }

    /// Flush what is queued and stop the export thread. Runs on drop; a
    /// second call does nothing. Reports whether telemetry was configured.
    pub fn shutdown(&mut self) -> bool {
        match self.provider.take() {
            None => false,
            Some(provider) => {
                let _ = provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
                true
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Compose the subscriber the service installs, plus the guard that keeps the
/// exporter alive. With a configuration the OTLP layer sits under the event
/// layers; without one this is the plain logging stack. A configuration whose
/// exporter cannot be built logs one line and falls back to the plain stack.
pub(crate) fn service_layers<W>(
    config: Option<&Config>,
    filter: &EnvFilter,
    service: &str,
    attrs: &[(&str, &str)],
    dir: Option<&Path>,
    stdout: W,
) -> (impl tracing::Subscriber, Guard)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    service_layers_with_exporter(
        |endpoint| otlp_exporter(endpoint, None),
        config,
        filter,
        service,
        attrs,
        dir,
        stdout,
    )
}

/// The same composition with the exporter construction injected: `open`
/// receives the resolved traces endpoint and returns the exporter to batch.
/// Tests use it to watch a span leave through the composed stack.
/// The returned subscriber borrows nothing: the captures are precise, so a
/// caller may hold it as `'static`, which `set_default` wants.
pub(crate) fn service_layers_with_exporter<W, E, F>(
    open: F,
    config: Option<&Config>,
    filter: &EnvFilter,
    service: &str,
    attrs: &[(&str, &str)],
    dir: Option<&Path>,
    stdout: W,
) -> (impl tracing::Subscriber + use<W, E, F>, Guard)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
    E: SpanExporter + 'static,
    F: FnOnce(&str) -> Result<E, String>,
{
    let (extra, guard) = export_layer_with(open, config, service, attrs);
    let subscriber = logging::service_layers_with(filter, service, dir, stdout, extra);
    (subscriber, guard)
}

/// The export layer and its guard, for a service that assembles its own
/// subscriber: the tagma keeps a stderr/file policy, a log-directory ownership
/// rule and a panic hook that have no place in the shared bootstrap, so it
/// composes its registry itself and takes only this layer.
///
/// Building this layer publishes the process-wide tracer provider and the W3C
/// propagator with it, so a service that mounts it can inject its context into
/// an outgoing call and adopt one from an incoming one without a second call.
///
/// The guard owns what it published: shutting it down leaves the process-wide
/// handle pointing at that provider, so a `global::tracer()` created afterwards
/// silently records spans nothing exports.
///
/// `None` with an inert guard means nothing is exported: no endpoint is set,
/// or the exporter could not be built (one stderr line, tracing stays local).
///
/// The layer carries its own filter, on [`SPAN_TARGET`], and is therefore not
/// tied to `RUST_LOG`: a service whose log filter is `warn` still exports the
/// spans it opens, while its INFO log lines stay out of the trace.
///
/// `attrs` become resource attributes on every span this process exports, so
/// an identity only the caller knows (a tagma id, say) rides along without a
/// second plumbing path. `export_stats()` answers what the pipeline did with
/// those spans afterwards.
pub fn export_layer(
    config: Option<&Config>,
    service: &str,
    attrs: &[(&str, &str)],
) -> (Option<Box<dyn Layer<Registry> + Send + Sync>>, Guard) {
    export_layer_with(
        |endpoint| otlp_exporter(endpoint, None),
        config,
        service,
        attrs,
    )
}

/// [`export_layer`] with the intake's credential attached to every export.
///
/// `token` is the credential itself, never a preformatted header: the scheme
/// is this crate's business, so a caller reads its token from wherever it
/// keeps it and knows nothing about the OTLP wire. `None` sends no
/// `authorization` header at all, which is what an instance with no
/// credential store has to send. Headers a deployment sets through
/// `OTEL_EXPORTER_OTLP_HEADERS` still apply, because a process without a
/// credential store of its own has no other way to carry one; an
/// `authorization` set there overrides this token in silence, so the two
/// sources must not both be set.
pub fn export_layer_authorized(
    config: Option<&Config>,
    service: &str,
    attrs: &[(&str, &str)],
    token: Option<&str>,
) -> (Option<Box<dyn Layer<Registry> + Send + Sync>>, Guard) {
    export_layer_with(
        |endpoint| otlp_exporter(endpoint, token),
        config,
        service,
        attrs,
    )
}

/// [`export_layer`] with the exporter construction injected, so the assembly
/// can be exercised without a collector.
fn export_layer_with<E, F>(
    open: F,
    config: Option<&Config>,
    service: &str,
    attrs: &[(&str, &str)],
) -> (Option<Box<dyn Layer<Registry> + Send + Sync>>, Guard)
where
    E: SpanExporter + 'static,
    F: FnOnce(&str) -> Result<E, String>,
{
    let Some(config) = config else {
        return (None, Guard::inert());
    };
    match build_export_layer_with(open, config, service, attrs) {
        Ok((layer, provider)) => {
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            opentelemetry::global::set_tracer_provider(provider.clone());
            (
                Some(layer),
                Guard {
                    provider: Some(provider),
                },
            )
        }
        Err(error) => {
            eprintln!(
                "kallipai: {service} telemetry export disabled ({error}); tracing stays local"
            );
            (None, Guard::inert())
        }
    }
}

/// Install the process-wide subscriber and, when an endpoint is configured,
/// the OTLP exporter that feeds it, publishing the process-wide tracer
/// provider and the W3C propagator with it. Returns the guard; hold it for
/// the life of the process so the queue is flushed on the way out.
///
/// This process reports no resource attributes of its own.
pub fn init_service_telemetry(filter: &EnvFilter, service: &str, dir: Option<&Path>) -> Guard {
    let config = config_from_env();
    let (subscriber, guard) =
        service_layers(config.as_ref(), filter, service, &[], dir, std::io::stdout);
    subscriber.try_init().ok();
    guard
}

/// Build the OTLP layer and the provider behind it: export over HTTP, the
/// sampler from the configuration, one resource naming the service and the
/// namespace it belongs to. `with_service_name` overrides
/// `OTEL_SERVICE_NAME`, so the service argument is where a rename happens and
/// the environment cannot set the name behind its back.
fn build_export_layer_with<E, F>(
    open: F,
    config: &Config,
    service: &str,
    attrs: &[(&str, &str)],
) -> Result<(Box<dyn Layer<Registry> + Send + Sync>, SdkTracerProvider), String>
where
    E: SpanExporter + 'static,
    F: FnOnce(&str) -> Result<E, String>,
{
    let exporter = open(&traces_endpoint(config.endpoint()))?;
    let batch = BatchConfigBuilder::default()
        .with_max_queue_size(QUEUE_SIZE)
        .with_max_export_batch_size(BATCH_SIZE)
        .with_scheduled_delay(SCHEDULED_DELAY)
        .with_max_export_timeout(EXPORT_BUDGET)
        .build();
    let processor = BatchSpanProcessor::builder(
        // The hint wraps the tally: the failure the wrapper books by cutting a
        // hung call has to reach the log, or the one line saying a collector
        // stopped answering is the casualty of where the bound sits.
        HintOnceExporter {
            inner: CountingExporter {
                inner: exporter,
                stats: export_stats(),
            },
            hinted: Arc::new(std::array::from_fn(|_| AtomicBool::new(false))),
        },
        runtime::TokioCurrentThread,
    )
    .with_batch_config(batch)
    .build();
    let provider = SdkTracerProvider::builder()
        .with_sampler(config.sampler().sampler())
        .with_resource(resource(service, attrs))
        .with_span_processor(processor)
        .build();
    let tracer = provider.tracer(service.to_string());
    let layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(span_filter())
        .boxed();
    export_stats().mark_enabled();
    Ok((layer, provider))
}

/// The export layer's own filter: spans on [`SPAN_TARGET`], no event at all.
///
/// An event is what carries text -- a message, an upstream error body, up to
/// the capture cap -- so no level of event reaches the trace; span status and
/// attributes say what happened instead. The largest carrier on the agent path
/// is an `error!` that prints an upstream error body: a truth worth keeping out
/// of a collector for good. Nothing is lost by dropping the rest: every span
/// this workspace publishes settles its own status, from [`Settle`] on an agent
/// path and from [`record_response`] or the propagation shell on a request path.
pub(crate) fn span_filter() -> EnvFilter {
    EnvFilter::new(format!("off,{SPAN_TARGET}=info"))
}

/// The OTLP/HTTP exporter for a resolved traces endpoint.
///
/// A token becomes `authorization: Bearer <token>` on every request; no token
/// leaves the request headers to the environment alone.
fn otlp_exporter(endpoint: &str, token: Option<&str>) -> Result<OtlpExporter, String> {
    let exporter = OtlpExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_DEADLINE)
        // Spelled out rather than inherited: three retries from 100ms fit
        // inside the deadline only because a refusal comes back at once, and
        // an SDK default that changed under us must not move that bar.
        .with_retry_policy(RetryPolicy::recommended());
    let exporter = match token {
        Some(token) => exporter.with_headers(HashMap::from([(
            AUTHORIZATION_HEADER.to_string(),
            format!("Bearer {token}"),
        )])),
        None => exporter,
    };
    exporter.build().map_err(|error| error.to_string())
}

/// Wraps the exporter so an unreachable collector says so once. Every failed
/// export still returns its error to the batch processor, but only the first
/// one reaches the log: a collector that is down for hours must not turn into
/// a line per export.
#[derive(Debug)]
pub(crate) struct HintOnceExporter<E> {
    pub(crate) inner: E,
    /// One flag per reason: a first `unavailable` must not swallow a later
    /// `unauthorized`, which is the one an operator can actually fix.
    pub(crate) hinted: Arc<[AtomicBool; 4]>,
}

impl<E: SpanExporter> SpanExporter for HintOnceExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let spans = batch.len();
        let result = self.inner.export(batch).await;
        if let Err(error) = &result
            && !self.hinted[failure_reason(error).index()].swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                spans,
                error = %error,
                "telemetry export failed; spans are dropped until the export path recovers"
            );
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    // The resource reaches an exporter through this call and nowhere else: a
    // wrapper that swallows it exports spans without the service name.
    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// What one process's export pipeline has done so far.
///
/// The batch processor is the only writer, so a snapshot is a moment of the
/// pipeline and never a batch counted twice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportSnapshot {
    /// Whether an export layer was installed at all. False means the spans
    /// stay in this process: no endpoint, or an exporter that failed to build.
    pub enabled: bool,
    /// Spans the collector accepted.
    pub exported_spans: u64,
    /// Failed exports by reason; the four sum to every failed attempt.
    pub failed_timeout: u64,
    pub failed_unavailable: u64,
    /// The intake answered 401 or 403: the credential this process sends is
    /// not the one the intake accepts.
    pub failed_unauthorized: u64,
    pub failed_other: u64,
    /// Unix seconds of the last accepted export; 0 while none has landed.
    pub last_success_unix: u64,
    /// Unix seconds of the last failed attempt; 0 while none has failed.
    ///
    /// Read beside `last_success_unix`: a failure stamped later is what says
    /// the collector is unreachable now. A success and a failure in the same
    /// second compare equal, which reads as nothing lost; the next batch
    /// settles it. Both zero means nothing has been attempted.
    pub last_failure_unix: u64,
}

/// The process-wide export tally behind [`ExportSnapshot`].
///
/// One pipeline per process means one tally: [`export_layer`] publishes it, and
/// a reader (a metrics collector, in practice) takes a snapshot without
/// touching the exporter.
#[derive(Debug, Default)]
pub struct ExportStats {
    enabled: AtomicBool,
    exported_spans: AtomicU64,
    failed_timeout: AtomicU64,
    failed_unavailable: AtomicU64,
    failed_unauthorized: AtomicU64,
    failed_other: AtomicU64,
    last_success_unix: AtomicU64,
    last_failure_unix: AtomicU64,
}

impl ExportStats {
    fn mark_enabled(&self) {
        self.enabled.store(true, Ordering::Relaxed);
    }

    /// Account one export attempt. The result is read, never changed: this is a
    /// tally beside the pipeline, not a decision about it.
    fn record(&self, spans: usize, result: &OTelSdkResult) {
        match result {
            Ok(()) => {
                self.exported_spans
                    .fetch_add(spans as u64, Ordering::Relaxed);
                self.last_success_unix
                    .store(crate::timefmt::now_epoch(), Ordering::Relaxed);
            }
            Err(error) => {
                let counter = match failure_reason(error) {
                    ExportFailure::Timeout => &self.failed_timeout,
                    ExportFailure::Unavailable => &self.failed_unavailable,
                    ExportFailure::Unauthorized => &self.failed_unauthorized,
                    ExportFailure::Other => &self.failed_other,
                };
                counter.fetch_add(1, Ordering::Relaxed);
                self.last_failure_unix
                    .store(crate::timefmt::now_epoch(), Ordering::Relaxed);
            }
        }
    }

    pub fn snapshot(&self) -> ExportSnapshot {
        ExportSnapshot {
            enabled: self.enabled.load(Ordering::Relaxed),
            exported_spans: self.exported_spans.load(Ordering::Relaxed),
            failed_timeout: self.failed_timeout.load(Ordering::Relaxed),
            failed_unavailable: self.failed_unavailable.load(Ordering::Relaxed),
            failed_unauthorized: self.failed_unauthorized.load(Ordering::Relaxed),
            failed_other: self.failed_other.load(Ordering::Relaxed),
            last_success_unix: self.last_success_unix.load(Ordering::Relaxed),
            last_failure_unix: self.last_failure_unix.load(Ordering::Relaxed),
        }
    }
}

/// The process's export tally. One `OnceLock` for the same reason the tracer
/// provider is process-wide: exactly one pipeline is ever published.
pub fn export_stats() -> &'static ExportStats {
    static STATS: OnceLock<ExportStats> = OnceLock::new();
    STATS.get_or_init(ExportStats::default)
}

/// Why an export failed, as far as the tally can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExportFailure {
    Timeout,
    Unavailable,
    Unauthorized,
    Other,
}

impl ExportFailure {
    /// Its slot in the hint-once exporters' flags, one flag per reason.
    pub(super) const fn index(self) -> usize {
        match self {
            ExportFailure::Timeout => 0,
            ExportFailure::Unavailable => 1,
            ExportFailure::Unauthorized => 2,
            ExportFailure::Other => 3,
        }
    }
}

/// Classify one SDK error into the tally's four reasons.
///
/// `Timeout` is the SDK's own variant. The OTLP exporter reports an unreachable
/// or refusing collector as `Internal` with the transport error appended, so
/// "cannot reach it" has to be read off the message; anything else under
/// `Internal` (a body that would not serialize, say) is the honest `other`.
/// A connect error or a deadline that surfaced as `Internal` counts as
/// `unavailable` too: the OTLP stack does not always tell the two apart.
///
/// The intake answering 401 or 403 reads as `Internal` carrying
/// `HTTP export failed with status code: <code>`. Its status is checked first:
/// a rejected credential is a different fact from an unreachable collector, and
/// a rejection body may well contain a word from the unreachable list.
pub(super) fn failure_reason(error: &OTelSdkError) -> ExportFailure {
    match error {
        OTelSdkError::Timeout(_) => ExportFailure::Timeout,
        OTelSdkError::InternalFailure(message) => {
            let lowered = message.to_ascii_lowercase();
            let rejected = ["status code: 401", "status code: 403"]
                .iter()
                .any(|needle| lowered.contains(needle));
            if rejected {
                return ExportFailure::Unauthorized;
            }
            let unreachable = [
                "connect",
                "unavailable",
                "refused",
                "dns",
                "timeout",
                "deadline",
            ]
            .iter()
            .any(|needle| lowered.contains(needle));
            if unreachable {
                ExportFailure::Unavailable
            } else {
                ExportFailure::Other
            }
        }
        OTelSdkError::AlreadyShutdown => ExportFailure::Other,
    }
}

/// Wraps the exporter so the pipeline reports what it did. Every method
/// forwards unchanged -- the tally is taken on the way through, never instead
/// of the call -- and `set_resource` in particular must keep forwarding, or the
/// spans leave without the attributes the resource carries.
#[derive(Debug)]
pub(crate) struct CountingExporter<E> {
    pub(crate) inner: E,
    /// Where the tally goes. Injected rather than read from the process
    /// singleton, so a test can watch one exporter's accounting alone.
    pub(crate) stats: &'static ExportStats,
}

impl<E: SpanExporter> SpanExporter for CountingExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let spans = batch.len();
        // The wrapper holds the call to a deadline of its own rather than
        // trusting the processor's: a cancelled call never returns, the tally
        // is written after it returns, and the failure most worth counting is
        // the one that would leave no mark.
        let attempt = tokio::time::timeout(EXPORT_DEADLINE, self.inner.export(batch)).await;
        let result = attempt.unwrap_or(Err(OTelSdkError::Timeout(EXPORT_DEADLINE)));
        self.stats.record(spans, &result);
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}
