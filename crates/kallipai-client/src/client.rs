use std::sync::Arc;

use anyhow::{Context, Result};
use kallipai_common::protocol::ApiError;
use kallipai_common::telemetry::HttpClient;

mod agents;
mod approvals;
mod attachments;
mod budget;
mod history;
mod inbox;
mod lesche;
mod settings;
mod status;
mod task;
mod team;
pub use kallipai_common::protocol::TaskExport;
pub use lesche::{DirectReadQuery, MessageDelivery, RoomReadQuery};

struct Inner {
    base_url: String,
    http: HttpClient,
    auth_token: Option<String>,
}

/// Async client for the kallipai tagma HTTP API.
#[derive(Clone)]
pub struct TagmaClient {
    inner: Arc<Inner>,
}

impl TagmaClient {
    /// Start building a [`TagmaClient`].
    ///
    /// `base_url` is required and is the tagma's HTTP root (e.g.
    /// `http://127.0.0.1:3000`).  Chain `.auth_token()` and/or
    /// `.http_client()` to override defaults, then call `.build()`.
    ///
    /// The default HTTP client is created lazily in [`TagmaClientBuilder::build`]
    /// so that callers who override it via `.http_client()` never pay the cost
    /// of constructing the default `reqwest::Client`.
    pub fn builder(base_url: &str) -> TagmaClientBuilder {
        TagmaClientBuilder {
            base_url: base_url.trim_end_matches('/').to_owned(),
            auth_token: None,
            http: None,
        }
    }

    /// Construct a client from environment variables.
    ///
    /// Reads `KALLIPAI_TAGMA_URL` (default: `http://127.0.0.1:3000`) and
    /// `KALLIPAI_AUTH_TOKEN` (required). Returns an error if the token is
    /// missing, with guidance tailored to common scenarios:
    ///
    /// - **Agent running inside the tagma**: the token is embedded
    ///   automatically in the spawned agent's environment, this should not happen.
    /// - **Operator user**: copy the token from the tagma startup output and
    ///   `export KALLIPAI_AUTH_TOKEN=<token>`.
    /// - **Automation**: set `KALLIPAI_AUTH_TOKEN` to the same value as the
    ///   tagma's `KALLIPAI_OPERATOR_TOKEN`.
    pub fn from_env() -> Result<Self> {
        let (url, token) = read_env_config()?;
        Self::builder(&url).auth_token(token).build()
    }

    /// Like [`from_env()`](Self::from_env), but injects a pre-built HTTP client.
    ///
    /// Use this when the caller needs to control TLS configuration (e.g.
    /// disabling cert verification for loopback-only connections in minimal
    /// containers).
    pub fn from_env_with_http(http: HttpClient) -> Result<Self> {
        let (url, token) = read_env_config()?;
        Self::builder(&url)
            .auth_token(token)
            .http_client(http)
            .build()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.inner.base_url)
    }

    /// The tagma's HTTP root this client talks to, without a trailing
    /// slash: the identity an export manifest records.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// Set Authorization: Bearer <token> if an auth token is configured.
    fn with_auth(
        &self,
        req: reqwest_middleware::RequestBuilder,
    ) -> reqwest_middleware::RequestBuilder {
        if let Some(ref token) = self.inner.auth_token {
            req.bearer_auth(token)
        } else {
            req
        }
    }

    // -- HTTP helpers ---------------------------------------------------------

    /// Send request, parse structured JSON error on non-2xx, deserialize
    /// success body as `T`.
    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
        context_msg: &'static str,
    ) -> Result<T> {
        if !response.status().is_success() {
            return Err(error_from_response(response).await);
        }
        response.json().await.context(context_msg)
    }

    /// Send request, parse structured JSON error on non-2xx, return the raw
    /// response for callers that need the body as-is: SSE streams,
    /// fire-and-forget writes, widened documents, and opaque payloads.
    async fn ensure_success(&self, response: reqwest::Response) -> Result<reqwest::Response> {
        if !response.status().is_success() {
            return Err(error_from_response(response).await);
        }
        Ok(response)
    }
}

// -- Env helpers ---------------------------------------------------------------

/// Read `KALLIPAI_TAGMA_URL` and `KALLIPAI_AUTH_TOKEN` from the
/// environment.  Returns `(url, token)`.
fn read_env_config() -> Result<(String, String)> {
    let url =
        std::env::var("KALLIPAI_TAGMA_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let token = std::env::var("KALLIPAI_AUTH_TOKEN").context(concat!(
        "KALLIPAI_AUTH_TOKEN is not set.\n",
        "\n",
        "How to obtain the token:\n",
        "- Agent (inside tagma): token is embedded automatically, check tagma setup.\n",
        "- Operator user: copy from tagma startup output, then:\n",
        "    export KALLIPAI_AUTH_TOKEN=<token>\n",
        "- Automation: start the tagma with a preset operator token:\n",
        "    KALLIPAI_OPERATOR_TOKEN=<secret> kallipai-tagma\n",
        "  then use the same value for the client:\n",
        "    KALLIPAI_AUTH_TOKEN=<secret> kallip <command>",
    ))?;
    Ok((url, token))
}

