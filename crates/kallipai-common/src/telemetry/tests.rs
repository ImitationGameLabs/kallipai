use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::*;

use super::*;
use crate::logging;
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
    let config = config_from_lookup(lookup(&[(ENDPOINT_VAR, "http://127.0.0.1:4318/v1/traces")]))
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
        &[],
        None,
        telemetry_out.clone(),
    );
    tracing::subscriber::with_default(subscriber, || tracing::info!("kallipai telemetry marker"));
    drop(guardian);

    let logging_out = Captured::default();
    let subscriber = logging::service_layers(&test_filter(), "archeion", None, logging_out.clone());
    tracing::subscriber::with_default(subscriber, || tracing::info!("kallipai telemetry marker"));

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
        &[],
        None,
        enabled_out.clone(),
    );
    tracing::subscriber::with_default(subscriber, || tracing::info!("kallipai telemetry marker"));

    let plain_out = Captured::default();
    let (subscriber, _guardian) = service_layers(
        None,
        &test_filter(),
        "archeion",
        &[],
        None,
        plain_out.clone(),
    );
    tracing::subscriber::with_default(subscriber, || tracing::info!("kallipai telemetry marker"));

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
        &[],
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
        hinted: Arc::new(std::array::from_fn(|_| AtomicBool::new(false))),
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

/// Says `unavailable` first and `unauthorized` after: two facts an operator
/// reacts to differently, and the second one arrives after the first is old
/// news.
#[derive(Debug, Clone)]
struct ReasonChangeExporter {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl SpanExporter for ReasonChangeExporter {
    async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        Err(if call == 0 {
            OTelSdkError::InternalFailure("connection refused".to_string())
        } else {
            OTelSdkError::InternalFailure("HTTP export failed with status code: 401".to_string())
        })
    }
}

#[tokio::test]
async fn a_later_unauthorized_is_not_swallowed_by_an_earlier_unavailable() {
    let counter = WarnCounter::default();
    let subscriber = tracing_subscriber::registry().with(counter.clone());
    let exporter = HintOnceExporter {
        inner: ReasonChangeExporter {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
        hinted: Arc::new(std::array::from_fn(|_| AtomicBool::new(false))),
    };
    {
        let _quiet = tracing::subscriber::set_default(subscriber);
        for _ in 0..4 {
            assert!(exporter.export(Vec::new()).await.is_err());
        }
    }
    assert_eq!(
        counter.0.load(Ordering::Relaxed),
        2,
        "one line per reason: the rejection must be said even after the outage"
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
    let (layer, mut guard) = export_layer(None, "tagma", &[]);
    assert!(layer.is_none(), "an unconfigured service exports nothing");
    assert!(!guard.shutdown(), "an inert guard owns no provider");
}

#[test]
fn export_layer_carries_the_layer_and_the_guard_when_configured() {
    let dead = config("http://127.0.0.1:1");
    let (layer, mut guard) = export_layer(Some(&dead), "tagma", &[]);
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

// The exporter and its readers live in [`super::test_observability`]: one
// implementation, shared with a dependent crate that reads the same
// boundary. What stays here is the assembly of the production stack.
use super::test_observability::RecordingExporter;

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
        &[],
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
        spans: recording.batches(),
        resource: recording.resource(),
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

/// A background attempt starts a trace of its own, and the span carries how
/// the attempt ended. The parent is `None` at construction, so a span the
/// task happens to be in cannot adopt the attempt.
#[tokio::test]
async fn a_background_span_starts_its_own_trace_and_carries_its_verdict() {
    let recording = RecordingExporter::default();
    let handed = recording.clone();
    let config = config("http://collector:4318");
    let (subscriber, mut guard) = service_layers_with_exporter(
        move |_endpoint: &str| Ok(handed),
        Some(&config),
        &test_filter(),
        "tagma",
        &[],
        None,
        Captured::default(),
    );
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let caller = tracing::info_span!(target: SPAN_TARGET, "caller");
        let _entered = caller.enter();
        let span = spans::background_span("repull_profile_source");
        Settle::new(&span).failed("retrying");
    }
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.batches();
    assert_eq!(
        spans.len(),
        2,
        "the span the task was in and the attempt: {:?}",
        spans
            .iter()
            .map(|span| span.name.clone())
            .collect::<Vec<_>>()
    );
    let span = spans
        .iter()
        .find(|span| span.name == "repull_profile_source")
        .expect("the attempt exports a span of its own");
    let caller = spans
        .iter()
        .find(|span| span.name == "caller")
        .expect("the span the task was in exports too");
    assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Internal);
    assert_eq!(
        span.parent_span_id,
        opentelemetry::trace::SpanId::INVALID,
        "a root even with a span around the task"
    );
    assert_ne!(
        span.span_context.trace_id(),
        caller.span_context.trace_id(),
        "the attempt starts a trace rather than joining the one around it"
    );
    assert_eq!(
        attribute(span, "kallipai.outcome").as_deref(),
        Some("failed")
    );
    assert_eq!(attribute(span, "error.type").as_deref(), Some("retrying"));
}

/// The exemption is a promise on the wire: the scrape leaves no span, while
/// the path beside it still opens one, so a trace of the process does not
/// carry the requests that measure it.
#[cfg(feature = "axum")]
#[tokio::test]
async fn an_exempt_path_opens_no_server_span_and_its_neighbour_still_does() {
    use tower::ServiceExt as _;

    let app = axum::Router::new()
        .route("/metrics", axum::routing::get(|| async { "scrape" }))
        .route("/other", axum::routing::get(|| async { "work" }));
    let app = with_server_span_except(app, &["/metrics"]);

    let (subscriber, mut guard, recording) = recording_with(test_filter());
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        for path in ["/metrics", "/other"] {
            let response = app
                .clone()
                .oneshot(request_for(path, &[]))
                .await
                .expect("the router answers");
            assert_eq!(response.status(), axum::http::StatusCode::OK);
        }
    }
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.batches();
    assert_eq!(
        spans.len(),
        1,
        "the exempt path opens no span: {:?}",
        spans
            .iter()
            .map(|span| span.name.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        attribute(&spans[0], "http.route").as_deref(),
        Some("/other")
    );
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
/// The parent read itself, asserted where it lives: both faces call it,
/// and a break here would cost every face its trace linkage.
mod parent_adoption {
    use super::super::parent_from_headers;
    use opentelemetry::trace::{SpanId, TraceContextExt as _, TraceId};

    const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
    const SPAN: &str = "b7ad6b7169203331";

    fn headers(value: &str) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "traceparent",
            value.parse().expect("a header value the test wrote"),
        );
        headers
    }

    #[test]
    fn a_traceparent_header_names_the_parent_verbatim() {
        // The read goes through the process-wide propagator the bootstrap
        // publishes; a test publishes it too, so this assertion does not
        // depend on another test in the binary having done so.
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );

        let context = parent_from_headers(&headers(&format!("00-{TRACE}-{SPAN}-01")));
        let span = context.span();
        let span_context = span.span_context();
        assert!(span_context.is_valid(), "an adopted context is a valid one");
        assert_eq!(
            span_context.trace_id(),
            TraceId::from_hex(TRACE).expect("a trace id")
        );
        assert_eq!(
            span_context.span_id(),
            SpanId::from_hex(SPAN).expect("a span id")
        );
    }

    #[test]
    fn no_header_leaves_the_context_empty() {
        let context = parent_from_headers(&http::HeaderMap::new());
        assert!(
            !context.span().span_context().is_valid(),
            "no caller context, no parent"
        );
    }

    #[test]
    fn a_malformed_header_is_ignored_rather_than_panicking() {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );

        for value in [
            "",
            "garbage",
            "00-xyz-123-01",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331",
            "01-0af7651916cd43dd8448eb211c80319c-0000000000000000-01",
            "00-00000000000000000000000000000000-b7ad6b7169203331-01",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01-extra",
            "ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        ] {
            let context = parent_from_headers(&headers(value));
            assert!(
                !context.span().span_context().is_valid(),
                "{value:?} names no parent"
            );
        }
    }
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

