//! A backend that carries this process's trace context to a model endpoint.
//!
//! A model request leaves through the client library's own transport, with no
//! `telemetry::http_client` client between it and the socket, so the request
//! opens no client span and carries no `traceparent`. The gateway that answers
//! it then starts a trace of its own, and one turn's waterfall cannot say how
//! much of a chat span was this process and how much was the far side.
//!
//! [`TracedBackend`] is the seam the base library leaves open: [`LlmBackend`]
//! publishes `prepare`/`send`/`parse`, so a wrapper can own the send step and
//! nothing else. This one injects the current span's context onto the prepared
//! request and hands it on unchanged; the rest is delegated, so the streaming
//! parser, the retry classifier and the stateful conversation chain behave
//! exactly as they do unwrapped.
//!
//! It opens no client span: the wrapper holds a prepared request, not a client,
//! so the far side's span hangs under the chat span itself instead of under a
//! second hop this process would have to draw.
//!
//! It covers HTTP backends only. `generate` and `stream_generate` are left to
//! the trait's defaults, which compose this wrapper's own `prepare`/`send`/
//! `parse`; writing that composition out again would copy the library's own
//! order into this crate. The trade is that a provider overriding either method
//! is shadowed by the default rather than consulted -- no built-in provider
//! does, and the transparency test below pins that a wrapped backend sends the
//! same bytes, answers the same and streams the same events as a bare one.
//!
//! [`LlmBackend`]: just_llm_client::LlmBackend

use std::sync::Arc;

use async_trait::async_trait;
use just_llm_client::types::generation::{
    GenerationRequest, GenerationResponse, Message, ToolDefinition,
};
use just_llm_client::{
    BackendConstructError, BackendError, Balance, CapabilityError, CapabilityNegotiation,
    GenerationStream, Identifiable, LlmBackend, ModelCatalog,
};

/// The family this wrapper reports. It is never registered in a factory, so the
/// name exists to keep the two places that report or refuse it in agreement.
const FAMILY: &str = "traced";

/// A backend that puts this process's trace context on every request it sends.
pub struct TracedBackend {
    inner: Arc<dyn LlmBackend>,
}

impl TracedBackend {
    /// Wrap `inner` so its sends carry the context of the span they run under.
    pub fn wrap(inner: Arc<dyn LlmBackend>) -> Arc<dyn LlmBackend> {
        Arc::new(Self { inner })
    }
}

impl Identifiable for TracedBackend {
    fn family(&self) -> &'static str {
        self.inner.family()
    }
}

impl CapabilityNegotiation for TracedBackend {
    /// Delegated rather than defaulted, like the rest of this impl block: the
    /// trait hands every capability out as unsupported by default, so a wrapper
    /// that skipped one would report a backend that has it as one that has not.
    fn model_catalog(&self) -> Result<&dyn ModelCatalog, CapabilityError> {
        self.inner.model_catalog()
    }

    fn balance(&self) -> Result<&dyn Balance, CapabilityError> {
        self.inner.balance()
    }

    /// Delegated rather than defaulted: the trait's default answers `false`,
    /// which would silently turn the conversation chain stateless.
    fn supports_stateful_conversation(&self) -> bool {
        self.inner.supports_stateful_conversation()
    }
}

#[async_trait]
impl LlmBackend for TracedBackend {
    fn prepare(&self, request: GenerationRequest) -> Result<reqwest::Request, BackendError> {
        self.inner.prepare(request)
    }

    fn prepare_streaming(
        &self,
        request: GenerationRequest,
    ) -> Result<reqwest::Request, BackendError> {
        self.inner.prepare_streaming(request)
    }

    async fn send(
        &self,
        mut prepared: reqwest::Request,
    ) -> Result<reqwest::Response, BackendError> {
        kallipai_common::telemetry::inject_current_context(prepared.headers_mut());
        self.inner.send(prepared).await
    }

    async fn parse(&self, response: reqwest::Response) -> Result<GenerationResponse, BackendError> {
        self.inner.parse(response).await
    }

