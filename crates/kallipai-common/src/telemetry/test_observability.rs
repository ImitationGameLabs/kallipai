use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::trace::{SpanKind, Status};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider, SpanData, SpanExporter};
use tracing_subscriber::Layer;
use tracing_subscriber::registry::Registry;

/// One span as the export boundary shows it, in plain values: a test
/// asserts on these instead of on OpenTelemetry types.
#[derive(Clone, Debug)]
pub struct ObservedSpan {
    /// The span name: the method and the route for a server span.
    pub name: String,
    /// `server`, `client`, `producer`, `consumer` or `internal`.
    pub kind: &'static str,
    /// The trace the span belongs to, as the exporter names it.
    pub trace_id: String,
    /// This span's own id.
    pub span_id: String,
    /// The span it hangs under, or `None` when it is a root.
    pub parent_span_id: Option<String>,
    /// `unset`, `ok`, or `error: <description>`.
    pub status: String,
    /// Every attribute the span carries, rendered.
    pub attributes: BTreeMap<String, String>,
}

impl ObservedSpan {
    /// One attribute's rendered value.
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }
}

/// The far side of an export: the spans the SDK handed to an exporter.
#[derive(Clone, Debug, Default)]
pub(crate) struct RecordingExporter {
    spans: Arc<Mutex<Vec<SpanData>>>,
    resource: Arc<Mutex<Option<Resource>>>,
}

impl RecordingExporter {
    /// The spans exported so far, as plain values.
    pub fn observed(&self) -> Vec<ObservedSpan> {
        self.spans
            .lock()
            .unwrap()
            .iter()
            .map(ObservedSpan::from)
            .collect()
    }

    /// The spans as the SDK handed them over.
    #[cfg(test)]
    pub(crate) fn batches(&self) -> Vec<SpanData> {
        self.spans.lock().unwrap().clone()
    }

    /// The resource the exporter was given.
    #[cfg(test)]
    pub(crate) fn resource(&self) -> Option<Resource> {
        self.resource.lock().unwrap().clone()
    }
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

impl From<&SpanData> for ObservedSpan {
    fn from(span: &SpanData) -> Self {
        let mut attributes = BTreeMap::new();
        for pair in &span.attributes {
            attributes.insert(pair.key.as_str().to_string(), pair.value.to_string());
        }
        Self {
            name: span.name.to_string(),
            kind: match span.span_kind {
                SpanKind::Client => "client",
                SpanKind::Server => "server",
                SpanKind::Producer => "producer",
                SpanKind::Consumer => "consumer",
                SpanKind::Internal => "internal",
            },
            trace_id: span.span_context.trace_id().to_string(),
            span_id: span.span_context.span_id().to_string(),
            parent_span_id: (span.parent_span_id != opentelemetry::trace::SpanId::INVALID)
                .then(|| span.parent_span_id.to_string()),
            status: match &span.status {
                Status::Unset => "unset".to_string(),
                Status::Ok => "ok".to_string(),
                Status::Error { description } => format!("error: {description}"),
            },
            attributes,
        }
    }
}

/// A recorder wired to a provider that exports each span as it ends, so a
/// test reads what the process published instead of waiting on a batch.
///
/// Building it publishes the process-wide W3C propagator and the tracer
/// provider -- the two things the service bootstrap publishes -- so a call
/// made under an installed layer injects and adopts context the way it does
/// in a service. One recorder per process: the last one built owns the
/// process-wide handles.
pub struct Recorder {
    exporter: RecordingExporter,
    tracer: SdkTracer,
}

impl Recorder {
    /// A recorder that names its spans with `service`.
    pub fn new(service: &str) -> Self {
        let exporter = RecordingExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .with_resource(
                Resource::builder()
                    .with_service_name(service.to_string())
                    .with_attribute(KeyValue::new(
                        "service.namespace",
                        super::RESOURCE_NAMESPACE,
                    ))
                    .build(),
            )
            .build();
        let tracer = provider.tracer(service.to_string());
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        opentelemetry::global::set_tracer_provider(provider.clone());
        Self { exporter, tracer }
    }

    /// The layer that hands spans to this recorder's exporter, filtered
    /// as the service bootstrap filters it: a span the bootstrap would
    /// drop is not recorded here either.
    pub fn layer(&self) -> Box<dyn Layer<Registry> + Send + Sync> {
        tracing_opentelemetry::layer()
            .with_tracer(self.tracer.clone())
            .with_filter(super::span_filter())
            .boxed()
    }