/// A real service on loopback: an axum router under the request span, the
/// status its one route answers with, and the `traceparent` the handler saw.
/// The subscriber is installed thread-locally (`set_default`) and the
/// spawned service runs on the same single-threaded runtime, so its span is
/// visible; a multi-threaded flavor would put it out of sight and fail here
/// for the wrong reason.
#[cfg(feature = "axum")]
async fn service_on_loopback(
    route: &'static str,
    status: axum::http::StatusCode,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<Option<String>>>,
) {
    let seen = Arc::new(Mutex::new(None));
    let recorded = seen.clone();
    let app = with_server_span(axum::Router::new().route(
        route,
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let recorded = recorded.clone();
            async move {
                *recorded.lock().unwrap() = headers
                    .get("traceparent")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                status
            }
        }),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listener.local_addr().expect("the bound address");
    let served = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("the service serves");
    });
    (addr, served, seen)
}

/// A port with nothing behind it: bound, then closed.
#[cfg(feature = "axum")]
async fn closed_port() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listener.local_addr().expect("the bound address");
    drop(listener);
    addr
}

/// A listener that accepts the call and never answers: only the client's
/// own timeout can end it.
#[cfg(feature = "axum")]
async fn a_peer_that_never_answers() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    addr
}

/// The one span of a kind among the exported spans.
#[cfg(feature = "axum")]
fn span_of_kind(spans: &[SpanData], kind: opentelemetry::trace::SpanKind) -> &SpanData {
    spans
        .iter()
        .find(|span| span.span_kind == kind)
        .unwrap_or_else(|| panic!("the call left one {kind:?} span, not: {spans:?}"))
}

