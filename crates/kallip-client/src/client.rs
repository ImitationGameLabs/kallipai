use std::sync::Arc;

use anyhow::{Context, Result};
use kallip_common::protocol::ApiError;

mod agents;
mod approvals;
mod budget;
mod dirlock;
mod inbox;
mod lesche;
mod status;

pub use dirlock::{DirLockAcquireResponse, DirLockWhoResponse};

struct Inner {
    base_url: String,
    http: reqwest::Client,
    auth_token: Option<String>,
}

/// Async client for the kallip tagma HTTP API.
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
    /// Reads `KALLIP_TAGMA_URL` (default: `http://127.0.0.1:3000`) and
    /// `KALLIP_AUTH_TOKEN` (required). Returns an error if the token is
    /// missing, with guidance tailored to common scenarios:
    ///
    /// - **Agent running inside the tagma**: the token is embedded
    ///   automatically in the spawned agent's environment, this should not happen.
    /// - **Operator user**: copy the token from the tagma startup output and
    ///   `export KALLIP_AUTH_TOKEN=<token>`.
    /// - **Automation**: set `KALLIP_AUTH_TOKEN` to the same value as the
    ///   tagma's `KALLIP_OPERATOR_TOKEN`.
    pub fn from_env() -> Result<Self> {
        let (url, token) = read_env_config()?;
        Self::builder(&url).auth_token(token).build()
    }

    /// Like [`from_env()`](Self::from_env), but injects a pre-built HTTP client.
    ///
    /// Use this when the caller needs to control TLS configuration (e.g.
    /// disabling cert verification for loopback-only connections in minimal
    /// containers).
    pub fn from_env_with_http(http: reqwest::Client) -> Result<Self> {
        let (url, token) = read_env_config()?;
        Self::builder(&url)
            .auth_token(token)
            .http_client(http)
            .build()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.inner.base_url)
    }

    /// Set Authorization: Bearer <token> if an auth token is configured.
    fn with_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
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

    /// Send request, parse structured JSON error on non-2xx, return raw
    /// response (for SSE streams that need the body as-is).
    async fn ensure_success(&self, response: reqwest::Response) -> Result<reqwest::Response> {
        if !response.status().is_success() {
            return Err(error_from_response(response).await);
        }
        Ok(response)
    }
}

// -- Env helpers ---------------------------------------------------------------

/// Read `KALLIP_TAGMA_URL` and `KALLIP_AUTH_TOKEN` from the
/// environment.  Returns `(url, token)`.
fn read_env_config() -> Result<(String, String)> {
    let url = std::env::var("KALLIP_TAGMA_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let token = std::env::var("KALLIP_AUTH_TOKEN").context(concat!(
        "KALLIP_AUTH_TOKEN is not set.\n",
        "\n",
        "How to obtain the token:\n",
        "- Agent (inside tagma): token is embedded automatically, check tagma setup.\n",
        "- Operator user: copy from tagma startup output, then:\n",
        "    export KALLIP_AUTH_TOKEN=<token>\n",
        "- Automation: start the tagma with a preset operator token:\n",
        "    KALLIP_OPERATOR_TOKEN=<secret> kallip-tagma\n",
        "  then use the same value for the client:\n",
        "    KALLIP_AUTH_TOKEN=<secret> kallip <command>",
    ))?;
    Ok((url, token))
}

/// The process-wide default `reqwest::Client`, constructed once and shared by
/// every `TagmaClient` that doesn't override `.http_client()`. Cloning a
/// `reqwest::Client` is cheap and shares the underlying pool.
fn shared_http() -> &'static reqwest::Client {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::ClientBuilder::new()
            .build()
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
    http: Option<reqwest::Client>,
}

impl TagmaClientBuilder {
    /// Set the bearer token for authenticating with the tagma.
    pub fn auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Override the default [`reqwest::Client`].
    ///
    /// Use this when you need custom TLS settings (e.g. disabling cert
    /// verification for loopback-only connections in minimal containers).
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http = Some(client);
        self
    }

    /// Consume the builder and produce a [`TagmaClient`].
    ///
    /// If no custom HTTP client was provided via `.http_client()`, a default
    /// `reqwest::Client` is constructed here.  Construction can fail if the
    /// system CA store is missing; callers that need to avoid this should
    /// supply their own client via `.http_client()`.
    pub fn build(self) -> Result<TagmaClient> {
        // Callers that don't override `.http_client()` share one process-wide
        // default `reqwest::Client` (constructed once, then cloned — the client
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
}
