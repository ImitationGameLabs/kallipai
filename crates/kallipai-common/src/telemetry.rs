//! OpenTelemetry tracing bootstrap for the polis services.
//!
//! One entry point, [`init_service_telemetry`], sits on top of the logging
//! bootstrap. With `OTEL_EXPORTER_OTLP_ENDPOINT` unset or blank it installs
//! exactly the subscriber `logging::init_service_logging` installs --
//! not a disabled exporter, but the same construction path, so a deployment
//! that never sets the variable cannot tell the two apart. With the variable
//! set it adds an OTLP/HTTP span exporter and the layer that feeds it.
//!
//! The exporter never sits in a request path: spans go through a batch
//! processor whose queue is bounded, and a full queue drops spans instead of
//! blocking the caller. The queue drains on a thread with its own runtime, so
//! a collector that is down costs spans, never latency. The first failed
//! export says so once on the log channel and then stays quiet.
//!
//! A dropped span leaves no trace of its own: the SDK counts drops but reports
//! them only through its internal logs, which this crate leaves off, so a
//! deployment notices loss from the collector's intake and not from here. The
//! export layer shares the event filter, so `RUST_LOG=warn` quiets traces
//! along with events.
//!
//! A setup that cannot be built at all (a malformed endpoint, say) degrades:
//! one stderr line, tracing stays local, the service starts. Telemetry is not
//! load-bearing the way an event file channel is, so it does not take the
//! process down the way a broken `KALLIPAI_<SERVICE>_LOG_DIR` does.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::propagation::Extractor;
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
const RESOURCE_NAMESPACE: &str = "kallipai";

/// The path the OTLP HTTP exporter posts traces to; a configured base gains it.
const TRACES_PATH: &str = "/v1/traces";

/// The export configuration: where spans go and which sampler decides.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    endpoint: String,
    sampler: SamplerSpec,
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
fn traces_endpoint(base: &str) -> String {
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
fn service_layers_with_exporter<W, E, F>(
    open: F,
    config: Option<&Config>,
    filter: &EnvFilter,
    service: &str,
    dir: Option<&Path>,
    stdout: W,
) -> (impl tracing::Subscriber, Guard)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
    E: SpanExporter + 'static,
    F: FnOnce(&str) -> Result<E, String>,
{
    let (extra, guard) = export_layer_with(open, config, filter, service);
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
pub fn export_layer(
    config: Option<&Config>,
    filter: &EnvFilter,
    service: &str,
) -> (Option<Box<dyn Layer<Registry> + Send + Sync>>, Guard) {
    export_layer_with(otlp_exporter, config, filter, service)
}

/// [`export_layer`] with the exporter construction injected, so the assembly
/// can be exercised without a collector.
fn export_layer_with<E, F>(
    open: F,
    config: Option<&Config>,
    filter: &EnvFilter,
    service: &str,
) -> (Option<Box<dyn Layer<Registry> + Send + Sync>>, Guard)
where
    E: SpanExporter + 'static,
    F: FnOnce(&str) -> Result<E, String>,
{
    let Some(config) = config else {
        return (None, Guard::inert());
    };
    match build_export_layer_with(open, config, filter, service) {
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

/// The span kind a server request declares; a `Display` value renders into the
/// log prefix unquoted, which a string literal does not.
#[cfg(feature = "axum")]
const SERVER_SPAN_KIND: &str = "server";

/// The route name a request gets when it matched no route at all.
#[cfg(feature = "axum")]
const UNMATCHED_ROUTE: &str = "unmatched";

/// The server span for one request: the method and the route template as the
/// name (`GET /v1/files/{id}`), the OpenTelemetry HTTP attributes on the span.
///
/// The name deliberately avoids the raw path: an id in a span name scatters one
/// endpoint across a thousand names and defeats aggregation. The path still
/// travels as `url.path`, capped at 256 characters, so a trace
/// can answer "which room" without a name nothing can aggregate. The status is
/// recorded once the response exists, and a server-side failure also carries an
/// error type and the failed status, so a trace can be filtered for them.
///
/// A 4xx response only records `http.response.status_code`; the semantic
/// conventions reserve the error status and `error.type` for a 5xx.
///
/// Only `otel.name` and `otel.kind` are tracing fields: those two render into
/// the prefix of every event inside the request, and the rest is worth nothing
/// there. Everything else is a span attribute, which costs the log no space.
///
/// The span ends when the response head exists. For a streaming body (SSE, a
/// download) the duration is time to the first byte rather than the transfer,
/// and a body that fails mid-stream is not recorded as an error on the span.
///
/// A request that arrives with a W3C `traceparent` adopts it as its parent, so
/// one trace spans the services a call crosses.
///
/// Mount it with [`with_server_span`]; every axum router mounts this one layer,
/// so a trace reads the same whichever service answered.
#[cfg(feature = "axum")]
pub async fn server_span(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use opentelemetry::trace::Status;
    use tracing::Instrument as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or(UNMATCHED_ROUTE, axum::extract::MatchedPath::as_str);
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let name = format!("{method} {route}");
    let span = tracing::info_span!(
        "request",
        otel.name = %name,
        otel.kind = %SERVER_SPAN_KIND,
    );
    // The parent travels with the request; adopting it before the span is
    // entered is what lets the OpenTelemetry layer build the span under it.
    let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderCarrier(request.headers()))
    });
    // A span the OpenTelemetry layer does not own cannot take a parent; it then
    // stays a root, which is what a request without a caller's context is.
    let _ = span.set_parent(parent);
    span.set_attribute("http.request.method", method.as_str().to_string());
    span.set_attribute("http.route", route.to_string());
    span.set_attribute("url.path", capped(&path, URL_PATH_LIMIT).to_string());
    let response = next.run(request).instrument(span.clone()).await;
    let status = response.status();
    span.set_attribute("http.response.status_code", i64::from(status.as_u16()));
    if status.is_server_error() {
        span.set_attribute("error.type", format!("http_{}", status.as_u16()));
        span.set_status(Status::error(status.to_string()));
    }
    response
}

/// The request headers as a propagator sees them.
#[cfg(feature = "axum")]
struct HeaderCarrier<'a>(&'a axum::http::HeaderMap);