/// The observing stack a test installs, with the log filter it names: a
/// subscriber that exports into a recording exporter, the guard that owns
/// what it published, and the exporter whose spans the test reads back.
fn recording_with(
    filter: EnvFilter,
) -> (
    impl tracing::Subscriber + Send + Sync + 'static,
    Guard,
    RecordingExporter,
) {
    let recording = RecordingExporter::default();
    let handed = recording.clone();
    let config = config("http://collector:4318");
    let (subscriber, guard) = service_layers_with_exporter(
        move |_endpoint: &str| Ok(handed),
        Some(&config),
        &filter,
        "archeion",
        &[],
        None,
        Captured::default(),
    );
    (subscriber, guard, recording)
}

/// The observing stack a test installs: a subscriber that exports into a
/// recording exporter, the guard that owns what it published, and the
/// exporter whose spans the test reads afterwards.
#[cfg(feature = "axum")]
fn recording_telemetry() -> (
    impl tracing::Subscriber + Send + Sync + 'static,
    Guard,
    RecordingExporter,
) {
    let recording = RecordingExporter::default();
    let handed = recording.clone();
    let config = config("http://collector:4318");
    let filter = test_filter();
    let (subscriber, guard) = service_layers_with_exporter(
        move |_endpoint: &str| Ok(handed),
        Some(&config),
        &filter,
        "archeion",
        &[],
        None,
        Captured::default(),
    );
    (subscriber, guard, recording)
}

#[cfg(feature = "axum")]
#[tokio::test]
async fn one_trace_spans_two_services() {
    use opentelemetry::trace::{SpanKind, TraceContextExt as _};
    use tracing::Instrument as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let (addr, served, seen) = service_on_loopback("/files/abc", axum::http::StatusCode::OK).await;
    let (subscriber, mut guard, recording) = recording_telemetry();
    let client = http_client(reqwest::Client::builder()).expect("a client");
    // The root span is built by the layer, so its context exists only
    // while the subscriber is installed: the call joins it from there.
    let root_span_context = {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let root = tracing::info_span!(target: SPAN_TARGET, "root");
        let root_context = root.context();
        let span_context = root_context.span().span_context().clone();
        let response = async { client.get(format!("http://{addr}/files/abc")).send().await }
            .instrument(root.clone())
            .await
            .expect("the service answers");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        span_context
    };
    served.abort();
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let traceparent = seen
        .lock()
        .unwrap()
        .clone()
        .expect("the header travelled with the call");
    let parts: Vec<&str> = traceparent.split('-').collect();
    assert_eq!(
        parts[1],
        root_span_context.trace_id().to_string(),
        "the caller's trace id travels in the header"
    );

    let spans = recording.batches();
    let client_span = span_of_kind(&spans, SpanKind::Client);
    assert_eq!(client_span.name, "GET", "the name stops at the method");
    assert_eq!(
        attribute(client_span, "url.path").as_deref(),
        Some("/files/abc"),
        "the attribute carries the path alone"
    );

    assert_eq!(parts[2], client_span.span_context.span_id().to_string());
    assert_eq!(
        client_span.span_context.trace_id(),
        root_span_context.trace_id()
    );
    assert_eq!(client_span.parent_span_id, root_span_context.span_id());
    // The layer stamps its own bookkeeping on every span it builds; what
    // the call adds is the closed set below, and nothing that carries
    // caller data such as a header, a body, a query or a message.
    const BOOKKEEPING: [&str; 8] = [
        "busy_ns",
        "code.file.path",
        "code.line.number",
        "code.module.name",
        "idle_ns",
        "target",
        "thread.id",
        "thread.name",
    ];
    let mut recorded: Vec<&str> = client_span
        .attributes
        .iter()
        .map(|pair| pair.key.as_str())
        .filter(|key| !BOOKKEEPING.contains(key))
        .collect();
    recorded.sort_unstable();
    assert_eq!(
        recorded,
        [
            "http.request.method",
            "http.response.status_code",
            "server.address",
            "server.port",
            "url.path"
        ],
        "the client span records the call and nothing else"
    );
    assert_eq!(
        attribute(client_span, "server.address").as_deref(),
        Some("127.0.0.1"),
        "the address is the one the URL wrote"
    );
    assert_eq!(
        attribute(client_span, "server.port"),
        Some(addr.port().to_string()),
        "the port comes from the URL"
    );

    let server_span = span_of_kind(&spans, SpanKind::Server);
    assert_eq!(
        server_span.span_context.trace_id(),
        root_span_context.trace_id()
    );
    assert_eq!(
        server_span.parent_span_id,
        client_span.span_context.span_id(),
        "the service answered inside the span of the call"
    );
}

