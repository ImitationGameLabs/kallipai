//! The metrics half of the export bootstrap.
//!
//! One pipeline on the same terms as the trace half: when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` names a collector, this module builds an
//! OTLP/HTTP metric exporter behind a periodic reader, on the resource the
//! spans travel with, and publishes the meter provider process-wide. Without
//! the variable it installs nothing: the process keeps recording into the
//! SDK's no-op meter, which is what a build with no collector has always done.
//!
//! The reader is periodic, so nothing here sits in a request path. An
//! observation lands in an aggregator the SDK owns and leaves on the next
//! cycle, or on the guard's shutdown, which flushes before it stops. A
//! collector that is down costs one cycle and one log line, never latency.
//!
//! The labels are the process's own. The resource names the service, the
//! namespace every kallipai service shares, and whatever the caller passes
//! (instance id, `tagma.id`, version, host): a collector downstream receives
//! these and does not rewrite them, so a process that runs alone is described
//! without anyone configuring it there.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use opentelemetry_otlp::{MetricExporter, RetryPolicy, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};

use super::export::{
    AUTHORIZATION_HEADER, Config, EXPORT_DEADLINE, SHUTDOWN_TIMEOUT, failure_reason, path_of,
    resource,
};

/// How often the reader hands what it aggregated to the exporter.
///
/// One minute is long enough that a push costs nothing next to the traffic it
/// describes, and short enough that a dashboard's step is a dashboard's step
/// rather than a guess at an interval somebody else configured.
pub const METRICS_INTERVAL: Duration = Duration::from_secs(60);

/// The path the OTLP HTTP exporter posts metrics to; a configured base gains it.
const METRICS_PATH: &str = "/v1/metrics";

/// Holds the meter provider for the life of the process and flushes what is
/// aggregated once on the way out. Inert when nothing is exported.
///
/// Bind it (`let _metrics = ...`): a bare statement drops it on the spot and
/// shuts the reader down against a process that keeps recording.
#[must_use = "dropping the guard shuts the reader down at the end of the statement"]
pub struct MetricsGuard {
    provider: Option<SdkMeterProvider>,
}

impl MetricsGuard {
    /// A guard for a process that exports nothing.
    fn inert() -> Self {
        Self { provider: None }
    }

    /// Flush what is aggregated and stop the reader. Runs on drop; a second
    /// call does nothing. Reports whether a pipeline was published.
    pub fn shutdown(&mut self) -> bool {
        let Some(provider) = self.provider.take() else {
            return false;
        };
        let _ = provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
        true
    }
}

impl Drop for MetricsGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Build the metric pipeline for a boot and publish it process-wide.
///
/// `token` is the same credential the trace half sends, read from wherever the
/// caller keeps it. `attrs` are the caller's resource attributes, the ones
/// that tell one instance of a service from the next.
///
/// A configuration whose exporter cannot be built degrades the way the trace
/// half does: one stderr line, nothing published, the process starts. Metrics
/// are not load-bearing.
pub fn metrics_provider(
    config: Option<&Config>,
    service: &str,
    attrs: &[(&str, &str)],
    token: Option<&str>,
) -> (Option<SdkMeterProvider>, MetricsGuard) {
    let opened = metrics_provider_with(
        |endpoint| otlp_metric_exporter(endpoint, token),
        config,
        service,
        attrs,
    );
    if let Some(provider) = &opened.0 {
        opentelemetry::global::set_meter_provider(provider.clone());
    }
    opened
}

/// [`metrics_provider`] with the exporter construction injected and nothing
/// published globally, so the assembly can be exercised without a collector.
fn metrics_provider_with<E, F>(
    open: F,
    config: Option<&Config>,
    service: &str,
    attrs: &[(&str, &str)],
) -> (Option<SdkMeterProvider>, MetricsGuard)
where
    E: PushMetricExporter + 'static,
    F: FnOnce(&str) -> Result<E, String>,
{
    let Some(config) = config else {
        return (None, MetricsGuard::inert());
    };
    let exporter = match open(&metrics_endpoint(config.endpoint())) {
        Ok(exporter) => exporter,
        Err(error) => {
            eprintln!(
                "kallipai: {service} metrics export disabled ({error}); instruments stay local"
            );
            return (None, MetricsGuard::inert());
        }
    };
    let hinted = HintOnceMetricExporter {
        inner: exporter,
        hinted: Arc::new(std::array::from_fn(|_| AtomicBool::new(false))),
    };
    let reader = PeriodicReader::builder(hinted)
        .with_interval(METRICS_INTERVAL)
        .build();
    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        // The same resource the spans carry, so a metric and a trace from one
        // process agree on the service, the namespace and the instance.
        .with_resource(resource(service, attrs))
        .build();
    (
        Some(provider.clone()),
        MetricsGuard {
            provider: Some(provider),
        },
    )
}

