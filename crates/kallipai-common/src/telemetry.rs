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
//! A setup that cannot be built at all (a malformed endpoint, say) degrades:
//! one stderr line, tracing stays local, the service starts. Telemetry is not
//! load-bearing the way an event file channel is, so it does not take the
//! process down the way a broken `KALLIPAI_<SERVICE>_LOG_DIR` does.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

/// Where the collector listens, OTLP over HTTP.
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
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a shutdown may spend flushing.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The export configuration: where spans go and which sampler decides.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    endpoint: String,
    sampler: SamplerSpec,
}

impl Config {
    /// The collector endpoint, verbatim from the environment.
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
        let ratio = || arg.and_then(|raw| raw.trim().parse::<f64>().ok());
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

/// Holds the tracer provider for the life of the process and flushes the
/// queue once on the way out. Inert when nothing is exported.
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
pub fn service_layers<W>(
    config: Option<&Config>,
    filter: &EnvFilter,
    service: &str,
    dir: Option<&Path>,
    stdout: W,
) -> (impl tracing::Subscriber, Guard)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let (extra, guard) = match config {
        None => (None, Guard::inert()),
        Some(config) => match build_export_layer(config, service, filter) {
            Ok((layer, provider)) => (
                Some(layer),
                Guard {
                    provider: Some(provider),
                },
            ),
            Err(error) => {
                eprintln!(
                    "kallipai: {service} telemetry export disabled ({error}); tracing stays local"
                );
                (None, Guard::inert())
            }
        },
    };
    let subscriber = logging::service_layers_with(filter, service, dir, stdout, extra);
    (subscriber, guard)
}

/// Install the process-wide subscriber and, when an endpoint is configured,
/// the OTLP exporter that feeds it. Returns the guard; hold it for the life
/// of the process so the queue is flushed on the way out.
pub fn init_service_telemetry(filter: &EnvFilter, service: &str, dir: Option<&Path>) -> Guard {
    let config = config_from_env();
    let (subscriber, guard) =
        service_layers(config.as_ref(), filter, service, dir, std::io::stdout);
    if let Some(provider) = guard.provider.as_ref() {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        opentelemetry::global::set_tracer_provider(provider.clone());
    }
    subscriber.try_init().ok();
    guard
}

/// Build the OTLP layer and the provider behind it: export over HTTP, the
/// sampler from the configuration, one resource naming the service.
fn build_export_layer(
    config: &Config,
    service: &str,
    filter: &EnvFilter,
) -> Result<(Box<dyn Layer<Registry> + Send + Sync>, SdkTracerProvider), String> {
    let exporter = OtlpExporter::builder()
        .with_http()
        .with_endpoint(config.endpoint())
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
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

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
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
        let dead = config("http://127.0.0.1:1/v1/traces");
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
    fn a_dead_collector_does_not_slow_the_caller() {
        let captured = Captured::default();
        let dead = config("http://127.0.0.1:1/v1/traces");
        let (subscriber, guardian) = service_layers(
            Some(&dead),
            &test_filter(),
            "archeion",
            None,
            captured.clone(),
        );
        let started = std::time::Instant::now();
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..500 {
                tracing::info_span!("request").in_scope(|| {});
            }
        });
        let elapsed = started.elapsed();
        drop(guardian);
        assert!(
            elapsed < Duration::from_secs(2),
            "emitting spans must not wait on the exporter, took {elapsed:?}"
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
}