/// The process-wide default [`HttpClient`], constructed once and shared by
/// every `TagmaClient` that doesn't override `.http_client()`. Cloning is
/// cheap and shares the underlying pool. The client carries the dispatch
/// middleware that injects the ambient trace context, so a call it makes
/// lands in the trace of the work that asked for it.
fn shared_http() -> &'static HttpClient {
    static HTTP: std::sync::OnceLock<HttpClient> = std::sync::OnceLock::new();
    HTTP.get_or_init(|| {
        kallipai_common::telemetry::http_client(reqwest::ClientBuilder::new())
            .expect("default reqwest client builds")
    })
}

// -- Builder ------------------------------------------------------------------

/// Fluent builder for [`TagmaClient`].
///
/// Created via [`TagmaClient::builder`].  `base_url` is required (passed to
/// `builder()`); `auth_token` and `http_client` are optional with sensible
/// defaults.
pub struct TagmaClientBuilder {
    base_url: String,
    auth_token: Option<String>,
    http: Option<HttpClient>,
}

impl TagmaClientBuilder {
    /// Set the bearer token for authenticating with the tagma.
    pub fn auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Override the default [`HttpClient`].
    ///
    /// Use this when you need custom TLS settings (e.g. disabling cert
    /// verification for loopback-only connections in minimal containers).
    /// The default client carries the dispatch middleware that publishes the
    /// ambient trace context; a client built without it leaves the trace at
    /// this process. Bypassing is a decision, so it is written out here.
    pub fn http_client(mut self, client: HttpClient) -> Self {
        self.http = Some(client);
        self
    }

    /// Consume the builder and produce a [`TagmaClient`].
    ///
    /// If no custom HTTP client was provided via `.http_client()`, the shared
    /// default client is used.  Constructing it can fail if the system CA
    /// store is missing; callers that need to avoid this should
    /// supply their own client via `.http_client()`.
    pub fn build(self) -> Result<TagmaClient> {
        // Callers that don't override `.http_client()` share one process-wide
        // default `HttpClient` (constructed once, then cloned — the client
        // is a cheap handle over a shared connection pool). This avoids churning
        // a fresh pool/TLS-state per built client (e.g. a peer service building
        // one per request). No blanket timeout is set here — it would clip the
        // long-lived SSE event stream; per-request timeouts live on the calls
        // that need them (e.g. `verify_agent`).
        let http = match self.http {
            Some(client) => client,
            None => shared_http().clone(),
        };
        Ok(TagmaClient {
            inner: Arc::new(Inner {
                base_url: self.base_url,
                http,
                auth_token: self.auth_token,
            }),
        })
    }
}

// -- Wire-format helpers for structured error deserialization ------------------

/// JSON envelope matching the tagma's error response: `{"error":{"message":"..."}}`.
#[derive(serde::Deserialize)]
struct Envelope {
    error: Body,
}

#[derive(serde::Deserialize)]
struct Body {
    message: String,
}