#[cfg(feature = "axum")]
#[tokio::test]
async fn a_call_outside_every_span_opens_one() {
    use opentelemetry::trace::{SpanId, SpanKind};

    // A caller outside every span: the call opens the trace it joins.
    let (addr, served, seen) = service_on_loopback("/ping", axum::http::StatusCode::OK).await;
    let (subscriber, mut guard, recording) = recording_telemetry();
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let client = http_client(reqwest::Client::builder()).expect("a client");
        let response = client
            .get(format!("http://{addr}/ping"))
            .send()
            .await
            .expect("the service answers");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }
    served.abort();
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.batches();
    let client_span = span_of_kind(&spans, SpanKind::Client);
    assert_eq!(
        client_span.parent_span_id,
        SpanId::INVALID,
        "a call made outside every span starts the trace"
    );
    let traceparent = seen
        .lock()
        .unwrap()
        .clone()
        .expect("the header still travels");
    let parts: Vec<&str> = traceparent.split('-').collect();
    assert_eq!(
        parts[1],
        client_span.span_context.trace_id().to_string(),
        "the header carries the trace the call opened"
    );
    let server_span = span_of_kind(&spans, SpanKind::Server);
    assert_eq!(
        server_span.parent_span_id,
        client_span.span_context.span_id()
    );
}

/// A request whose headers carry no `traceparent` starts a trace of its own,
/// even when it is handled inside another span. `extract` starts from
/// `Context::current()` by default and hands the context back unchanged when
/// the carrier holds no valid header, so without an empty start a handler held
/// inside another span would adopt it as parent with nothing on the wire.
/// That is the shape a same-process stub has, and it is why the parent
/// assertions elsewhere need this one beside them.
#[cfg(feature = "axum")]
#[tokio::test]
async fn a_request_without_a_header_does_not_adopt_the_ambient_span() {
    use opentelemetry::trace::{SpanId, SpanKind, TraceContextExt as _};
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let (subscriber, mut guard, recording) = recording_telemetry();
    let ambient_trace = {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let ambient = tracing::info_span!(target: SPAN_TARGET, "ambient");
        let context = ambient.context();
        let trace_id = context.span().span_context().trace_id();
        // The handler runs inside the caller's span, as a same-process stub
        // does, and the request carries no header at all.
        ambient.in_scope(|| {
            let span = request_span("GET", "/files/{id}", "/files/abc", &http::HeaderMap::new());
            span.in_scope(|| {});
        });
        trace_id
    };
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.batches();
    let server_span = span_of_kind(&spans, SpanKind::Server);
    assert_ne!(
        server_span.span_context.trace_id(),
        ambient_trace,
        "a request with no header does not join the handler's trace"
    );
    assert_eq!(
        server_span.parent_span_id,
        SpanId::INVALID,
        "the span is a root, not a child of the span the handler ran in"
    );
}

/// No subscriber at all: nothing to propagate, and the call still works.
#[cfg(feature = "axum")]
#[tokio::test]
async fn without_telemetry_no_header_travels() {
    let (addr, served, seen) = service_on_loopback("/ping", axum::http::StatusCode::OK).await;
    let client = http_client(reqwest::Client::builder()).expect("a client");
    let response = client
        .get(format!("http://{addr}/ping"))
        .send()
        .await
        .expect("the service answers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    served.abort();
    assert_eq!(
        seen.lock().unwrap().clone(),
        None,
        "without telemetry no header reaches the wire"
    );
}

#[cfg(feature = "axum")]
#[tokio::test]
async fn a_failed_call_records_a_type_not_a_text() {
    use opentelemetry::trace::{SpanKind, Status};

    // An answer that failed: the code and the class, side by side.
    let (addr, served, _seen) =
        service_on_loopback("/boom", axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
    let (subscriber, mut guard, recording) = recording_telemetry();
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let client = http_client(reqwest::Client::builder()).expect("a client");
        let response = client
            .get(format!("http://{addr}/boom"))
            .send()
            .await
            .expect("an answer, however unhappy");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    served.abort();
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.batches();
    let client_span = span_of_kind(&spans, SpanKind::Client);
    assert_eq!(
        attribute(client_span, "http.response.status_code").as_deref(),
        Some("500")
    );
    assert_eq!(
        attribute(client_span, "error.type").as_deref(),
        Some("http_500")
    );
    match &client_span.status {
        Status::Error { description } => assert_eq!(
            description.as_ref(),
            "500 Internal Server Error",
            "the status repeats the code's phrase, not the body"
        ),
        other => panic!("a failed answer is an error span, not: {other:?}"),
    }

    // No answer at all: no status to record, one word to name.
    let closed = closed_port().await;
    let (subscriber, mut guard, recording) = recording_telemetry();
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let client = http_client(reqwest::Client::builder()).expect("a client");
        let error = client
            .get(format!("http://{closed}/"))
            .send()
            .await
            .expect_err("nothing answers on a closed port");
        assert!(
            error.is_connect(),
            "a closed port refuses the connection: {error}"
        );
    }
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.batches();
    let client_span = span_of_kind(&spans, SpanKind::Client);
    let recorded = attribute(client_span, "error.type").expect("a failed call names its type");
    assert_eq!(
        recorded, "transport",
        "a refused connection is a transport failure, not a class of its own"
    );
    assert_eq!(
        attribute(client_span, "http.response.status_code"),
        None,
        "no response, no status"
    );
    match &client_span.status {
        Status::Error { description } => {
            assert_eq!(
                description.as_ref(),
                "transport",
                "the status carries the class, not the request"
            );
            assert!(
                !description.contains("http"),
                "no URL or path reaches the status: {description}"
            );
        }
        other => panic!("a call that never completed is an error span, not: {other:?}"),
    }
}

/// A URL with no port in it states the scheme's default, and the span records
/// that number: the reader acts on the port the call used, not on the omission.
#[tokio::test]
async fn a_url_without_a_port_records_the_scheme_default() {
    use opentelemetry::trace::SpanKind;

    for (scheme, port) in [("http", "80"), ("https", "443")] {
        let (subscriber, mut guard, recording) = recording_telemetry();
        {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            // The scheme's default port, and nothing answers there: the call
            // fails after the span's attributes are written, which is all this
            // test reads. The timeout keeps a dropped packet on that port from
            // hanging the test.
            let client =
                http_client(reqwest::Client::builder().timeout(std::time::Duration::from_secs(1)))
                    .expect("a client");
            let _ = client
                .get(format!("{scheme}://localhost/health"))
                .send()
                .await;
        }
        assert!(guard.shutdown(), "a configured guard owns a provider");

        let spans = recording.batches();
        let client_span = span_of_kind(&spans, SpanKind::Client);
        assert_eq!(
            attribute(client_span, "server.port").as_deref(),
            Some(port),
            "{scheme} without a port records its default"
        );
    }
}

/// A call the caller's own timeout ends: the peer takes the connection and
/// never answers, so the wait is what fails.
#[cfg(feature = "axum")]
#[tokio::test]
async fn a_call_that_runs_out_of_time_names_the_timeout() {
    use opentelemetry::trace::{SpanKind, Status};

    let addr = a_peer_that_never_answers().await;
    let (subscriber, mut guard, recording) = recording_telemetry();
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let client =
            http_client(reqwest::Client::builder().timeout(std::time::Duration::from_millis(200)))
                .expect("a client");
        client
            .get(format!("http://{addr}/slow"))
            .send()
            .await
            .expect_err("a peer that never answers trips the timeout");
    }
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.batches();
    let client_span = span_of_kind(&spans, SpanKind::Client);
    assert_eq!(
        attribute(client_span, "error.type").as_deref(),
        Some("timeout"),
        "a wait that ran out is named apart from a transport failure"
    );
    assert_eq!(
        attribute(client_span, "http.response.status_code"),
        None,
        "no response, no status"
    );
    match &client_span.status {
        Status::Error { description } => assert_eq!(description.as_ref(), "timeout"),
        other => panic!("a timed-out call is an error span, not: {other:?}"),
    }
}