/// The metrics URL to post to, resolved from the configured base.
///
/// Same rule as the trace half: the variable is a base, a value whose path
/// already ends with the signal path is kept as written, and a query stays on
/// the end behind the path.
pub(crate) fn metrics_endpoint(base: &str) -> String {
    let (endpoint, query) = match base.find('?') {
        Some(at) => (&base[..at], &base[at..]),
        None => (base, ""),
    };
    let endpoint = endpoint.trim_end_matches('/');
    if path_of(endpoint).ends_with(METRICS_PATH) {
        return format!("{endpoint}{query}");
    }
    format!("{endpoint}{METRICS_PATH}{query}")
}

/// The OTLP/HTTP exporter for a resolved metrics endpoint.
///
/// A token becomes `authorization: Bearer <token>` on every request; no token
/// leaves the request headers to the environment alone. The temporality is
/// cumulative, spelled out rather than left to the environment, because a
/// store that sums counters reads a delta as a total.
fn otlp_metric_exporter(endpoint: &str, token: Option<&str>) -> Result<MetricExporter, String> {
    let client = blocking_client()?;
    let exporter = MetricExporter::builder()
        .with_http()
        .with_http_client(client)
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_DEADLINE)
        // Spelled out rather than inherited, for the same reason the trace
        // exporter spells it: three retries inside the deadline must not
        // depend on a default that moved.
        .with_retry_policy(RetryPolicy::recommended())
        // Same reasoning: an operator's temporality preference in the
        // environment must not turn a running total into a delta.
        .with_temporality(Temporality::Cumulative);
    let exporter = match token {
        Some(token) => exporter.with_headers(HashMap::from([(
            AUTHORIZATION_HEADER.to_string(),
            format!("Bearer {token}"),
        )])),
        None => exporter,
    };
    exporter.build().map_err(|error| error.to_string())
}

/// The HTTP client the metrics exporter posts through.
///
/// The SDK's periodic reader exports from a thread of its own, with no
/// Tokio runtime behind it, and the async client needs a reactor on the
/// thread it sends from. A client that carries its own runtime is what
/// makes a push work there; the trace half keeps the async client because
/// it sends from the runtime. The client is built on a scratch thread, the
/// way the exporter's own builder does it, so an assertion about the
/// surrounding context cannot trip it. The one call site runs before the
/// runtime exists.
fn blocking_client() -> Result<reqwest::blocking::Client, String> {
    std::thread::Builder::new()
        .name("kallipai-metrics-http".to_string())
        .spawn(|| {
            reqwest::blocking::Client::builder()
                .timeout(EXPORT_DEADLINE)
                .build()
        })
        .map_err(|error| format!("cannot start the metrics http client: {error}"))?
        .join()
        .map_err(|_| "the metrics http client thread panicked".to_string())?
        .map_err(|error| format!("cannot build the metrics http client: {error}"))
}

/// Wraps the exporter so an unreachable collector says so once per reason.
/// Every failed cycle still returns its error to the reader; only the first of
/// a kind reaches the log.
#[derive(Debug)]
struct HintOnceMetricExporter<E> {
    inner: E,
    hinted: Arc<[AtomicBool; 4]>,
}

