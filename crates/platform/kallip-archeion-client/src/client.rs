//! Async HTTP client for the kallip-archeion relay.
//!
//! Two surfaces:
//! - **Public** ([`ArcheionClient::enroll`]): first-run tagma enrollment. Unsigned
//!   at the HTTP layer; the device signs the enroll proof locally.
//! - **Admin** (the `admin_*` methods): operator management driving
//!   `/admin/*` with an `sk-admin-` bearer token.
//!
//! Tagma room discovery lives in `kallip-lesche-client` (the chat domain
//! is lesche).
//!
//! All admin DTOs are shared with the server via [`kallip_archeion_common::admin`],
//! so client and server cannot drift on the wire contract.

use std::sync::Arc;

use anyhow::{Context, Result};
use kallip_archeion_common::admin::{
    CreateEnrollmentCodeRequest, CreateEnrollmentCodeResponse, Page, PageQuery, PasskeySummary,
    RotateAdminTokenResponse, UpdateUserRequest, UserSummary,
};
use kallip_archeion_common::bytes::{Ed25519PublicKey, Ed25519Signature};
use kallip_archeion_common::control::{EnrollRequest, EnrollResponse};
use kallip_archeion_common::ids::TagmaId;
use kallip_archeion_common::proof::enroll_transcript;
use kallip_common::protocol::ApiError;
use kallip_e2ee::DeviceKey;

struct Inner {
    base_url: String,
    http: reqwest::Client,
    admin_token: Option<String>,
}

/// Async HTTP client for the kallip-archeion relay.
#[derive(Clone)]
pub struct ArcheionClient {
    inner: Arc<Inner>,
}

impl ArcheionClient {
    /// Start building an [`ArcheionClient`].
    ///
    /// `base_url` is the archeion control-plane root: the bare origin in
    /// direct mode (e.g. `http://127.0.0.1:7100`) or the edge service root
    /// including the /v1/archeion service segment (e.g.
    /// `https://api.example.com/v1/archeion`) through the platform edge.
    /// The tails the methods append are bare.
    pub fn builder(base_url: &str) -> ArcheionClientBuilder {
        ArcheionClientBuilder {
            base_url: base_url.trim_end_matches('/').to_owned(),
            admin_token: None,
            http: None,
        }
    }