/// The rule the two halves break in opposite directions: a client span counts
/// a 4xx as its own failure, a server span leaves it to the status code.
#[cfg(feature = "axum")]
#[tokio::test]
async fn a_client_span_reads_a_4xx_as_failed_and_the_server_span_does_not() {
    use opentelemetry::trace::{SpanKind, Status};

    let (addr, served, _seen) =
        service_on_loopback("/deny", axum::http::StatusCode::NOT_FOUND).await;
    let (subscriber, mut guard, recording) = recording_telemetry();
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let client = http_client(reqwest::Client::builder()).expect("a client");
        let response = client
            .get(format!("http://{addr}/deny"))
            .send()
            .await
            .expect("an answer, however unhappy");
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    }
    served.abort();
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.batches();
    let client_span = span_of_kind(&spans, SpanKind::Client);
    assert_eq!(
        attribute(client_span, "error.type").as_deref(),
        Some("http_404"),
        "a client span counts a 4xx as its own failure"
    );
    match &client_span.status {
        Status::Error { description } => assert_eq!(description.as_ref(), "404 Not Found"),
        other => panic!("a failed call is an error span, not: {other:?}"),
    }

    let server_span = span_of_kind(&spans, SpanKind::Server);
    assert_eq!(
        attribute(server_span, "error.type"),
        None,
        "the same 4xx is no error on the server span"
    );
    assert!(
        matches!(&server_span.status, Status::Unset),
        "and leaves the server status unset: {:?}",
        server_span.status
    );
}

