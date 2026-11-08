//! The exporter half: configuration, the layer that feeds a collector, and the
//! bootstrap entry point.
//!
//! The module document in [`super`] covers the shape of a deployment. Nothing
//! here sits in a request path; a service that never sets the endpoint does not
//! compile this file at all (feature `telemetry`) and still gets the
//! propagation half.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter as OtlpExporter, WithExportConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
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
/// Upper bound on one export call, so a stalled collector cannot pin the queue.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(3);
/// Passed to `shutdown_with_timeout`, which the SDK ignores: a shutdown flushes
/// and then waits for one more export, so the real bound is about twice
/// `EXPORT_TIMEOUT` -- six seconds, inside the ten a default container stop
/// before the process is killed.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);

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
fn path_of(endpoint: &str) -> &str {
    let authority = match endpoint.find("://") {
        Some(sep) => &endpoint[sep + 3..],
        None => endpoint,
    };
    authority.find('/').map_or("", |at| &authority[at..])
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
    dir: Option<&Path>,
    stdout: W,
) -> (impl tracing::Subscriber, Guard)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    service_layers_with_exporter(otlp_exporter, config, filter, service, dir, stdout)
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
    dir: Option<&Path>,
    stdout: W,
) -> (impl tracing::Subscriber + use<W, E, F>, Guard)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
    E: SpanExporter + 'static,
    F: FnOnce(&str) -> Result<E, String>,
{
    let (extra, guard) = export_layer_with(open, config, service);
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
pub fn export_layer(
    config: Option<&Config>,
    service: &str,
) -> (Option<Box<dyn Layer<Registry> + Send + Sync>>, Guard) {
    export_layer_with(otlp_exporter, config, service)
}

/// [`export_layer`] with the exporter construction injected, so the assembly
/// can be exercised without a collector.
fn export_layer_with<E, F>(
    open: F,
    config: Option<&Config>,
    service: &str,
) -> (Option<Box<dyn Layer<Registry> + Send + Sync>>, Guard)
where
    E: SpanExporter + 'static,
    F: FnOnce(&str) -> Result<E, String>,
{
    let Some(config) = config else {
        return (None, Guard::inert());
    };
    match build_export_layer_with(open, config, service) {
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
pub fn init_service_telemetry(filter: &EnvFilter, service: &str, dir: Option<&Path>) -> Guard {
    let config = config_from_env();
    let (subscriber, guard) =
        service_layers(config.as_ref(), filter, service, dir, std::io::stdout);
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
        .with_max_export_timeout(EXPORT_TIMEOUT)
        .build();
    let processor = BatchSpanProcessor::builder(
        HintOnceExporter {
            inner: exporter,
            hinted: Arc::new(AtomicBool::new(false)),
        },
        runtime::TokioCurrentThread,
    )
    .with_batch_config(batch)
    .build();
    let provider = SdkTracerProvider::builder()
        .with_sampler(config.sampler().sampler())
        .with_resource(
            Resource::builder()
                .with_service_name(service.to_string())
                .with_attribute(KeyValue::new("service.namespace", RESOURCE_NAMESPACE))
                .build(),
        )
        .with_span_processor(processor)
        .build();
    let tracer = provider.tracer(service.to_string());
    let layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(span_filter())
        .boxed();
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
fn otlp_exporter(endpoint: &str) -> Result<OtlpExporter, String> {
    OtlpExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())
}

/// Wraps the exporter so an unreachable collector says so once. Every failed
/// export still returns its error to the batch processor, but only the first
/// one reaches the log: a collector that is down for hours must not turn into
/// a line per export.
#[derive(Debug)]
pub(crate) struct HintOnceExporter<E> {
    pub(crate) inner: E,
    pub(crate) hinted: Arc<AtomicBool>,
}

impl<E: SpanExporter> SpanExporter for HintOnceExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let spans = batch.len();
        let result = self.inner.export(batch).await;
        if let Err(error) = &result
            && !self.hinted.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                spans,
                error = %error,
                "telemetry export failed; spans are dropped while the collector stays unreachable"
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