/// Turn a non-2xx response into an `ApiError`, preferring the structured
/// `{"error":{"message":"..."}}` body when it parses.
async fn error_from_response(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Envelope>(&body)
        .map(|e| e.error.message)
        .unwrap_or(body);
    ApiError {
        status: status.as_u16(),
        message,
        dangling: None,
        code: None,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> TagmaClient {
        TagmaClient::builder(&server.uri()).build().unwrap()
    }

    fn as_api_error(err: &anyhow::Error) -> &ApiError {
        err.downcast_ref::<ApiError>()
            .expect("downcasts to ApiError")
    }

    async fn send(client: &TagmaClient, path: &str) -> reqwest::Response {
        client
            .inner
            .http
            .get(client.url(path))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn handle_response_extracts_envelope_message_from_error_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/probe"))
            .respond_with(
                ResponseTemplate::new(503).set_body_string(r#"{"error":{"message":"boom"}}"#),
            )
            .mount(&server)
            .await;
        let client = client_for(&server);
        let response = send(&client, "/probe").await;
        let err = client
            .handle_response::<serde_json::Value>(response, "probe context")
            .await
            .expect_err("503");
        let api = as_api_error(&err);
        assert_eq!(api.status, 503);
        assert_eq!(api.message, "boom");
    }

    #[tokio::test]
    async fn handle_response_falls_back_to_raw_body_when_not_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/probe"))
            .respond_with(ResponseTemplate::new(500).set_body_string("gateway exploded"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let response = send(&client, "/probe").await;
        let err = client
            .handle_response::<serde_json::Value>(response, "probe context")
            .await
            .expect_err("500");
        let api = as_api_error(&err);
        assert_eq!(api.status, 500);
        assert_eq!(api.message, "gateway exploded");
    }

    #[tokio::test]
    async fn handle_response_deserializes_success_body() {
        let server = MockServer::start().await;
        let body = serde_json::json!({ "ok": true });
        Mock::given(method("GET"))
            .and(path("/probe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let response = send(&client, "/probe").await;
        let value = client
            .handle_response::<serde_json::Value>(response, "probe context")
            .await
            .expect("success body parses");
        assert_eq!(value["ok"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn ensure_success_extracts_envelope_message_from_error_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/probe"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(r#"{"error":{"message":"bad token"}}"#),
            )
            .mount(&server)
            .await;
        let client = client_for(&server);
        let response = send(&client, "/probe").await;
        let err = client.ensure_success(response).await.expect_err("401");
        let api = as_api_error(&err);
        assert_eq!(api.status, 401);
        assert_eq!(api.message, "bad token");
    }

    #[tokio::test]
    async fn ensure_success_falls_back_to_raw_body_when_not_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/probe"))
            .respond_with(ResponseTemplate::new(500).set_body_string("gateway exploded"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let response = send(&client, "/probe").await;
        let err = client.ensure_success(response).await.expect_err("500");
        let api = as_api_error(&err);
        assert_eq!(api.status, 500);
        assert_eq!(api.message, "gateway exploded");
    }

    #[tokio::test]
    async fn ensure_success_passes_success_response_through() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/probe"))
            .respond_with(ResponseTemplate::new(200).set_body_string("stream body"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let response = send(&client, "/probe").await;
        let response = client.ensure_success(response).await.expect("2xx passes");
        assert_eq!(response.status().as_u16(), 200);
    }

    // -- the trace a call belongs to ----------------------------------------

    /// Serve `app` on an ephemeral port and hand back its base URL.
    async fn spawn_stub(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// The tagma's answer to a timezone read, with the server span layered
    /// on: a recorder sees the request arrive the way the daemon sees it.
    async fn spawn_timezone_stub() -> String {
        let app = kallipai_common::telemetry::with_server_span(axum::Router::new().route(
            "/settings/timezone",
            axum::routing::get(|| async { axum::Json(serde_json::json!({ "timezone": "UTC" })) }),
        ));
        spawn_stub(app).await
    }

    /// The span a caller would be in when it makes the call.
    fn agent_request(method: &str, path: &str) -> tracing::Span {
        kallipai_common::telemetry::request_span(method, path, path, &axum::http::HeaderMap::new())
    }

    /// A read made inside a request keeps that request's trace: the tagma
    /// answers inside the span of the call.
    #[tokio::test]
    async fn a_read_carries_the_context_of_the_request_it_serves() {
        use kallipai_common::telemetry::test_observability::{
            client_span_of, recorder, span_of, spans_until,
        };
        use tracing::Instrument as _;

        let base = spawn_timezone_stub().await;
        let (recorder, subscriber) = recorder("kallipai-client");
        {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            let client = TagmaClient::builder(&base).build().expect("a client");
            let request = agent_request("GET", "/agent/status");
            let zone = async { client.get_timezone().await }
                .instrument(request.clone())
                .await
                .expect("the read lands");
            assert_eq!(zone.as_deref(), Some("UTC"));
        }

        let spans = spans_until(&recorder, 3).await;
        let asked = span_of(&spans, "GET /agent/status", "server");
        let call = client_span_of(&spans, "GET", "/settings/timezone");
        let answered = span_of(&spans, "GET /settings/timezone", "server");
        assert_eq!(
            call.parent_span_id.as_deref(),
            Some(asked.span_id.as_str()),
            "the call hangs under the request that made it"
        );
        assert_eq!(
            answered.parent_span_id.as_deref(),
            Some(call.span_id.as_str()),
            "the tagma answered inside the span of the call"
        );
        assert_eq!(
            answered.trace_id, call.trace_id,
            "one trace spans the read, not a second one at the tagma"
        );
        assert_eq!(
            call.trace_id, asked.trace_id,
            "and that trace is the request's"
        );
    }

    /// The same read on a client with no middleware: nothing puts a header on
    /// the wire, so the tagma starts a trace of its own. Without this, the
    /// test above would pass on a header read back at the caller.
    #[tokio::test]
    async fn a_read_that_carries_no_header_leaves_the_tagma_its_own_trace() {
        use kallipai_common::telemetry::test_observability::{recorder, span_of, spans_until};
        use tracing::Instrument as _;

        let base = spawn_timezone_stub().await;
        let (recorder, subscriber) = recorder("kallipai-client");
        {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            // A bare client, written out: `http_client` is typed so a read
            // through the client cannot drop the propagation unnoticed.
            let bare = reqwest_middleware::ClientWithMiddleware::from(reqwest::Client::new());
            let client = TagmaClient::builder(&base)
                .http_client(bare)
                .build()
                .expect("a client");
            let request = agent_request("GET", "/agent/status");
            async { client.get_timezone().await }
                .instrument(request.clone())
                .await
                .expect("the read lands");
        }

        let spans = spans_until(&recorder, 2).await;
        let asked = span_of(&spans, "GET /agent/status", "server");
        let answered = span_of(&spans, "GET /settings/timezone", "server");
        assert_eq!(
            answered.parent_span_id, None,
            "a request with no header is a root at the tagma"
        );
        assert_ne!(
            answered.trace_id, asked.trace_id,
            "the tagma did not join the caller's trace"
        );
    }
}