    /// Construct a client from environment variables.
    ///
    /// Reads `KALLIP_ARCHEION_URL` (default: `http://127.0.0.1:7100`, the
    /// `KALLIP_ARCHEION_ADMIN_TOKEN` (the `sk-admin-` token, required) -- the same
    /// variable the archeion server reads, so a deployment defines the admin token
    /// once. For a tokenless (enroll-only) client, build via [`Self::builder`]
    /// directly.
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("KALLIP_ARCHEION_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7100".to_string());
        let token = std::env::var("KALLIP_ARCHEION_ADMIN_TOKEN")
            .context("KALLIP_ARCHEION_ADMIN_TOKEN required (the sk-admin- token)")?;
        Self::builder(&url).admin_token(token).build()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.inner.base_url)
    }

    /// The configured admin token, or an error if none was provided. Admin
    /// methods are bearer-gated; calling one without a token is a programmer
    /// error, not a runtime condition.
    fn require_admin_token(&self) -> Result<&str> {
        self.inner.admin_token.as_deref().context(
            "admin token required (set via builder.admin_token or KALLIP_ARCHEION_ADMIN_TOKEN)",
        )
    }

    fn with_admin_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        // Every admin method must call `require_admin_token()` first. Assert it
        // here so a future method that forgets the guard fails loudly in debug
        // builds rather than silently sending an unauthenticated request.
        debug_assert!(self.inner.admin_token.is_some());
        req.bearer_auth(
            self.inner
                .admin_token
                .as_deref()
                .expect("require_admin_token was not called"),
        )
    }

    // -- HTTP helpers ---------------------------------------------------------

    /// Send request, parse the structured JSON error on non-2xx, deserialize the
    /// success body as `T`. The archeion returns errors as
    /// `{"error":{"message":"..."}}` (the shared [`ApiError`] envelope).
    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
        context_msg: &'static str,
    ) -> Result<T> {
        let status = response.status();
        if !status.is_success() {
            return Err(parse_error(status, response).await);
        }
        response.json().await.context(context_msg)
    }

    /// Like [`Self::handle_response`] but for empty success bodies (204 No
    /// Content). Returns `Ok(())` on 2xx.
    async fn ensure_empty(&self, response: reqwest::Response) -> Result<()> {
        let status = response.status();
        if !status.is_success() {
            return Err(parse_error(status, response).await);
        }
        Ok(())
    }

    // -- public surface -------------------------------------------------------

    /// Reachability probe: `GET /healthz`. Unauthenticated; confirms the server
    /// is up and serving.
    pub async fn healthz(&self) -> Result<()> {
        let resp = self
            .inner
            .http
            .get(self.url("/healthz"))
            .send()
            .await
            .context("healthz GET failed")?;
        if !resp.status().is_success() {
            return Err(parse_error(resp.status(), resp).await);
        }
        Ok(())
    }

    /// Verify the admin credential: `GET /admin` with the bearer. Unlike
    /// [`Self::healthz`] (an unauthenticated reachability probe), this confirms
    /// the admin token is actually accepted, distinguishing "server down" from
    /// "wrong token".
    pub async fn admin_verify_token(&self) -> Result<()> {
        self.require_admin_token()?;
        let resp = self
            .with_admin_auth(self.inner.http.get(self.url("/admin")))
            .send()
            .await
            .context("admin ping failed")?;
        if !resp.status().is_success() {
            return Err(parse_error(resp.status(), resp).await);
        }
        Ok(())
    }

    /// Enroll a tagma with the archeion using a single-use enrollment `code`.
    ///
    /// Unsigned at the HTTP layer; `device` signs the enroll transcript locally.
    /// Returns the assigned tagma id and the `sk-tagma-` token (returned once).
    pub async fn enroll(&self, code: &str, device: &DeviceKey) -> Result<(TagmaId, String)> {
        let public = device.public_bytes();
        let signature = device.sign(&enroll_transcript(code, &public));
        let req = EnrollRequest {
            code: code.to_string(),
            device_public_key: Ed25519PublicKey(public.to_vec()),
            signature: Ed25519Signature(signature.to_vec()),
        };
        let resp: EnrollResponse = self
            .handle_response(
                self.inner
                    .http
                    .post(self.url("/tagmata/enroll"))
                    .json(&req)
                    .send()
                    .await
                    .context("enrollment POST failed")?,
                "decode enrollment response",
            )
            .await?;
        Ok((resp.tagma_id, resp.tagma_token))
    }

    // -- admin surface (sk-admin- bearer) -------------------------------------

    /// Mint an enrollment code on a user's behalf. Returns the `sk-enroll-...`
    /// plaintext (once).
    pub async fn admin_create_enrollment_code(
        &self,
        body: CreateEnrollmentCodeRequest,
    ) -> Result<CreateEnrollmentCodeResponse> {
        self.require_admin_token()?;
        self.handle_response(
            self.with_admin_auth(self.inner.http.post(self.url("/admin/tagmata")).json(&body))
                .send()
                .await
                .context("create enrollment code failed")?,
            "decode enrollment code response",
        )
        .await
    }

    /// List users (paginated).
    pub async fn admin_list_users(&self, query: &PageQuery) -> Result<Page<UserSummary>> {
        self.require_admin_token()?;
        self.handle_response(
            self.with_admin_auth(self.inner.http.get(self.url("/admin/users")).query(query))
                .send()
                .await
                .context("list users failed")?,
            "decode user page",
        )
        .await
    }

    /// Disable (`disabled = true`) or re-enable (`false`) a user. Returns the
    /// refreshed user summary.
    pub async fn admin_update_user(
        &self,
        user_id: &str,
        body: UpdateUserRequest,
    ) -> Result<UserSummary> {
        self.require_admin_token()?;
        self.handle_response(
            self.with_admin_auth(
                self.inner
                    .http
                    .patch(self.url(&format!("/admin/users/{user_id}")))
                    .json(&body),
            )
            .send()
            .await
            .context("update user failed")?,
            "decode user response",
        )
        .await
    }

    /// List a user's passkeys.
    pub async fn admin_list_user_passkeys(&self, user_id: &str) -> Result<Vec<PasskeySummary>> {
        self.require_admin_token()?;
        self.handle_response(
            self.with_admin_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/admin/users/{user_id}/passkeys"))),
            )
            .send()
            .await
            .context("list passkeys failed")?,
            "decode passkey list",
        )
        .await
    }

    /// Revoke a passkey by id (hard-delete + audit row). A second revoke of the
    /// now-absent id returns 404.
    pub async fn admin_revoke_passkey(&self, passkey_id: &str) -> Result<()> {
        self.require_admin_token()?;
        self.ensure_empty(
            self.with_admin_auth(
                self.inner
                    .http
                    .delete(self.url(&format!("/admin/passkeys/{passkey_id}"))),
            )
            .send()
            .await
            .context("revoke passkey failed")?,
        )
        .await
    }

    /// Rotate the minted admin token: `POST /admin/token/rotate`. Returns the
    /// fresh `sk-admin-...` plaintext (once); the old token is invalid after.
    pub async fn admin_rotate_token(&self) -> Result<RotateAdminTokenResponse> {
        self.require_admin_token()?;
        self.handle_response(
            self.with_admin_auth(self.inner.http.post(self.url("/admin/token/rotate")))
                .send()
                .await
                .context("admin token rotation failed")?,
            "decode admin token rotation response",
        )
        .await
    }
}

