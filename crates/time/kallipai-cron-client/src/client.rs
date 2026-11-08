//! `CronClient` — HTTP client for the kallipai-cron management API.
//!
//! Env-driven like `TagmaClient`: `KALLIPAI_CRON_URL` (default loopback) +
//! `KALLIPAI_AUTH_TOKEN` (the agent bearer, auto-injected into every agent shell).
//! Every management request carries the bearer plus the caller's claimed agent
//! id — `?agent=<id>` on the read/delete ops, and `agent_id` in the create body
//! — which the daemon verifies against the tagma before scoping to that agent's
//! own schedules.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, Error as ReqwestError, Response, StatusCode};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tracing::{debug, instrument};

use kallipai_common::SPAN_TARGET;
use kallipai_common::agentid::AgentId;
use kallipai_common::telemetry::HttpClient;
use kallipai_cron_common::{
    CreateScheduleRequest, Schedule, ScheduleStatus, SchedulesListResponse, StatusResponse,
    UpdateScheduleRequest,
};

const DEFAULT_URL: &str = "http://127.0.0.1:3010";
const ENV_URL: &str = "KALLIPAI_CRON_URL";
const ENV_TOKEN: &str = "KALLIPAI_AUTH_TOKEN";

/// Default per-request timeout. The cron client is pure request/response (never
/// SSE), so a blanket timeout is correct here — unlike `TagmaClient`, which
/// omits one to preserve streaming.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Client error types. Carries the HTTP status so callers can branch.
#[derive(Debug, Error)]
pub enum CronClientError {
    #[error("network error: {0}")]
    Network(#[from] reqwest_middleware::Error),
    #[error("api error (HTTP {status}): {message}")]
    Api { status: u16, message: String },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// A failure below the middleware chain lands in the same bucket: callers
/// branch on the variant, not on which layer refused.
impl From<ReqwestError> for CronClientError {
    fn from(error: ReqwestError) -> Self {
        CronClientError::Network(reqwest_middleware::Error::Reqwest(error))
    }
}

impl CronClientError {
    /// The HTTP status of an `Api` variant, if any.
    pub fn status(&self) -> Option<u16> {
        match self {
            CronClientError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

struct Inner {
    base_url: String,
    http: HttpClient,
    /// Agent bearer (`KALLIPAI_AUTH_TOKEN`); the daemon verifies it against the
    /// claimed agent id via the tagma on every request.
    token: String,
}

/// Clonable handle. `Arc`'d inner, so cloning is cheap and shares the pool.
#[derive(Clone)]
pub struct CronClient(Arc<Inner>);

impl CronClient {
    /// Build a client for `base_url` + `token` using the default reqwest client.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::builder(&base_url.into())
            .token(token.into())
            .build()
            .expect("default client builds")
    }

    /// Start a builder.
    pub fn builder(base_url: &str) -> CronClientBuilder {
        CronClientBuilder {
            base_url: base_url.to_string(),
            token: String::new(),
            http: None,
            timeout: None,
        }
    }

    /// Construct from env: `KALLIPAI_CRON_URL` (default `http://127.0.0.1:3010`)
    /// and `KALLIPAI_AUTH_TOKEN` (required — the agent bearer).
    pub fn from_env() -> Result<Self, CronClientError> {
        let base_url = std::env::var(ENV_URL).unwrap_or_else(|_| DEFAULT_URL.to_string());
        let token = std::env::var(ENV_TOKEN).map_err(|_| CronClientError::Api {
            status: 0,
            message: format!("{ENV_TOKEN} not set (agent shells are injected by the tagma)"),
        })?;
        Self::builder(&base_url).token(token).build()
    }

    /// The base URL this client is configured to use.
    pub fn base_url(&self) -> &str {
        &self.0.base_url
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest_middleware::RequestBuilder {
        let url = format!("{}{}", self.0.base_url, path);
        self.0.http.request(method, url).bearer_auth(&self.0.token)
    }

    /// Deserialize a success body, or map a non-2xx response to `Api`.
    async fn handle<T: DeserializeOwned>(response: Response) -> Result<T, CronClientError> {
        let status = response.status();
        let url = response.url().clone();
        debug!(%url, %status, "cron response");
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            // The daemon returns errors as the tagma envelope
            // `{"error":{"message":"..."}}` (kallipai_common::protocol::ApiError);
            // unwrap to a clean message, falling back to the raw body.
            let message = serde_json::from_str::<Envelope>(&body)
                .map(|e| e.error.message)
                .unwrap_or(body);
            return Err(CronClientError::Api {
                status: status.as_u16(),
                message,
            });
        }
        let text = response.text().await?;
        Ok(serde_json::from_str(&text)?)
    }

    // -- endpoints --

    #[instrument(target = SPAN_TARGET, skip(self))]
    pub async fn health(&self) -> Result<String, CronClientError> {
        // /health returns a plain-text body (not JSON), so bypass `handle`.
        let resp = self.request(reqwest::Method::GET, "/health").send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Envelope>(&body)
                .map(|e| e.error.message)
                .unwrap_or(body);
            return Err(CronClientError::Api {
                status: status.as_u16(),
                message,
            });
        }
        Ok(resp.text().await?)
    }

    /// Daemon status scoped to `agent` (that agent's active/pending counts + next fire).
    #[instrument(target = SPAN_TARGET, skip(self))]
    pub async fn status(&self, agent: &AgentId) -> Result<StatusResponse, CronClientError> {
        let resp = self
            .request(reqwest::Method::GET, "/status")
            .query(&[("agent", agent.as_ref())])
            .send()
            .await?;
        Self::handle(resp).await
    }

    #[instrument(target = SPAN_TARGET, skip(self, req))]
    pub async fn create(&self, req: CreateScheduleRequest) -> Result<Schedule, CronClientError> {
        // `agent_id` travels in the body (the verified caller).
        let resp = self
            .request(reqwest::Method::POST, "/schedules")
            .json(&req)
            .send()
            .await?;
        Self::handle(resp).await
    }

    /// List `agent`'s schedules, optionally filtered by status/tag.
    #[instrument(target = SPAN_TARGET, skip(self))]
    pub async fn list(
        &self,
        agent: &AgentId,
        status: Option<ScheduleStatus>,
        tag: Option<&str>,
    ) -> Result<Vec<Schedule>, CronClientError> {
        let mut req = self
            .request(reqwest::Method::GET, "/schedules")
            .query(&[("agent", agent.as_ref())]);
        if let Some(s) = status {
            req = req.query(&[("status", s.to_string())]);
        }
        if let Some(t) = tag {
            req = req.query(&[("tag", t.to_string())]);
        }
        let resp = req.send().await?;
        let page: SchedulesListResponse = Self::handle(resp).await?;
        Ok(page.schedules)
    }

    /// Get one of `agent`'s schedules by id; `None` if not found or owned by
    /// another agent (the daemon returns `404` for both, uniformly).
    #[instrument(target = SPAN_TARGET, skip(self))]
    pub async fn get(
        &self,
        agent: &AgentId,
        id: &str,
    ) -> Result<Option<Schedule>, CronClientError> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/schedules/{id}"))
            .query(&[("agent", agent.as_ref())])
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let sched: Schedule = Self::handle(resp).await?;
        Ok(Some(sched))
    }

    /// `agent`'s earliest-fire schedule, or `None`.
    #[instrument(target = SPAN_TARGET, skip(self))]
    pub async fn next(&self, agent: &AgentId) -> Result<Option<Schedule>, CronClientError> {
        let resp = self
            .request(reqwest::Method::GET, "/schedules/next")
            .query(&[("agent", agent.as_ref())])
            .send()
            .await?;
        let opt: Option<Schedule> = Self::handle(resp).await?;
        Ok(opt)
    }

    /// Delete one of `agent`'s schedules; `false` if not found / cross-owner.
    #[instrument(target = SPAN_TARGET, skip(self))]
    pub async fn delete(&self, agent: &AgentId, id: &str) -> Result<bool, CronClientError> {
        let resp = self
            .request(reqwest::Method::DELETE, &format!("/schedules/{id}"))
            .query(&[("agent", agent.as_ref())])
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let _: serde_json::Value = Self::handle(resp).await?;
        Ok(true)
    }

    /// Update one of `agent`'s schedules (status-only: pause/resume).
    #[instrument(target = SPAN_TARGET, skip(self, req))]
    pub async fn update(
        &self,
        agent: &AgentId,
        id: &str,
        req: UpdateScheduleRequest,
    ) -> Result<Schedule, CronClientError> {
        let resp = self
            .request(reqwest::Method::PATCH, &format!("/schedules/{id}"))
            .query(&[("agent", agent.as_ref())])
            .json(&req)
            .send()
            .await?;
        Self::handle(resp).await
    }
}

/// Builder for [`CronClient`].
///
/// By default the client builds its own reqwest `Client` with a 30s per-request
/// timeout. This deliberately diverges from `TagmaClient::shared_http()` (one
/// process-wide `OnceLock<Client>`): the cron client is request/response only,
/// never SSE, so a per-instance client is fine and a blanket timeout is correct.
/// Do not "consolidate" this onto `shared_http()`.
pub struct CronClientBuilder {
    base_url: String,
    token: String,
    http: Option<HttpClient>,
    timeout: Option<Duration>,
}

impl CronClientBuilder {
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = token.into();
        self
    }

    /// Inject a pre-built reqwest client. When set, [`Self::timeout`] and the
    /// default timeout are ignored — the caller owns the client's configuration
    /// (caller-knows-best; mirrors `TagmaClientBuilder`).
    pub fn http_client(mut self, http: HttpClient) -> Self {
        self.http = Some(http);
        self
    }

    /// Override the per-request timeout. No effect if [`Self::http_client`] is
    /// also set.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn build(self) -> Result<CronClient, CronClientError> {
        let http = match self.http {
            Some(c) => c,
            None => kallipai_common::telemetry::http_client(
                Client::builder().timeout(self.timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT)),
            )?,
        };
        Ok(CronClient(Arc::new(Inner {
            base_url: self.base_url,
            http,
            token: self.token,
        })))
    }
}