    /// The spans exported so far, as plain values.
    pub fn observed(&self) -> Vec<ObservedSpan> {
        self.exporter.observed()
    }
}

/// A recorder and the subscriber that feeds it.
///
/// The subscriber comes back uninstalled: the test decides where its scope
/// begins ([`tracing::subscriber::set_default`]). A task spawned inside that
/// scope shares it, because the subscriber is the thread's.
pub fn recorder(service: &str) -> (Recorder, impl tracing::Subscriber + Send + Sync + 'static) {
    use tracing_subscriber::layer::SubscriberExt as _;
    let recorder = Recorder::new(service);
    let subscriber = tracing_subscriber::registry().with(recorder.layer());
    (recorder, subscriber)
}

/// One exported span by the name and kind a test expects.
pub fn span_of<'a>(spans: &'a [ObservedSpan], name: &str, kind: &str) -> &'a ObservedSpan {
    spans
        .iter()
        .find(|span| span.name == name && span.kind == kind)
        .unwrap_or_else(|| panic!("no {kind} span named {name} among {spans:?}"))
}

/// One exported client span by the call it made: a client span is named for the
/// method alone, so the path it went to is the attribute that tells two calls
/// apart.
///
/// This answers with the first match, so two calls of the same method to the
/// same path give the same span twice. A test that needs both, a retry say,
/// walks the list instead.
pub fn client_span_of<'a>(spans: &'a [ObservedSpan], method: &str, path: &str) -> &'a ObservedSpan {
    spans
        .iter()
        .find(|span| {
            span.name == method && span.kind == "client" && span.attribute("url.path") == Some(path)
        })
        .unwrap_or_else(|| panic!("no client span for {method} {path} among {spans:?}"))
}

/// The spans a reader accepts, once they have all ended, or a panic naming
/// what the wait asked for and what it saw.
///
/// The wait is bounded by time rather than by a count of yields: the exporter
/// hands a batch over from its own thread, and on a loaded machine that can
/// take longer than a yield budget would survive, which would report a short
/// set as if a span were missing. `wanted` states the condition in words, so a
/// timeout reads as the thing the test waited for, not as a predicate.
pub async fn spans_when<F>(recorder: &Recorder, wanted: &str, done: F) -> Vec<ObservedSpan>
where
    F: Fn(&[ObservedSpan]) -> bool,
{
    const WAIT: Duration = Duration::from_secs(5);

    let poll = async {
        loop {
            let spans = recorder.observed();
            if done(&spans) {
                return spans;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    tokio::time::timeout(WAIT, poll).await.unwrap_or_else(|_| {
        let spans = recorder.observed();
        panic!(
            "waited {}s for {wanted}, saw {}: {spans:?}",
            WAIT.as_secs(),
            spans.len()
        )
    })
}

/// The spans a test expects, once they have all ended: a span reaches the
/// exporter when its scope ends, and a stub's scope ends on its own task.
pub async fn spans_until(recorder: &Recorder, wanted: usize) -> Vec<ObservedSpan> {
    spans_when(recorder, &format!("{wanted} spans"), |spans| {
        spans.len() >= wanted
    })
    .await
}

#[cfg(test)]
mod recorder_tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    /// The recorder reads back what a span published: the layer converts
    /// the tracing span, and the exporter hands it over as plain values.
    #[test]
    fn a_recorder_reads_back_the_span_a_test_opens() {
        let recorder = Recorder::new("a-test-service");
        let subscriber = tracing_subscriber::registry().with(recorder.layer());
        let spans = {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            let probe =
                tracing::info_span!(target: crate::telemetry::SPAN_TARGET, "probe", answer = 42);
            let entered = probe.enter();
            let inner = tracing::info_span!(target: crate::telemetry::SPAN_TARGET, "inner");
            drop(inner);
            drop(entered);
            drop(probe);
            recorder.observed()
        };
        assert_eq!(spans.len(), 2, "the probe and its inner span: {spans:#?}");
        let probe = spans
            .iter()
            .find(|span| span.name == "probe")
            .expect("the named span");
        assert_eq!(probe.kind, "internal");
        assert_eq!(probe.attribute("answer"), Some("42"));
        assert_eq!(probe.parent_span_id, None);
        let inner = spans
            .iter()
            .find(|span| span.name == "inner")
            .expect("the inner span");
        assert_eq!(
            inner.parent_span_id.as_deref(),
            Some(probe.span_id.as_str())
        );
        assert_eq!(inner.trace_id, probe.trace_id);
    }
}