/// Build an [`ArcheionClient`] with optional overrides.
pub struct ArcheionClientBuilder {
    base_url: String,
    admin_token: Option<String>,
    http: Option<reqwest::Client>,
}

impl ArcheionClientBuilder {
    /// Set the `sk-admin-` bearer token (required for the admin methods).
    pub fn admin_token(mut self, token: impl Into<String>) -> Self {
        self.admin_token = Some(token.into());
        self
    }

    /// Override the default [`reqwest::Client`] (e.g. for custom TLS).
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http = Some(client);
        self
    }

    /// Consume the builder and produce an [`ArcheionClient`].
    pub fn build(self) -> Result<ArcheionClient> {
        // The default client carries a total request timeout: every method on
        // this client is a request/reply with a natural end (enroll, admin
        // calls, healthz). Callers needing a long-lived stream override it via
        // [`Self::http_client`].
        let http = match self.http {
            Some(client) => client,
            None => reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
        };
        Ok(ArcheionClient {
            inner: Arc::new(Inner {
                base_url: self.base_url,
                http,
                admin_token: self.admin_token,
            }),
        })
    }
}

/// Parse the archeion's `{"error":{"message":"..."}}` envelope into an [`ApiError`]
/// (falling back to the raw body if it is not the expected shape).
async fn parse_error(status: reqwest::StatusCode, response: reqwest::Response) -> anyhow::Error {
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ErrorEnvelope>(&body)
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

#[derive(serde::Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    message: String,
}

#[cfg(test)]
mod tests {
    //! Assert each method maps its request shape + status code to the right
    //! return type, against a wiremock archeion. DTOs are shared with the server,
    //! so these tests lock the request shape, not the DTO definitions.

    use super::*;
    use kallip_archeion_common::admin::CreateEnrollmentCodeRequest;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer, admin: bool) -> ArcheionClient {
        let mut b = ArcheionClient::builder(&server.uri());
        if admin {
            b = b.admin_token("sk-admin-test");
        }
        b.build().unwrap()
    }

    #[tokio::test]
    async fn admin_list_users_paginates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/admin/users"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{
                    "id": "u1",
                    "username": "alice",
                    "email": "a@x.test",
                    "display_name": null,
                    "created_at": "2026-01-01T00:00:00Z",
                    "disabled_at": null,
                }],
                "next_cursor": null,
            })))
            .mount(&server)
            .await;
        let page = client(&server, true)
            .admin_list_users(&PageQuery::default())
            .await
            .expect("ok");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].username, "alice");
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn admin_revoke_passkey_is_empty_204() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/admin/passkeys/some-id"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        client(&server, true)
            .admin_revoke_passkey("some-id")
            .await
            .expect("ok");
    }

    #[tokio::test]
    async fn admin_error_envelope_is_parsed() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/admin/users/nope"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": { "message": "unknown user" }
            })))
            .mount(&server)
            .await;
        let err = client(&server, true)
            .admin_update_user("nope", UpdateUserRequest { disabled: true })
            .await
            .expect_err("404");
        let api = err.downcast_ref::<ApiError>().expect("ApiError");
        assert_eq!(api.status, 404);
        assert_eq!(api.message, "unknown user");
    }

    #[tokio::test]
    async fn admin_method_without_token_errors() {
        let server = MockServer::start().await;
        let err = client(&server, false)
            .admin_list_users(&PageQuery::default())
            .await
            .expect_err("no token");
        assert!(
            err.to_string().contains("admin token required"),
            "expected token-required error, got: {err}"
        );
    }

    #[tokio::test]
    async fn enroll_decodes_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tagmata/enroll"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tagma_id": "tagma-1",
                "tagma_token": "sk-tagma-x",
            })))
            .mount(&server)
            .await;
        let device = DeviceKey::generate();
        let (id, token) = client(&server, false)
            .enroll("sk-enroll-code", &device)
            .await
            .expect("ok");
        assert_eq!(id.to_string(), "tagma-1");
        assert_eq!(token, "sk-tagma-x");
    }

    #[tokio::test]
    async fn admin_create_enrollment_code_round_trips() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/admin/tagmata"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "sk-enroll-z" })),
            )
            .mount(&server)
            .await;
        let resp = client(&server, true)
            .admin_create_enrollment_code(CreateEnrollmentCodeRequest {
                user_id: "u1".to_string(),
            })
            .await
            .expect("ok");
        assert_eq!(resp.code, "sk-enroll-z");
    }
}