    async fn parse_streaming(
        &self,
        response: reqwest::Response,
    ) -> Result<GenerationStream, BackendError> {
        self.inner.parse_streaming(response).await
    }

    fn render_messages(&self, messages: &[Message]) -> Result<String, BackendError> {
        self.inner.render_messages(messages)
    }

    fn render_tools(&self, tools: &[ToolDefinition]) -> Result<String, BackendError> {
        self.inner.render_tools(tools)
    }

    /// The family of the type, not of the backend it wraps: this wrapper is
    /// never keyed in a factory (see [`new`](LlmBackend::new)).
    fn family() -> &'static str {
        FAMILY
    }

    /// A wrapper is built around an existing backend by [`TracedBackend::wrap`]
    /// and never through a factory, so this constructor has no work to do.
    fn new(
        _http: reqwest::ClientBuilder,
        _api_key: &str,
        _base_url: Option<&str>,
    ) -> Result<Arc<dyn LlmBackend>, BackendConstructError> {
        Err(BackendConstructError::unknown_family(FAMILY))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;
    use just_llm_client::provider::OpenAiCompatBackend;
    use just_llm_client::types::generation::{GenerationEvent, GenerationResponse};
    use just_llm_client::{
        BackendError, GenerationClient, GenerationClientOptions, GenerationStream, LlmBackend,
    };
    use kallipai_common::telemetry::test_observability::{ObservedSpan, Recorder};
    use tracing::Instrument as _;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::TracedBackend;
    use crate::spans::chat_span;
    use crate::test_support::{RecordingBackend, ds_backend, install_recorder, request, user_msg};

    const SSE_BODY: &str = "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4.1-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    const JSON_BODY: &str = "{\"id\":\"c1\",\"object\":\"chat.completion\",\"created\":1,\"model\":\"gpt-4.1-mini\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}";

    /// A client whose backend is wrapped, pointed at the mock server.
    fn client(server: &MockServer, model: &str) -> GenerationClient {
        let inner = OpenAiCompatBackend::new(
            reqwest::Client::builder().use_rustls_tls(),
            "test-key",
            Some(&server.uri()),
        )
        .expect("the openai-compat backend constructs without network");
        GenerationClient::new(
            TracedBackend::wrap(inner),
            GenerationClientOptions::new(model),
        )
    }

    /// The chat span as the exporter published it.
    fn chat_of(recorder: &Recorder, model: &str) -> ObservedSpan {
        let name = format!("chat {model}");
        recorder
            .observed()
            .into_iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("the {name} span: {:?}", recorder.observed()))
    }

    /// The `traceparent` the mock server received, split into W3C fields; the
    /// label names the failing call when there is none.
    async fn traceparent_at(server: &MockServer, label: &str) -> Vec<String> {
        let requests = server
            .received_requests()
            .await
            .expect("the mock server records what it answered");
        let first = requests
            .first()
            .unwrap_or_else(|| panic!("the {label} request reached the mock server"));
        let header = first
            .headers
            .get("traceparent")
            .unwrap_or_else(|| panic!("the {label} request carries a traceparent"));
        header
            .to_str()
            .expect("a traceparent is ascii")
            .split('-')
            .map(str::to_string)
            .collect()
    }

    fn answering(content_type: &str, body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body, content_type)
    }

    #[tokio::test]
    async fn a_streaming_send_carries_the_chat_spans_context() {
        let recorder = install_recorder();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(answering("text/event-stream", SSE_BODY))
            .mount(&server)
            .await;

        let client = client(&server, "gpt-stream");
        let chat = chat_span("gpt-stream", "p1", "prof-1", "agent-1");
        let mut stream = client
            .stream_generate(request("gpt-stream", vec![user_msg("hello")]))
            .instrument(chat.clone())
            .await
            .expect("the mock server answers the stream");
        let mut events = 0;
        while let Some(event) = stream.next().await {
            event.expect("every event parses");
            events += 1;
        }
        assert!(events > 0, "the stream was consumed, not only opened");
        drop(stream);
        drop(chat);

        let parts = traceparent_at(&server, "gpt-stream").await;
        let chat = chat_of(recorder, "gpt-stream");
        assert_eq!(
            parts[1], chat.trace_id,
            "the request joins the trace the chat span belongs to"
        );
        assert_eq!(
            parts[2], chat.span_id,
            "and names the chat span as its parent"
        );
        assert_eq!(parts[3], "01", "and asks the gateway to record the request");
    }

    #[tokio::test]
    async fn a_non_streaming_send_carries_the_chat_spans_context() {
        let recorder = install_recorder();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(answering("application/json", JSON_BODY))
            .mount(&server)
            .await;

        let client = client(&server, "gpt-completion");
        let chat = chat_span("gpt-completion", "p1", "prof-1", "agent-1");
        let response = client
            .generate(request("gpt-completion", vec![user_msg("hello")]))
            .instrument(chat.clone())
            .await
            .expect("the mock server answers the completion");
        assert_eq!(
            response.id, "c1",
            "the parse step still runs on the inner backend"
        );
        drop(chat);

        let parts = traceparent_at(&server, "gpt-completion").await;
        let chat = chat_of(recorder, "gpt-completion");
        assert_eq!(
            parts[1], chat.trace_id,
            "the completion joins the chat span's trace"
        );
        assert_eq!(parts[2], chat.span_id, "and hangs under the chat span");
        assert_eq!(parts[3], "01", "and asks the gateway to record the request");
    }

    #[test]
    fn the_wrapper_delegates_what_it_does_not_own() {
        let inner = OpenAiCompatBackend::new(
            reqwest::Client::builder().use_rustls_tls(),
            "test-key",
            Some("http://127.0.0.1:1"),
        )
        .expect("constructs without network");
        let family = inner.family();
        let tools = inner
            .render_tools(&[])
            .expect("the inner backend renders tools");
        let messages = inner
            .render_messages(&[user_msg("hello")])
            .expect("the inner backend renders messages");
        let catalog = inner.model_catalog().is_ok();
        let wrapped = TracedBackend::wrap(inner);
        assert_eq!(
            wrapped.family(),
            family,
            "the family is the inner backend's"
        );
        assert_eq!(
            wrapped.render_tools(&[]).expect("the wrapper delegates"),
            tools,
            "tool rendering stays the inner backend's"
        );

        assert_eq!(
            wrapped.model_catalog().is_ok(),
            catalog,
            "a model catalog the backend has stays reachable"
        );
        assert_eq!(
            wrapped
                .render_messages(&[user_msg("hello")])
                .expect("the wrapper delegates"),
            messages,
            "message rendering, which feeds token estimation, is the inner backend's"
        );

        assert!(
            TracedBackend::wrap(ds_backend()).balance().is_ok(),
            "a backend that has a balance keeps it behind the wrapper"
        );

        let stateful = TracedBackend::wrap(RecordingBackend::new());
        assert!(
            stateful.supports_stateful_conversation(),
            "a stateful backend stays stateful behind the wrapper"
        );

        assert!(
            TracedBackend::new(reqwest::Client::builder(), "test-key", None).is_err(),
            "a wrapper is not built through a factory"
        );
    }

    /// Every family the factory ships, over the wrapper, on the streaming path.
    ///
    /// `stream_generate` is the trait's default, so this walks the composition
    /// the crate relies on: a family whose request took another road would send
    /// without the context, and the trace would break while every other test
    /// stayed green. The list comes from the factory, so a family added later
    /// is covered without touching this test's body.
    #[tokio::test]
    async fn every_registered_family_sends_the_context_of_the_span_it_runs_under() {
        let recorder = install_recorder();
        let factory = just_llm_client::client::BackendFactory::new();
        let mut families: Vec<String> = factory.families().map(str::to_owned).collect();
        families.sort();
        assert!(!families.is_empty(), "the factory ships built-in families");

        for family in families {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(answering("text/event-stream", SSE_BODY))
                .mount(&server)
                .await;
            let inner = factory
                .create(
                    &family,
                    reqwest::Client::builder().use_rustls_tls(),
                    "test-key",
                    Some(&server.uri()),
                )
                .unwrap_or_else(|e| panic!("the {family} backend constructs: {e}"));
            let client = GenerationClient::new(
                TracedBackend::wrap(inner),
                GenerationClientOptions::new("probe-model"),
            );
            let probe_name = format!("probe {family}");
            let probe = tracing::info_span!(
                target: kallipai_common::telemetry::SPAN_TARGET,
                "probe",
                otel.name = %probe_name
            );
            // Anthropic refuses a request without a token ceiling before it
            // reaches the wire; every other family ignores the field.
            let mut asked = request("probe-model", vec![user_msg("hello")]);
            asked.max_tokens = Some(16);
            // A body shaped for another family's parser is a parse error, not a
            // failed send: what left this process is what is under test. The
            // outcome rides along in the label so a missing request says why.
            let wire_note = match client
                .stream_generate(asked)
                .instrument(probe.clone())
                .await
            {
                Ok(_) => "the stream opened".to_string(),
                Err(error) => error.to_string(),
            };
            let parts = traceparent_at(&server, &format!("{family} ({wire_note})")).await;
            drop(probe);

            let probe = recorder
                .observed()
                .into_iter()
                .find(|span| span.name == probe_name)
                .unwrap_or_else(|| panic!("the {family} probe span: {:?}", recorder.observed()));
            assert_eq!(
                parts[1], probe.trace_id,
                "the {family} request joins the probe's trace"
            );
            assert_eq!(
                parts[2], probe.span_id,
                "and names the probe span as its parent"
            );
            assert_eq!(
                parts[3], "01",
                "and asks the next hop to record the {family} request"
            );
        }
    }

    /// What a call answered, without the transport metadata that differs by
    /// timing alone.
    fn outcome_summary(outcome: &Result<GenerationResponse, BackendError>) -> String {
        match outcome {
            Ok(response) => format!(
                "ok {} {} {:?}",
                response.id,
                response.model,
                response.text()
            ),
            Err(error) => format!("error {error}"),
        }
    }

    /// What a stream delivered, drained to its end: the events in the order they
    /// arrived, ending with whatever the stream closed on.
    async fn stream_summary(outcome: Result<GenerationStream, BackendError>) -> String {
        let mut stream = match outcome {
            Ok(stream) => stream,
            Err(error) => return format!("error {error}"),
        };
        let mut events: Vec<GenerationEvent> = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => events.push(event),
                Err(error) => return format!("after {} events, error {error}", events.len()),
            }
        }
        format!("{events:?}")
    }

    /// One bare/wrapped pair on one family and one entry point: the same
    /// request to the same server, and the two calls must differ by nothing but
    /// the propagation headers and their outcome.
    async fn assert_transparent(
        family: &str,
        factory: &just_llm_client::client::BackendFactory,
        streaming: bool,
    ) {
        let server = MockServer::start().await;
        let (content_type, body) = if streaming {
            ("text/event-stream", SSE_BODY)
        } else {
            ("application/json", JSON_BODY)
        };
        Mock::given(method("POST"))
            .respond_with(answering(content_type, body))
            .mount(&server)
            .await;
        let build = || {
            factory
                .create(
                    family,
                    reqwest::Client::builder().use_rustls_tls(),
                    "test-key",
                    Some(&server.uri()),
                )
                .unwrap_or_else(|e| panic!("the {family} backend constructs: {e}"))
        };
        let bare = build();
        let wrapped = TracedBackend::wrap(build());
        // Both calls ask for the same thing under the same model name, so any
        // difference on the wire comes from the wrapper. Anthropic refuses a
        // request without a token ceiling before it reaches the wire.
        let ask = || {
            let mut asked = request("transparency-model", vec![user_msg("hello")]);
            asked.max_tokens = Some(16);
            asked
        };

        let bare_outcome = if streaming {
            stream_summary(bare.stream_generate(ask()).await).await
        } else {
            outcome_summary(&bare.generate(ask()).await)
        };
        let probe = tracing::info_span!(
            target: kallipai_common::telemetry::SPAN_TARGET,
            "probe",
            otel.name = "wrapped call"
        );
        let wrapped_outcome = if streaming {
            stream_summary(
                wrapped
                    .stream_generate(ask())
                    .instrument(probe.clone())
                    .await,
            )
            .await
        } else {
            outcome_summary(&wrapped.generate(ask()).instrument(probe.clone()).await)
        };
        drop(probe);

        let requests = server
            .received_requests()
            .await
            .expect("the mock server records what it answered");
        // One call each side, no more: a wrapper that fanned a call out, or a
        // shadowed override that retried, would otherwise hide behind the pair
        // picked out below.
        assert_eq!(
            requests.len(),
            2,
            "the {family} pair (streaming: {streaming}) sent {} requests instead of one per side; \
             headers: {:?}",
            requests.len(),
            requests
                .iter()
                .map(|request| &request.headers)
                .collect::<Vec<_>>()
        );
        let wrapped_request = requests
            .iter()
            .find(|request| request.headers.contains_key("traceparent"));
        let bare_request = requests
            .iter()
            .find(|request| !request.headers.contains_key("traceparent"));
        let (wrapped_request, bare_request) = wrapped_request.zip(bare_request).unwrap_or_else(|| {
            panic!("the {family} pair sent one request with a traceparent and one without: {requests:?}")
        });

        assert_eq!(
            bare_request.method, wrapped_request.method,
            "the {family} method is unchanged by the wrapper"
        );
        assert_eq!(bare_request.url, wrapped_request.url, "and so is the url");
        assert_eq!(
            bare_request.body, wrapped_request.body,
            "and the body, byte for byte"
        );
        // The propagation headers and nothing else. The W3C propagator sets
        // `tracestate` next to `traceparent` unconditionally, empty here because
        // this process carried no upstream state.
        let mut added: Vec<&str> = wrapped_request
            .headers
            .keys()
            .filter(|name| !bare_request.headers.contains_key(*name))
            .map(|name| name.as_str())
            .collect();
        added.sort_unstable();
        assert!(
            added.contains(&"traceparent"),
            "the wrapped {family} call carries a traceparent: {added:?}"
        );
        assert!(
            added
                .iter()
                .all(|name| matches!(*name, "traceparent" | "tracestate")),
            "the wrapper adds nothing but propagation headers on {family}: {added:?}"
        );
        let mut stripped = wrapped_request.headers.clone();
        stripped.remove("traceparent");
        stripped.remove("tracestate");
        assert_eq!(
            stripped, bare_request.headers,
            "every other {family} header is the bare call's, value for value"
        );
        assert_eq!(
            bare_outcome, wrapped_outcome,
            "the {family} outcome is unchanged by the wrapper (streaming: {streaming})"
        );
    }

    /// The wrapper is transparent on every family the factory ships, on both
    /// entry points: a bare backend and a wrapped one, built from the same
    /// configuration, send the same request to the same server, and the wire
    /// differs by the propagation headers alone while the outcome agrees.
    ///
    /// A shadowed override is what this catches. A family whose own `generate`
    /// or `stream_generate` took another road would answer the bare call one
    /// way and the wrapped call another, and the split shows up as a different
    /// body, a different header set, a different outcome or a different event
    /// sequence.
    #[tokio::test]
    async fn wrapping_adds_one_header_and_changes_nothing_else() {
        // The propagator is published with the recorder; without both, the
        // wrapped call would carry nothing and the pair would look identical.
        install_recorder();
        let factory = just_llm_client::client::BackendFactory::new();
        let mut families: Vec<String> = factory.families().map(str::to_owned).collect();
        families.sort();
        assert!(!families.is_empty(), "the factory ships built-in families");

        for family in &families {
            assert_transparent(family, &factory, false).await;
            assert_transparent(family, &factory, true).await;
        }
    }
}