impl<E: PushMetricExporter> PushMetricExporter for HintOnceMetricExporter<E> {
    async fn export(&self, batch: &ResourceMetrics) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        if let Err(error) = &result
            && !self.hinted[failure_reason(error).index()].swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                error = %error,
                "metrics export failed; this cycle is dropped until the export path recovers"
            );
        }
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures_util::FutureExt as _;
    use opentelemetry::KeyValue;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::error::OTelSdkError;

    use super::*;
    use crate::telemetry::export::SamplerSpec;

    /// What the fake exporter saw, as plain values.
    #[derive(Debug, Default)]
    struct Seen {
        resources: Vec<Vec<(String, String)>>,
        names: Vec<String>,
    }

    #[derive(Debug, Clone, Default)]
    struct FakeExporter(Arc<Mutex<Seen>>);

    impl FakeExporter {
        fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
            self.0.lock().expect("the recorder is not poisoned")
        }
    }

    impl PushMetricExporter for FakeExporter {
        async fn export(&self, batch: &ResourceMetrics) -> OTelSdkResult {
            let mut seen = self.seen();
            seen.resources.push(
                batch
                    .resource()
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            );
            for scope in batch.scope_metrics() {
                for metric in scope.metrics() {
                    seen.names.push(metric.name().to_string());
                }
            }
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

    /// An exporter that refuses to be built, for the degrade path.
    struct BrokenExporter;

    impl PushMetricExporter for BrokenExporter {
        async fn export(&self, _batch: &ResourceMetrics) -> OTelSdkResult {
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

    fn config(endpoint: &str) -> Config {
        Config {
            endpoint: endpoint.to_string(),
            sampler: SamplerSpec::ParentBasedAlwaysOn,
        }
    }

    #[test]
    fn a_base_url_gains_the_metrics_path() {
        assert_eq!(
            metrics_endpoint("http://collector:4318"),
            "http://collector:4318/v1/metrics"
        );
        assert_eq!(
            metrics_endpoint("http://collector:4318/"),
            "http://collector:4318/v1/metrics"
        );
    }

    #[test]
    fn a_full_signal_url_is_kept_and_a_query_stays_behind_the_path() {
        assert_eq!(
            metrics_endpoint("http://collector:4318/v1/metrics"),
            "http://collector:4318/v1/metrics"
        );
        assert_eq!(
            metrics_endpoint("http://collector:4318/v1/metrics/"),
            "http://collector:4318/v1/metrics"
        );
        assert_eq!(
            metrics_endpoint("http://collector:4318?tenant=x"),
            "http://collector:4318/v1/metrics?tenant=x"
        );
    }

    #[test]
    fn with_no_endpoint_nothing_is_published() {
        let (provider, mut guard) = metrics_provider(None, "probe", &[], None);
        assert!(provider.is_none());
        assert!(!guard.shutdown(), "an inert guard flushes nothing");
    }

    #[test]
    fn an_exporter_that_cannot_be_built_leaves_the_process_without_a_pipeline() {
        let config = config("http://collector:4318");
        let (provider, mut guard) = metrics_provider_with(
            |_endpoint| Err::<BrokenExporter, _>("no transport".to_string()),
            Some(&config),
            "probe",
            &[],
        );
        assert!(provider.is_none());
        assert!(!guard.shutdown());
    }

    #[test]
    fn a_recorded_instrument_reaches_the_exporter_with_the_process_resource() {
        let exporter = FakeExporter::default();
        let recorder = exporter.clone();
        let config = config("http://collector:4318");
        let (provider, mut guard) = metrics_provider_with(
            move |_endpoint| Ok(recorder),
            Some(&config),
            "probe",
            &[("service.instance.id", "one"), ("tagma.id", "abc")],
        );
        let provider = provider.expect("a pipeline was published");

        let counter = provider
            .meter("probe")
            .u64_counter("probe_requests")
            .build();
        counter.add(2, &[KeyValue::new("route", "list")]);
        provider.force_flush().expect("the reader flushes");

        let seen = exporter.seen();
        assert!(
            seen.names.iter().any(|name| name == "probe_requests"),
            "the counter reached the exporter: {:?}",
            seen.names
        );
        let resource = seen.resources.last().expect("one export");
        let attribute = |key: &str| {
            resource
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(attribute("service.name"), Some("probe"));
        assert_eq!(attribute("service.namespace"), Some("kallipai"));
        assert_eq!(attribute("service.instance.id"), Some("one"));
        assert_eq!(attribute("tagma.id"), Some("abc"));

        assert!(
            guard.shutdown(),
            "a published pipeline flushes on the way out"
        );
        assert!(!guard.shutdown(), "a second shutdown is a no-op");
    }

    #[test]
    fn a_failed_cycle_does_not_take_the_process_down() {
        #[derive(Debug, Clone)]
        struct Failing;

        impl PushMetricExporter for Failing {
            async fn export(&self, _batch: &ResourceMetrics) -> OTelSdkResult {
                Err(OTelSdkError::InternalFailure(
                    "collector refused".to_string(),
                ))
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

        let config = config("http://collector:4318");
        let (provider, _guard) =
            metrics_provider_with(|_endpoint| Ok(Failing), Some(&config), "probe", &[]);
        let provider = provider.expect("a pipeline was published");
        let counter = provider.meter("probe").u64_counter("probe_drops").build();
        counter.add(1, &[]);
        let error = provider
            .force_flush()
            .expect_err("the reader reports the failed cycle");
        assert!(matches!(error, OTelSdkError::InternalFailure(_)));
    }

    /// Exports the way the reader does: a plain thread with no runtime
    /// around it and a socket on the other end. A client that needs a
    /// reactor panics on that thread, so the push never leaves and a
    /// healthy-looking process reports nothing.
    #[test]
    fn an_export_posts_from_a_thread_with_no_runtime() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let endpoint = format!(
            "http://{}/v1/metrics",
            listener.local_addr().expect("a bound address")
        );
        let collector = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("one connection");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = std::io::Read::read(&mut socket, &mut buffer).expect("a request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            std::io::Write::write_all(&mut socket, b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .expect("a response");
            String::from_utf8_lossy(&request).into_owned()
        });

        let exporter = otlp_metric_exporter(&endpoint, Some("token")).expect("an exporter");
        let export =
            std::thread::spawn(move || exporter.export(&ResourceMetrics::default()).now_or_never());
        let settled = export.join().expect("the push runs off the runtime");
        assert!(
            matches!(settled, Some(Ok(()))),
            "the first push settles where it was called: {settled:?}"
        );

        let request = collector.join().expect("the collector sees the push");
        assert!(request.starts_with("POST /v1/metrics "), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer token"),
            "{request}"
        );
    }

    /// An operator's temporality preference cannot turn a running total into
    /// a delta: the exporter asks for cumulative over the environment, so
    /// dropping that line makes this red.
    #[test]
    fn the_metrics_exporter_stays_cumulative_against_a_delta_preference() {
        let exporter = kallipai_testkit::with_env(
            &[(
                "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE",
                Some("delta"),
            )],
            || otlp_metric_exporter("http://127.0.0.1:14318", None).expect("an exporter"),
        );
        assert_eq!(exporter.temporality(), Temporality::Cumulative);
    }
}