/// The query stays off the span: the attribute carries the path alone, the
/// name only the method, and no value holds what the URL carried.
#[cfg(feature = "axum")]
#[tokio::test]
async fn a_query_string_stays_out_of_the_span() {
    use opentelemetry::trace::SpanKind;

    let (addr, served, _seen) = service_on_loopback("/files/abc", axum::http::StatusCode::OK).await;
    let (subscriber, mut guard, recording) = recording_telemetry();
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let client = http_client(reqwest::Client::builder()).expect("a client");
        let response = client
            .get(format!("http://{addr}/files/abc?token=secret"))
            .send()
            .await
            .expect("the service answers");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }
    served.abort();
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.batches();
    let client_span = span_of_kind(&spans, SpanKind::Client);
    assert_eq!(
        attribute(client_span, "url.path").as_deref(),
        Some("/files/abc"),
        "the attribute carries the path the query hangs off, not the query"
    );
    assert_eq!(
        client_span.name, "GET",
        "the name is the method alone, so no query can reach it"
    );
    for pair in &client_span.attributes {
        assert!(
            !pair.value.as_str().contains("secret"),
            "no attribute value carries the query: {pair:?}"
        );
    }
}
#[tokio::test]
async fn a_span_reaches_the_exporter_while_no_event_does() {
    // The red line is about text, and text travels in events: a message, an
    // upstream error body. A span's own attributes are the vocabulary this
    // workspace chose, so an event must not ride along at any level.
    const SENTINEL: &str = "SENTINEL-b0dy-do-not-export";
    let recording = RecordingExporter::default();
    let handed = recording.clone();
    let config = config("http://collector:4318");
    let (subscriber, mut guard) = service_layers_with_exporter(
        move |_endpoint: &str| Ok(handed),
        Some(&config),
        &EnvFilter::new("warn"),
        "archeion",
        &[],
        None,
        Captured::default(),
    );
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        // RUST_LOG=warn leaves this callsite uninterested for the log
        // layer; the export layer's own filter is what keeps the span.
        let span = tracing::info_span!(target: SPAN_TARGET, "work");
        span.in_scope(|| {
            tracing::info!(body = SENTINEL, "a note");
            tracing::warn!(body = SENTINEL, "a warning carries text too");
            tracing::error!(body = SENTINEL, "and so does this one");
        });
    }
    assert!(guard.shutdown(), "a configured guard owns a provider");
    let spans = recording.batches();
    assert_eq!(spans.len(), 1, "one span, recorded despite the log filter");
    assert!(
        spans[0].events.is_empty(),
        "no event reaches the trace, at any level: {:?}",
        spans[0].events
    );
    assert!(
        !format!("{:?}", spans[0]).contains(SENTINEL),
        "the exported span carries no event text"
    );
}

#[test]
fn the_request_span_carries_the_span_target() {
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::sink)
            .with_ansi(false),
    );
    tracing::subscriber::with_default(subscriber, || {
        let span = request_span("GET", "/files/{id}", "/files/abc", &http::HeaderMap::new());
        assert_eq!(
            span.metadata().map(|meta| meta.target()),
            Some(SPAN_TARGET),
            "the export layer selects on this target"
        );
    });
}

#[cfg(feature = "axum")]
#[tokio::test]
async fn a_client_span_and_its_traceparent_survive_a_quiet_log_filter() {
    use opentelemetry::trace::SpanKind;

    let (addr, served, seen) = service_on_loopback("/files/abc", axum::http::StatusCode::OK).await;
    let recording = RecordingExporter::default();
    let handed = recording.clone();
    let config = config("http://collector:4318");
    let (subscriber, mut guard) = service_layers_with_exporter(
        move |_endpoint: &str| Ok(handed),
        Some(&config),
        &EnvFilter::new("warn"),
        "archeion",
        &[],
        None,
        Captured::default(),
    );
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let client = http_client(reqwest::Client::builder()).expect("a client");
        let response = client
            .get(format!("http://{addr}/files/abc"))
            .send()
            .await
            .expect("the service answers");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }
    served.abort();
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let traceparent = seen
        .lock()
        .unwrap()
        .clone()
        .expect("the header travelled with the call");
    let spans = recording.batches();
    let client_span = span_of_kind(&spans, SpanKind::Client);
    let parts: Vec<&str> = traceparent.split('-').collect();
    assert_eq!(
        parts[2],
        client_span.span_context.span_id().to_string(),
        "the header names the span the call opened"
    );
    let trace_id = client_span.span_context.trace_id().to_string();
    assert_eq!(parts[1], trace_id, "and the trace it belongs to");
    assert_eq!(client_span.name, "GET", "the name stops at the method");
}

#[tokio::test]
async fn a_scope_that_ends_without_a_verdict_reads_as_abandoned() {
    let (subscriber, mut guard, recording) = recording_with(EnvFilter::new("info"));
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let unfinished = tracing::info_span!(target: SPAN_TARGET, "unfinished");
        {
            let _settle = Settle::new(&unfinished);
        }
        let failed = tracing::info_span!(target: SPAN_TARGET, "failed");
        Settle::new(&failed).failed("tool_timeout");
        let cancelled = tracing::info_span!(target: SPAN_TARGET, "cancelled");
        Settle::new(&cancelled).cancelled();
        let finished = tracing::info_span!(target: SPAN_TARGET, "finished");
        Settle::new(&finished).done();
    }
    assert!(guard.shutdown(), "a configured guard owns a provider");

    let spans = recording.observed();
    let find = |name: &str| {
        spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("the {name} span: {spans:#?}"))
    };
    assert_eq!(
        find("unfinished").attribute("kallipai.outcome"),
        Some(ABANDONED),
        "a scope that said nothing did not succeed"
    );
    assert_eq!(find("unfinished").attribute("error.type"), Some(ABANDONED));
    assert!(find("unfinished").status.starts_with("error"));
    assert_eq!(find("failed").attribute("kallipai.outcome"), Some(FAILED));
    assert_eq!(find("failed").attribute("error.type"), Some("tool_timeout"));
    assert!(find("failed").status.starts_with("error"));
    assert_eq!(
        find("cancelled").attribute("kallipai.outcome"),
        Some(CANCELLED)
    );
    assert_eq!(
        find("cancelled").attribute("error.type"),
        None,
        "a cancelled scope is not a failure"
    );
    assert_eq!(
        find("cancelled").status,
        "unset",
        "a shutdown must not read as an error"
    );
    assert_eq!(find("finished").attribute("kallipai.outcome"), Some(OK));
    assert_eq!(
        find("finished").attribute("error.type"),
        None,
        "a finished scope carries no failure"
    );
    assert_eq!(find("finished").status, "unset");
}