#[cfg(feature = "axum")]
impl Extractor for HeaderCarrier<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(axum::http::HeaderName::as_str).collect()
    }
}

/// Cap for `url.path`. A path is caller data, and a deeply nested one must not
/// grow every span it lands on; truncation keeps whole characters.
#[cfg(feature = "axum")]
const URL_PATH_LIMIT: usize = 256;

#[cfg(feature = "axum")]
fn capped(value: &str, limit: usize) -> &str {
    match value.char_indices().nth(limit) {
        Some((at, _)) => &value[..at],
        None => value,
    }
}

/// Put [`server_span`] on a finished router, outermost, so the span covers the
/// layers inside it (body limit, CORS, CSRF) as well as the handler.
///
/// Whoever finishes assembling the router mounts it, which for some services
/// is `main` and for others the module that builds and serves the router. A new
/// service gets its span from one call at its own assembly point.
#[cfg(feature = "axum")]
pub fn with_server_span<S>(router: axum::Router<S>) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(axum::middleware::from_fn(server_span))
}

/// Build the OTLP layer and the provider behind it: export over HTTP, the
/// sampler from the configuration, one resource naming the service and the
/// namespace it belongs to. `with_service_name` overrides
/// `OTEL_SERVICE_NAME`, so the service argument is where a rename happens and
/// the environment cannot set the name behind its back.
fn build_export_layer_with<E, F>(
    open: F,
    config: &Config,
    filter: &EnvFilter,
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
        .with_filter(filter.clone())
        .boxed();
    Ok((layer, provider))
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
struct HintOnceExporter<E> {
    inner: E,
    hinted: Arc<AtomicBool>,
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    struct CapturedWriter<'a>(std::sync::MutexGuard<'a, Vec<u8>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = CapturedWriter<'a>;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedWriter(self.0.lock().unwrap())
        }
    }

    impl std::io::Write for CapturedWriter<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    fn text(captured: &Captured) -> String {
        String::from_utf8_lossy(&captured.0.lock().unwrap().clone()).into_owned()
    }

    /// A log line from the level word on: the shapes under test differ only in
    /// the clock prefix, which no test can pin.
    fn after_level(line: &str) -> &str {
        line.find("INFO").map_or(line, |at| &line[at..])
    }

    fn test_filter() -> EnvFilter {
        EnvFilter::new("info")
    }

    fn config(endpoint: &str) -> Config {
        Config {
            endpoint: endpoint.to_string(),
            sampler: SamplerSpec::ParentBasedAlwaysOn,
        }
    }

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn an_unset_or_blank_endpoint_means_no_export() {
        assert!(config_from_lookup(lookup(&[])).is_none());
        assert!(config_from_lookup(lookup(&[(ENDPOINT_VAR, "")])).is_none());
        assert!(config_from_lookup(lookup(&[(ENDPOINT_VAR, "   ")])).is_none());
    }

    #[test]
    fn the_endpoint_is_kept_and_the_sampler_defaults_to_parent_based() {
        let config =
            config_from_lookup(lookup(&[(ENDPOINT_VAR, "http://127.0.0.1:4318/v1/traces")]))
                .expect("a set endpoint yields a configuration");
        assert_eq!(config.endpoint(), "http://127.0.0.1:4318/v1/traces");
        assert_eq!(config.sampler(), SamplerSpec::ParentBasedAlwaysOn);
    }

    #[test]
    fn the_sampler_override_is_read_with_its_argument() {
        let always_off = config_from_lookup(lookup(&[
            (ENDPOINT_VAR, "http://collector:4318"),
            (SAMPLER_VAR, "always_off"),
        ]))
        .expect("a set endpoint yields a configuration");
        assert_eq!(always_off.sampler(), SamplerSpec::AlwaysOff);

        let ratio = config_from_lookup(lookup(&[
            (ENDPOINT_VAR, "http://collector:4318"),
            (SAMPLER_VAR, "parentbased_traceidratio"),
            (SAMPLER_ARG_VAR, "0.25"),
        ]))
        .expect("a set endpoint yields a configuration");
        assert_eq!(ratio.sampler(), SamplerSpec::ParentBasedTraceIdRatio(0.25));
    }

    #[test]
    fn an_unknown_sampler_falls_back_to_the_default() {
        let config = config_from_lookup(lookup(&[
            (ENDPOINT_VAR, "http://collector:4318"),
            (SAMPLER_VAR, "jaeger_remote"),
        ]))
        .expect("a set endpoint yields a configuration");
        assert_eq!(config.sampler(), SamplerSpec::ParentBasedAlwaysOn);
    }

    #[test]
    fn without_a_config_the_bootstrap_matches_the_logging_one() {
        let telemetry_out = Captured::default();
        let (subscriber, guardian) = service_layers(
            None,
            &test_filter(),
            "archeion",
            None,
            telemetry_out.clone(),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("kallipai telemetry marker")
        });
        drop(guardian);

        let logging_out = Captured::default();
        let subscriber =
            logging::service_layers(&test_filter(), "archeion", None, logging_out.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("kallipai telemetry marker")
        });

        assert_eq!(
            after_level(&text(&logging_out)),
            after_level(&text(&telemetry_out)),
            "the plain stack must render the event exactly as the logging bootstrap does"
        );
    }

    #[test]
    fn a_config_keeps_the_log_line_and_adds_the_export_layer() {
        let dead = config("http://127.0.0.1:1");
        let enabled_out = Captured::default();
        let (subscriber, mut guardian) = service_layers(
            Some(&dead),
            &test_filter(),
            "archeion",
            None,
            enabled_out.clone(),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("kallipai telemetry marker")
        });

        let plain_out = Captured::default();
        let (subscriber, _guardian) =
            service_layers(None, &test_filter(), "archeion", None, plain_out.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("kallipai telemetry marker")
        });

        assert_eq!(
            after_level(&text(&plain_out)),
            after_level(&text(&enabled_out)),
            "the export layer must not change how events are rendered"
        );
        assert!(guardian.shutdown(), "a configured guard owns a provider");
        assert!(
            !guardian.shutdown(),
            "a second shutdown finds nothing left to flush"
        );
    }

    #[test]
    fn a_refused_export_does_not_reach_the_caller() {
        let captured = Captured::default();
        let dead = config("http://127.0.0.1:1");
        let (subscriber, mut guardian) = service_layers(
            Some(&dead),
            &test_filter(),
            "archeion",
            None,
            captured.clone(),
        );
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..500 {
                tracing::info_span!("request").in_scope(|| {});
            }
        });
        assert!(
            guardian.shutdown(),
            "the guard owns the provider whether or not the collector answers"
        );
    }

    #[derive(Debug)]
    struct FailingExporter;

    impl SpanExporter for FailingExporter {
        async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
            Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                "collector unreachable".to_string(),
            ))
        }
    }

    #[derive(Clone, Default)]
    struct WarnCounter(Arc<std::sync::atomic::AtomicUsize>);

    impl<S: tracing::Subscriber> Layer<S> for WarnCounter {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[tokio::test]
    async fn a_failing_export_says_so_once() {
        let counter = WarnCounter::default();
        let subscriber = tracing_subscriber::registry().with(counter.clone());
        let exporter = HintOnceExporter {
            inner: FailingExporter,
            hinted: Arc::new(AtomicBool::new(false)),
        };
        {
            let _quiet = tracing::subscriber::set_default(subscriber);
            for _ in 0..3 {
                assert!(exporter.export(Vec::new()).await.is_err());
            }
        }
        assert_eq!(
            counter.0.load(Ordering::Relaxed),
            1,
            "a collector that stays down must not log a line per export"
        );
    }

    /// Witnesses the opening fields of every span and every later recording, so
    /// a middleware's span can be read back without an exporter.
    #[cfg(feature = "axum")]
    #[derive(Clone, Default)]
    struct SpanFields(Arc<Mutex<Vec<(String, String)>>>);

    #[cfg(feature = "axum")]
    #[derive(Default)]
    struct FieldVisitor(Vec<(String, String)>);

    #[cfg(feature = "axum")]
    impl tracing::field::Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let value = format!("{value:?}");
            self.0.push((field.name().to_string(), value));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
    }

    #[cfg(feature = "axum")]
    impl SpanFields {
        fn absorb(&self, record: impl FnOnce(&mut FieldVisitor)) {
            let mut visitor = FieldVisitor::default();
            record(&mut visitor);
            self.0.lock().unwrap().extend(visitor.0);
        }

        /// The last value recorded under `name`, whether at span open or later.
        fn get(&self, name: &str) -> Option<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[cfg(feature = "axum")]
    impl<S: tracing::Subscriber> Layer<S> for SpanFields {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.absorb(|visitor| attrs.record(visitor));
        }

        fn on_record(
            &self,
            _id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.absorb(|visitor| values.record(visitor));
        }
    }

    /// Drive one request through the span layer and return the status plus the
    /// fields the span ended up carrying.
    #[cfg(feature = "axum")]
    async fn drive(app: axum::Router, uri: &str) -> (axum::http::StatusCode, SpanFields) {
        use tower::ServiceExt as _;

        let fields = SpanFields::default();
        let subscriber = tracing_subscriber::registry().with(fields.clone());
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let request = axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .expect("a well-formed request");
        let response = with_server_span(app)
            .oneshot(request)
            .await
            .expect("the router answers");
        (response.status(), fields)
    }

    #[cfg(feature = "axum")]
    #[tokio::test]
    async fn the_request_span_declares_its_name_and_kind_and_nothing_else() {
        let app = axum::Router::new().route("/files/{id}", axum::routing::get(|| async { "ok" }));
        let (status, fields) = drive(app, "/files/abc").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(fields.get("otel.name").as_deref(), Some("GET /files/{id}"));
        assert_eq!(fields.get("otel.kind").as_deref(), Some("server"));
        // Every other fact is a span attribute: a tracing field is rendered into
        // the prefix of each event inside the request, where it only crowds.
        for absent in [
            "http.request.method",
            "http.route",
            "url.path",
            "http.response.status_code",
            "error.type",
        ] {
            assert_eq!(
                fields.get(absent),
                None,
                "{absent} must not be a tracing field"
            );
        }
    }

    #[cfg(feature = "axum")]
    #[tokio::test]
    async fn a_request_that_matched_no_route_is_named_unmatched() {
        let app = axum::Router::new().fallback(|| async { "no such path" });
        let (status, fields) = drive(app, "/nowhere/at/all").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(fields.get("otel.name").as_deref(), Some("GET unmatched"));
    }

    #[cfg(feature = "axum")]
    #[tokio::test]
    async fn an_unmatched_request_carries_the_unmatched_route_as_an_attribute() {
        let app = axum::Router::new().fallback(|| async { "no such path" });
        let exported = exported_spans(
            app,
            request_for("/nowhere/at/all", &[]),
            "http://collector:4318",
            "http://collector:4318/v1/traces",
        )
        .await;
        let span = exported
            .spans
            .first()
            .expect("the fallback produced one span");
        assert_eq!(attribute(span, "http.route").as_deref(), Some("unmatched"));
    }

    #[test]
    fn export_layer_stays_inert_without_a_configuration() {
        let (layer, mut guard) = export_layer(None, &test_filter(), "tagma");
        assert!(layer.is_none(), "an unconfigured service exports nothing");
        assert!(!guard.shutdown(), "an inert guard owns no provider");
    }

    #[test]
    fn export_layer_carries_the_layer_and_the_guard_when_configured() {
        let dead = config("http://127.0.0.1:1");
        let (layer, mut guard) = export_layer(Some(&dead), &test_filter(), "tagma");
        assert!(
            layer.is_some(),
            "a configured service exports through this layer"
        );
        let carries_tracecontext = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.fields().any(|field| field == "traceparent")
        });
        assert!(
            carries_tracecontext,
            "the export layer publishes the W3C propagator"
        );
        assert!(guard.shutdown(), "a configured guard owns a provider");
    }

    #[test]
    fn a_base_endpoint_gains_the_traces_path_and_a_full_signal_one_is_kept() {
        assert_eq!(
            traces_endpoint("http://collector:4318"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            traces_endpoint("http://collector:4318/"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            traces_endpoint("http://collector:4318/v1/traces"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            traces_endpoint("https://otel.example.com/otlp"),
            "https://otel.example.com/otlp/v1/traces"
        );
        assert_eq!(
            traces_endpoint("http://collector:4318?x=1"),
            "http://collector:4318/v1/traces?x=1"
        );
        assert_eq!(
            traces_endpoint("http://collector:4318/v1/traces/"),
            "http://collector:4318/v1/traces"
        );
    }

    #[test]
    fn an_out_of_range_ratio_keeps_the_default_sampler() {
        for ratio in ["1.5", "-1", "NaN", "inf"] {
            let config = config_from_lookup(lookup(&[
                (ENDPOINT_VAR, "http://collector:4318"),
                (SAMPLER_VAR, "parentbased_traceidratio"),
                (SAMPLER_ARG_VAR, ratio),
            ]))
            .expect("a set endpoint yields a configuration");
            assert_eq!(
                config.sampler(),
                SamplerSpec::ParentBasedAlwaysOn,
                "a ratio of {ratio} must not pin sampling either way"
            );
        }
        let config = config_from_lookup(lookup(&[
            (ENDPOINT_VAR, "http://collector:4318"),
            (SAMPLER_VAR, "parentbased_traceidratio"),
            (SAMPLER_ARG_VAR, "0.25"),
        ]))
        .expect("a set endpoint yields a configuration");
        assert_eq!(config.sampler(), SamplerSpec::ParentBasedTraceIdRatio(0.25));
    }

    /// The far side of an export: the spans the SDK handed to an exporter, and
    /// the resource it named them with.
    #[derive(Clone, Debug, Default)]
    struct RecordingExporter {
        spans: Arc<Mutex<Vec<SpanData>>>,
        resource: Arc<Mutex<Option<Resource>>>,
    }

    impl SpanExporter for RecordingExporter {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.spans.lock().unwrap().extend(batch);
            Ok(())
        }

        fn set_resource(&mut self, resource: &Resource) {
            *self.resource.lock().unwrap() = Some(resource.clone());
        }
    }

    /// What one request left behind at the export boundary.
    struct Exported {
        spans: Vec<SpanData>,
        resource: Option<Resource>,
    }

    #[cfg(feature = "axum")]
    fn request_for(uri: &str, headers: &[(&str, &str)]) -> axum::http::Request<axum::body::Body> {
        let mut builder = axum::http::Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder
            .body(axum::body::Body::empty())
            .expect("a well-formed request")
    }

    fn attribute(span: &SpanData, key: &str) -> Option<String> {
        span.attributes
            .iter()
            .find(|pair| pair.key.as_str() == key)
            .map(|pair| pair.value.to_string())
    }

    fn resource_attribute(exported: &Exported, name: &str) -> Option<String> {
        let key = opentelemetry::Key::new(name.to_string());
        exported
            .resource
            .as_ref()
            .and_then(|resource| resource.get(&key))
            .map(|value| value.to_string())
    }

    /// Drive one request through the whole stack -- event layers, span layer and
    /// a recording exporter -- and hand back what crossed the export boundary.
    #[cfg(feature = "axum")]
    async fn exported_spans(
        app: axum::Router,
        request: axum::http::Request<axum::body::Body>,
        configured: &str,
        expected_endpoint: &str,
    ) -> Exported {
        use tower::ServiceExt as _;

        let recording = RecordingExporter::default();
        let handed = recording.clone();
        let expected = expected_endpoint.to_string();
        let config = config(configured);
        let (subscriber, mut guard) = service_layers_with_exporter(
            move |endpoint: &str| {
                assert_eq!(
                    endpoint, expected,
                    "the exporter is built for the resolved traces URL"
                );
                Ok(handed)
            },
            Some(&config),
            &test_filter(),
            "archeion",
            None,
            Captured::default(),
        );
        {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            let response = with_server_span(app)
                .oneshot(request)
                .await
                .expect("the router answers");
            assert!(
                response.status().is_success() || response.status().is_server_error(),
                "the request completed inside the composed stack"
            );
        }
        assert!(guard.shutdown(), "a configured guard owns a provider");
        Exported {
            spans: recording.spans.lock().unwrap().clone(),
            resource: recording.resource.lock().unwrap().clone(),
        }
    }

    #[cfg(feature = "axum")]
    #[test]
    fn a_long_path_is_capped_for_the_span_attribute() {
        let short = "/files/abc";
        assert_eq!(capped(short, URL_PATH_LIMIT), short);

        let long = format!("/files/{}", "a".repeat(URL_PATH_LIMIT * 2));
        let capped_long = capped(&long, URL_PATH_LIMIT);
        assert_eq!(capped_long.chars().count(), URL_PATH_LIMIT);
        assert!(long.starts_with(capped_long), "the prefix is what is kept");
    }

    #[cfg(feature = "axum")]
    #[tokio::test]
    async fn a_request_span_reaches_the_exporter() {
        let app = axum::Router::new().route("/files/{id}", axum::routing::get(|| async { "ok" }));
        let exported = exported_spans(
            app,
            request_for("/files/abc", &[]),
            "http://collector:4318",
            "http://collector:4318/v1/traces",
        )
        .await;

        assert_eq!(exported.spans.len(), 1, "one request, one exported span");
        let span = &exported.spans[0];
        assert_eq!(span.name, "GET /files/{id}");
        assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Server);
        assert_eq!(
            attribute(span, "http.request.method").as_deref(),
            Some("GET")
        );
        assert_eq!(
            attribute(span, "http.route").as_deref(),
            Some("/files/{id}")
        );
        assert_eq!(attribute(span, "url.path").as_deref(), Some("/files/abc"));
        assert_eq!(
            attribute(span, "http.response.status_code").as_deref(),
            Some("200")
        );
        assert_eq!(
            attribute(span, "error.type"),
            None,
            "an answered request is not an error"
        );
        assert_eq!(
            resource_attribute(&exported, "service.name").as_deref(),
            Some("archeion")
        );
        assert_eq!(
            resource_attribute(&exported, "service.namespace").as_deref(),
            Some("kallipai")
        );
        assert_eq!(span.status, opentelemetry::trace::Status::Unset);
    }

    #[cfg(feature = "axum")]
    #[tokio::test]
    async fn a_failed_request_reaches_the_exporter_as_an_error() {
        let app = axum::Router::new().route(
            "/boom",
            axum::routing::get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let exported = exported_spans(
            app,
            request_for("/boom", &[]),
            "http://collector:4318/v1/traces",
            "http://collector:4318/v1/traces",
        )
        .await;

        let span = exported
            .spans
            .first()
            .expect("the failed request produced one span");
        assert_eq!(
            attribute(span, "http.response.status_code").as_deref(),
            Some("500")
        );
        assert_eq!(attribute(span, "error.type").as_deref(), Some("http_500"));
        assert!(matches!(
            span.status,
            opentelemetry::trace::Status::Error { .. }
        ));
    }

    #[cfg(feature = "axum")]
    #[tokio::test]
    async fn a_traceparent_header_becomes_the_parent_of_the_request_span() {
        let app = axum::Router::new().route("/files/{id}", axum::routing::get(|| async { "ok" }));
        let exported = exported_spans(
            app.clone(),
            request_for(
                "/files/abc",
                &[(
                    "traceparent",
                    "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
                )],
            ),
            "http://collector:4318",
            "http://collector:4318/v1/traces",
        )
        .await;

        let span = exported
            .spans
            .first()
            .expect("the traced request produced one span");
        assert_eq!(
            span.span_context.trace_id(),
            opentelemetry::trace::TraceId::from_hex("0af7651916cd43dd8448eb211c80319c")
                .expect("a trace id")
        );
        assert_eq!(
            span.parent_span_id,
            opentelemetry::trace::SpanId::from_hex("b7ad6b7169203331").expect("a span id")
        );

        let exported = exported_spans(
            app,
            request_for("/files/abc", &[]),
            "http://collector:4318",
            "http://collector:4318/v1/traces",
        )
        .await;
        let span = exported
            .spans
            .first()
            .expect("the plain request produced one span");
        assert_eq!(
            span.parent_span_id,
            opentelemetry::trace::SpanId::INVALID,
            "without the header the span starts a trace"
        );
    }

    #[cfg(feature = "axum")]
    #[tokio::test]
    async fn the_log_line_inside_a_request_carries_the_span_name_and_kind() {
        use tower::ServiceExt as _;

        let out = Captured::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(out.clone())
                .with_ansi(false),
        );
        let app = axum::Router::new().route(
            "/files/{id}",
            axum::routing::get(|| async {
                tracing::info!("inside the handler");
            }),
        );
        {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            let response = with_server_span(app)
                .oneshot(request_for("/files/abc", &[]))
                .await
                .expect("the router answers");
            assert_eq!(response.status(), axum::http::StatusCode::OK);
        }

        let rendered = text(&out);
        assert!(
            rendered.contains("request{otel.name=GET /files/{id} otel.kind=server}"),
            "the prefix names the request and nothing else: {rendered}"
        );
        for absent in ["http.route", "url.path", "http.request.method"] {
            assert!(
                !rendered.contains(absent),
                "{absent} belongs to the span, not to the log line: {rendered}"
            );
        }
    }
}