// -- Wire-format helpers for structured error deserialization ----------------

/// JSON envelope matching the daemon's error response: `{"error":{"message":...}}`.
#[derive(serde::Deserialize)]
struct Envelope {
    error: Body,
}

#[derive(serde::Deserialize)]
struct Body {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_round_trip() {
        let c = CronClient::builder("http://example:9999")
            .token("sek")
            .build()
            .unwrap();
        assert_eq!(c.base_url(), "http://example:9999");
    }

    #[test]
    fn envelope_decodes_message() {
        let env: Envelope =
            serde_json::from_str(r#"{"error":{"message":"invalid cron token"}}"#).unwrap();
        assert_eq!(env.error.message, "invalid cron token");
    }

    // -- the trace a call belongs to -----------------------------------------

    /// A daemon that answers the list call, with the server span layered on so
    /// a recorder sees a request arrive.
    async fn spawn_daemon_stub() -> String {
        let app = kallipai_common::telemetry::with_server_span(axum::Router::new().route(
            "/schedules",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({ "schedules": [], "total": 0 }))
            }),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// The span a caller would be in when it asks the daemon.
    fn caller_span() -> tracing::Span {
        kallipai_common::telemetry::request_span(
            "POST",
            "/agent/turn",
            "/agent/turn",
            &axum::http::HeaderMap::new(),
        )
    }

    /// A list made inside a caller's span keeps that span's trace: the daemon
    /// answers inside the span of the call, which sits inside the method's own
    /// span, which sits inside the caller's.
    #[tokio::test]
    async fn a_list_carries_the_context_of_the_request_it_serves() {
        use kallipai_common::telemetry::test_observability::{
            client_span_of, recorder, span_of, spans_until,
        };
        use tracing::Instrument as _;

        let base = spawn_daemon_stub().await;
        let agent = AgentId::from("dev".to_owned());
        let (recorder, subscriber) = recorder("kallipai-cron-client");
        {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            let client = CronClient::builder(&base).token("sek").build().unwrap();
            async { client.list(&agent, None, None).await.expect("list lands") }
                .instrument(caller_span())
                .await;
        }

        let spans = spans_until(&recorder, 4).await;
        let asked = span_of(&spans, "POST /agent/turn", "server");
        let method = span_of(&spans, "list", "internal");
        let call = client_span_of(&spans, "GET", "/schedules");
        let answered = span_of(&spans, "GET /schedules", "server");
        assert_eq!(
            method.parent_span_id.as_deref(),
            Some(asked.span_id.as_str()),
            "the method's span sits under the caller's"
        );
        assert_eq!(
            call.parent_span_id.as_deref(),
            Some(method.span_id.as_str()),
            "the call hangs under the method that made it"
        );
        assert_eq!(
            answered.parent_span_id.as_deref(),
            Some(call.span_id.as_str()),
            "the daemon answered inside the span of the call"
        );
        assert_eq!(
            answered.trace_id, call.trace_id,
            "one trace spans the list, not a second one at the daemon"
        );
        assert_eq!(
            call.trace_id, asked.trace_id,
            "and that trace is the caller's"
        );
    }

    /// The same call on a client with no middleware: nothing puts a header on
    /// the wire, so the daemon starts a trace of its own. Without this, the
    /// test above would pass on a header read back at the caller.
    #[tokio::test]
    async fn a_list_that_carries_no_header_leaves_the_daemon_its_own_trace() {
        use kallipai_common::telemetry::test_observability::{recorder, span_of, spans_until};
        use tracing::Instrument as _;

        let base = spawn_daemon_stub().await;
        let agent = AgentId::from("dev".to_owned());
        let (recorder, subscriber) = recorder("kallipai-cron-client");
        {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            // The bypass, written out: a client with no middleware.
            let bare = reqwest_middleware::ClientWithMiddleware::from(reqwest::Client::new());
            let client = CronClient::builder(&base)
                .token("sek")
                .http_client(bare)
                .build()
                .unwrap();
            async { client.list(&agent, None, None).await.expect("list lands") }
                .instrument(caller_span())
                .await;
        }

        let spans = spans_until(&recorder, 3).await;
        let asked = span_of(&spans, "POST /agent/turn", "server");
        let answered = span_of(&spans, "GET /schedules", "server");
        assert_eq!(
            answered.parent_span_id, None,
            "a request with no header is a root at the daemon"
        );
        assert_ne!(
            answered.trace_id, asked.trace_id,
            "the daemon did not join the caller's trace"
        );
    }
}