/// An exporter that answers the way the test asks and keeps what the wrapper
/// handed it: the counting wrapper's two obligations in one fake.
#[derive(Debug, Clone)]
struct ScriptedExporter {
    answer: Answer,
    resource: Arc<Mutex<Option<Resource>>>,
}

#[derive(Debug, Clone, Copy)]
enum Answer {
    Ok,
    Unreachable,
    Timeout,
    /// The intake rejects the credential with this status. The message also
    /// carries a transport word, the way a rejection body can, so the test
    /// proves the status is read before the unreachable list.
    Rejected(&'static str),
    Other,
    /// A collector that takes the connection and never answers.
    Hung,
}

impl SpanExporter for ScriptedExporter {
    async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
        match self.answer {
            Answer::Ok => Ok(()),
            Answer::Unreachable => Err(OTelSdkError::InternalFailure(
                "connection refused by the collector".to_string(),
            )),
            Answer::Timeout => Err(OTelSdkError::Timeout(std::time::Duration::from_secs(1))),
            Answer::Rejected(status) => Err(OTelSdkError::InternalFailure(format!(
                "HTTP export failed with status code: {status}; the connection was closed"
            ))),
            Answer::Other => Err(OTelSdkError::InternalFailure(
                "the body would not serialize".to_string(),
            )),
            Answer::Hung => std::future::pending().await,
        }
    }

    fn set_resource(&mut self, resource: &Resource) {
        *self.resource.lock().unwrap() = Some(resource.clone());
    }
}

/// A tally of its own, so one test's accounting cannot be read from another's.
fn scripted_stats() -> &'static ExportStats {
    Box::leak(Box::new(ExportStats::default()))
}

#[tokio::test]
async fn the_counting_wrapper_books_the_answer_and_returns_it_unchanged() {
    let stats = scripted_stats();
    let mut counting = CountingExporter {
        inner: ScriptedExporter {
            answer: Answer::Unreachable,
            resource: Arc::new(Mutex::new(None)),
        },
        stats,
    };

    assert!(
        counting.export(Vec::new()).await.is_err(),
        "the wrapper returns the exporter's failure as it found it"
    );
    let snapshot = stats.snapshot();
    assert_eq!(
        snapshot.failed_unavailable, 1,
        "a refused connection is unreachable"
    );
    assert_eq!(snapshot.failed_timeout, 0);
    assert_eq!(snapshot.failed_unauthorized, 0);
    assert_eq!(snapshot.failed_other, 0);

    counting.inner.answer = Answer::Timeout;
    assert!(counting.export(Vec::new()).await.is_err());
    counting.inner.answer = Answer::Other;
    assert!(counting.export(Vec::new()).await.is_err());

    let snapshot = stats.snapshot();
    assert_eq!(snapshot.failed_unavailable, 1);
    assert_eq!(snapshot.failed_timeout, 1, "the SDK's own timeout variant");
    assert_eq!(snapshot.failed_unauthorized, 0);
    assert_eq!(
        snapshot.failed_other, 1,
        "an internal failure that is not transport"
    );
    assert_eq!(snapshot.exported_spans, 0);
    assert!(
        snapshot.last_failure_unix > 0,
        "a failed attempt stamps the moment"
    );
    assert_eq!(
        snapshot.last_success_unix, 0,
        "nothing has landed, so the two stamps cannot agree"
    );
}

/// A rejected credential is its own fact: the intake answered, so calling it
/// unreachable would send an operator to look at the network, and the
/// rejection body often mentions the connection it closed.
#[tokio::test]
async fn a_rejected_credential_is_booked_as_unauthorized() {
    let stats = scripted_stats();
    let mut counting = CountingExporter {
        inner: ScriptedExporter {
            answer: Answer::Rejected("401"),
            resource: Arc::new(Mutex::new(None)),
        },
        stats,
    };

    assert!(
        counting.export(Vec::new()).await.is_err(),
        "the wrapper returns the exporter's failure as it found it"
    );

    let snapshot = stats.snapshot();
    assert_eq!(
        snapshot.failed_unauthorized, 1,
        "the status says the intake refused this token"
    );
    assert_eq!(
        snapshot.failed_unavailable, 0,
        "a rejection body that mentions the connection is still a rejection"
    );
    assert_eq!(snapshot.failed_other, 0);
    assert_eq!(snapshot.exported_spans, 0);
    assert!(snapshot.last_failure_unix > 0);

    counting.inner.answer = Answer::Rejected("403");
    assert!(counting.export(Vec::new()).await.is_err());
    assert_eq!(
        stats.snapshot().failed_unauthorized,
        2,
        "a forbidden credential is the same fact as a rejected one"
    );
}

#[tokio::test]
async fn the_counting_wrapper_forwards_the_resource_and_counts_the_spans() {
    let stats = scripted_stats();
    let inner = ScriptedExporter {
        answer: Answer::Ok,
        resource: Arc::new(Mutex::new(None)),
    };
    let kept = inner.clone();
    let mut counting = CountingExporter { inner, stats };

    let resource = Resource::builder().with_service_name("probe").build();
    counting.set_resource(&resource);
    let forwarded = kept.resource.lock().unwrap().clone();
    assert_eq!(
        forwarded.map(|given| format!("{given:?}")),
        Some(format!("{resource:?}")),
        "set_resource must reach the exporter, or spans leave without the resource"
    );

    let (subscriber, mut guard, recorder) = recording_with(EnvFilter::new("info"));
    {
        let _subscribed = tracing::subscriber::set_default(subscriber);
        let span = tracing::info_span!(target: SPAN_TARGET, "probe");
        let _entered = span.enter();
    }
    guard.shutdown();
    let batch = recorder.batches();
    assert!(
        !batch.is_empty(),
        "the recording stack handed over one span"
    );
    let spans = batch.len() as u64;

    assert!(counting.export(batch).await.is_ok());
    let snapshot = stats.snapshot();
    assert_eq!(snapshot.exported_spans, spans);
    assert!(
        snapshot.last_success_unix > 0,
        "a success stamps the moment"
    );
    assert_eq!(
        snapshot.last_failure_unix, 0,
        "an export that never failed leaves no failure stamp"
    );
}

/// A collector that takes the connection and never answers is the failure
/// an operator most needs counted, and the batch processor would cancel the
/// call before any tally ran: the wrapper's own deadline is what makes it
/// visible.
#[tokio::test]
async fn a_collector_that_never_answers_is_booked_as_a_timeout() {
    let stats = scripted_stats();
    let counting = CountingExporter {
        inner: ScriptedExporter {
            answer: Answer::Hung,
            resource: Arc::new(Mutex::new(None)),
        },
        stats,
    };

    let answer = counting.export(Vec::new()).await;

    assert!(
        matches!(answer, Err(OTelSdkError::Timeout(_))),
        "the wrapper answers its own deadline: {answer:?}"
    );
    let snapshot = stats.snapshot();
    assert_eq!(snapshot.failed_timeout, 1, "a hung call is a timeout");
    assert_eq!(snapshot.failed_unavailable, 0);
    assert_eq!(snapshot.failed_other, 0);
    assert!(
        snapshot.last_failure_unix > 0,
        "the failure is stamped, which is what /health reads"
    );
    assert_eq!(snapshot.last_success_unix, 0);
}

/// The deadline sits between the retry budget it has to leave room for and
/// the processor's budget it has to undercut: below the retries, it spends
/// the call on sleeping; above the processor's, the call is cancelled
/// before the tally runs and the failure is never seen.
#[test]
fn the_export_deadline_leaves_room_for_the_recommended_retries() {
    // The recommended policy retries three times, sleeping 100ms, 200ms and
    // 400ms: 700ms before the last attempt is even made.
    assert!(
        export::EXPORT_DEADLINE >= std::time::Duration::from_millis(700),
        "a deadline under the retry budget spends it on sleeping"
    );
    assert!(
        export::EXPORT_DEADLINE < export::EXPORT_BUDGET,
        "the deadline must undercut the budget, or the tally never runs"
    );
}

/// The hint wraps the tally in the pipeline the layer builds, so a call the
/// wrapper cuts still gets its one line: the log and the count are written
/// by different layers, and the ordering is what lets the log see the cut.
#[tokio::test]
async fn a_cut_export_is_announced_once_and_counted() {
    let counter = WarnCounter::default();
    let subscriber = tracing_subscriber::registry().with(counter.clone());
    let stats = scripted_stats();
    let exporter = HintOnceExporter {
        inner: CountingExporter {
            inner: ScriptedExporter {
                answer: Answer::Hung,
                resource: Arc::new(Mutex::new(None)),
            },
            stats,
        },
        hinted: Arc::new(std::array::from_fn(|_| AtomicBool::new(false))),
    };

    let answer = {
        let _quiet = tracing::subscriber::set_default(subscriber);
        exporter.export(Vec::new()).await
    };

    assert!(matches!(answer, Err(OTelSdkError::Timeout(_))));
    assert_eq!(
        counter.0.load(Ordering::Relaxed),
        1,
        "the cut is announced once, like any other failure"
    );
    assert_eq!(stats.snapshot().failed_timeout, 1);
}
