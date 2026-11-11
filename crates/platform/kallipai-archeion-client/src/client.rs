//! Async HTTP client for the kallipai-archeion relay.
//!
//! Two surfaces:
//! - **Public** ([`ArcheionClient::enroll`]): first-run tagma enrollment. Unsigned
//!   at the HTTP layer; the device signs the enroll proof locally.
//! - **Admin** (the `admin_*` methods): operator management driving
//!   `/admin/*` with an `sk-admin-` bearer token.
//!
//! Tagma room discovery lives in `kallipai-lesche-client` (the chat domain
//! is lesche).
//!
//! All admin DTOs are shared with the server via [`kallipai_archeion_common::admin`],
//! so client and server cannot drift on the wire contract.

use std::sync::Arc;

use anyhow::{Context, Result};
use kallipai_archeion_common::admin::{
    CreateEnrollmentCodeRequest, CreateEnrollmentCodeResponse, Page, PageQuery, PasskeySummary,
    RotateAdminTokenResponse, UpdateUserRequest, UserSummary,
};
use kallipai_archeion_common::bytes::{Ed25519PublicKey, Ed25519Signature};
use kallipai_archeion_common::control::{EnrollRequest, EnrollResponse};
use kallipai_archeion_common::ids::TagmaId;
use kallipai_archeion_common::mint::MintEnrollmentCodeResponse;
use kallipai_archeion_common::proof::enroll_transcript;
use kallipai_archeion_common::tagmata::TagmaSummary;
use kallipai_common::protocol::ApiError;
use kallipai_common::telemetry::HttpClient;
use kallipai_e2ee::DeviceKey;

struct Inner {
    base_url: String,
    http: HttpClient,
    admin_token: Option<String>,
    /// The caller's personal access token: what a user-facing method (the
    /// self-service mint) authenticates with. Separate from `admin_token` so a
    /// client cannot spend one where the other was meant.
    access_token: Option<String>,
}

/// Async HTTP client for the kallipai-archeion relay.
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
            access_token: None,
            http: None,
        }
    }

    /// Construct a client from environment variables.
    ///
    /// Reads `KALLIPAI_ARCHEION_URL` (default: `http://127.0.0.1:7100`, the
    /// `KALLIPAI_ARCHEION_ADMIN_TOKEN` (the `sk-admin-` token, required) -- the same
    /// variable the archeion server reads, so a deployment defines the admin token
    /// once. For a tokenless (enroll-only) client, build via [`Self::builder`]
    /// directly.
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("KALLIPAI_ARCHEION_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7100".to_string());
        let token = std::env::var("KALLIPAI_ARCHEION_ADMIN_TOKEN")
            .context("KALLIPAI_ARCHEION_ADMIN_TOKEN required (the sk-admin- token)")?;
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
            "admin token required (set via builder.admin_token or KALLIPAI_ARCHEION_ADMIN_TOKEN)",
        )
    }

    fn with_admin_auth(
        &self,
        req: reqwest_middleware::RequestBuilder,
    ) -> reqwest_middleware::RequestBuilder {
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

    /// The configured access token, or an error if none was provided.
    fn require_access_token(&self) -> Result<&str> {
        self.inner
            .access_token
            .as_deref()
            .context("access token required (set via builder.access_token)")
    }

    fn with_access_auth(
        &self,
        req: reqwest_middleware::RequestBuilder,
    ) -> reqwest_middleware::RequestBuilder {
        // Same discipline as `with_admin_auth`: the caller must have gone
        // through `require_access_token()` first, so a method that forgets the
        // guard fails loudly in a debug build instead of sending an anonymous
        // request.
        debug_assert!(self.inner.access_token.is_some());
        req.bearer_auth(
            self.inner
                .access_token
                .as_deref()
                .expect("require_access_token was not called"),
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
    // -- user surface (personal access token) ----------------------------------

    /// Mint an enrollment code for the token's owner: `POST /tagmata` with the
    /// personal access token as the bearer.
    ///
    /// The code is single-use and short-lived; the pending tagma it names
    /// occupies one of the owner's live-pending slots until it enrolls or is
    /// revoked, and an expired code keeps holding its slot.
    pub async fn mint_enrollment_code(&self) -> Result<MintEnrollmentCodeResponse> {
        self.require_access_token()?;
        self.handle_response(
            self.with_access_auth(self.inner.http.post(self.url("/tagmata")))
                .send()
                .await
                .context("enrollment-code mint failed")?,
            "decode enrollment-code response",
        )
        .await
    }

    /// The token owner's tagmata: `GET /tagmata` with the personal access
    /// token. Pending and enrolled rows, newest first; revoked ones are not
    /// listed at all.
    pub async fn list_tagmata(&self) -> Result<Vec<TagmaSummary>> {
        self.require_access_token()?;
        self.handle_response(
            self.with_access_auth(self.inner.http.get(self.url("/tagmata")))
                .send()
                .await
                .context("tagma list failed")?,
            "decode tagma list",
        )
        .await
    }

    /// Revoke one of the token owner's tagmata: `DELETE /tagmata/{id}`.
    ///
    /// The platform answers 204 whether the row was live or already revoked,
    /// so a retry after a timeout is safe.
    pub async fn revoke_tagma(&self, id: &TagmaId) -> Result<()> {
        self.require_access_token()?;
        self.ensure_empty(
            self.with_access_auth(self.inner.http.delete(self.url(&format!("/tagmata/{id}"))))
                .send()
                .await
                .context("tagma revoke failed")?,
        )
        .await
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
    access_token: Option<String>,
    http: Option<HttpClient>,
}

impl ArcheionClientBuilder {
    /// Set the `sk-admin-` bearer token (required for the admin methods).
    pub fn admin_token(mut self, token: impl Into<String>) -> Self {
        self.admin_token = Some(token.into());
        self
    }

    /// Set the caller's personal access token (required for the user
    /// methods). Distinct from [`Self::admin_token`]: the two credentials
    /// reach different route families.
    pub fn access_token(mut self, token: impl Into<String>) -> Self {
        self.access_token = Some(token.into());
        self
    }

    /// Override the default [`HttpClient`] (e.g. for custom TLS).
    pub fn http_client(mut self, client: HttpClient) -> Self {
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
            None => kallipai_common::telemetry::http_client(
                reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)),
            )?,
        };
        Ok(ArcheionClient {
            inner: Arc::new(Inner {
                base_url: self.base_url,
                http,
                admin_token: self.admin_token,
                access_token: self.access_token,
            }),
        })
    }
}

/// Parse the archeion's `{"error":{"message":"..."}}` envelope into an [`ApiError`]
/// (falling back to the opening of the raw body if it is not the expected
/// shape: a proxy in front of the archeion can answer with a whole HTML page).
async fn parse_error(status: reqwest::StatusCode, response: reqwest::Response) -> anyhow::Error {
    let body = response.text().await.unwrap_or_default();
    let (message, code) = match serde_json::from_str::<ErrorEnvelope>(&body) {
        Ok(envelope) => (envelope.error.message, envelope.error.code),
        Err(_) => (opening_of(&body), None),
    };
    ApiError {
        status: status.as_u16(),
        message,
        dangling: None,
        code,
    }
    .into()
}

/// The opening of a body that is not the error envelope, bounded so one page
/// of HTML cannot land in an error line. The status line already carries the
/// fact an operator acts on.
fn opening_of(body: &str) -> String {
    const LIMIT: usize = 200;
    let body = body.trim();
    if body.chars().count() <= LIMIT {
        return body.to_owned();
    }
    let mut opening: String = body.chars().take(LIMIT).collect();
    opening.push('…');
    opening
}
#[derive(serde::Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    message: String,
    /// The machine-readable reason a caller may branch on (`perm_missing` /
    /// `user_required`, the gateway's key-lifecycle codes), absent where the
    /// status line says everything.
    #[serde(default)]
    code: Option<String>,
}

#[cfg(test)]
mod tests {
    //! Assert each method maps its request shape + status code to the right
    //! return type, against a wiremock archeion. DTOs are shared with the server,
    //! so these tests lock the request shape, not the DTO definitions.

    use super::*;
    use kallipai_archeion_common::admin::CreateEnrollmentCodeRequest;
    use kallipai_archeion_common::tagmata::TagmaState;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer, admin: bool) -> ArcheionClient {
        let mut b = ArcheionClient::builder(&server.uri());
        if admin {
            b = b.admin_token("sk-admin-test");
        }
        b.build().unwrap()
    }

    /// A client holding the personal access token, which is what the
    /// user-facing calls (mint, list, revoke) authenticate with.
    fn user_client(server: &MockServer) -> ArcheionClient {
        ArcheionClient::builder(&server.uri())
            .access_token("sk-user-test")
            .build()
            .unwrap()
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

    #[tokio::test]
    async fn mint_posts_tagmata_with_the_personal_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tagmata"))
            .and(header("authorization", "Bearer sk-user-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "sk-enroll-z",
                "id": "tagma-9",
                "created_at": "2026-01-01T00:00:00Z",
                "expires_at": "2026-01-01T01:00:00Z",
            })))
            .mount(&server)
            .await;
        let minted = ArcheionClient::builder(&server.uri())
            .access_token("sk-user-test")
            .build()
            .unwrap()
            .mint_enrollment_code()
            .await
            .expect("ok");
        assert_eq!(minted.code, "sk-enroll-z");
        assert_eq!(minted.id, "tagma-9");
        assert!(minted.expires_at > minted.created_at);
    }

    #[tokio::test]
    async fn mint_without_a_personal_token_errors() {
        let server = MockServer::start().await;
        let err = client(&server, true)
            .mint_enrollment_code()
            .await
            .expect_err("no access token");
        assert!(
            format!("{err:#}").contains("access token required"),
            "expected token-required error, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn list_tagmata_decodes_pending_and_enrolled_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tagmata"))
            .and(header("authorization", "Bearer sk-user-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "tagma_id": "tagma-2",
                    "label": null,
                    "state": "pending",
                    "created_at": "2026-01-02T00:00:00Z",
                    "code_masked": "sk-enroll-abc***xyz",
                    "expires_at": "2026-01-02T01:00:00Z"
                },
                {
                    "tagma_id": "tagma-1",
                    "label": "laptop",
                    "state": "enrolled",
                    "created_at": "2026-01-01T00:00:00Z"
                }
            ])))
            .mount(&server)
            .await;
        let rows = user_client(&server).list_tagmata().await.expect("ok");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tagma_id.as_ref(), "tagma-2");
        assert_eq!(rows[0].state, TagmaState::Pending);
        assert!(rows[0].code_masked.is_some());
        assert!(rows[0].expires_at.is_some());
        assert_eq!(rows[1].state, TagmaState::Enrolled);
        assert!(rows[1].code_masked.is_none());
        assert!(rows[1].expires_at.is_none());
        assert_eq!(rows[1].label.as_deref(), Some("laptop"));
    }

    #[tokio::test]
    async fn list_tagmata_without_a_personal_token_errors() {
        let server = MockServer::start().await;
        let err = client(&server, true)
            .list_tagmata()
            .await
            .expect_err("no access token");
        assert!(
            format!("{err:#}").contains("access token required"),
            "expected token-required error, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn revoke_tagma_accepts_a_204() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/tagmata/tagma-1"))
            .and(header("authorization", "Bearer sk-user-test"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        user_client(&server)
            .revoke_tagma(&TagmaId::from("tagma-1".to_owned()))
            .await
            .expect("revoked");
    }

    #[tokio::test]
    async fn a_revoke_error_keeps_the_platform_code() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/tagmata/tagma-1"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {
                    "message": "missing permission: archeion:tagma:write",
                    "code": "perm_missing"
                }
            })))
            .mount(&server)
            .await;
        let err = user_client(&server)
            .revoke_tagma(&TagmaId::from("tagma-1".to_owned()))
            .await
            .expect_err("403");
        let api = err.downcast_ref::<ApiError>().expect("ApiError");
        assert_eq!(api.status, 403);
        assert_eq!(api.code.as_deref(), Some("perm_missing"));
    }

    #[tokio::test]
    async fn a_forbidden_body_keeps_its_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tagmata"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {
                    "message": "missing permission: archeion:tagma:write",
                    "code": "perm_missing",
                }
            })))
            .mount(&server)
            .await;
        let err = ArcheionClient::builder(&server.uri())
            .access_token("sk-user-test")
            .build()
            .unwrap()
            .mint_enrollment_code()
            .await
            .expect_err("403");
        // The two 403s are told apart by this code alone: dropping it here
        // collapses "missing permission" and "not a user principal" into
        // one message the caller cannot act on.
        let api = err.downcast_ref::<ApiError>().expect("ApiError");
        assert_eq!(api.status, 403);
        assert_eq!(api.code.as_deref(), Some("perm_missing"));
    }

    #[tokio::test]
    async fn a_non_envelope_body_is_cut_short() {
        let server = MockServer::start().await;
        let page = format!("<html><body>{}</body></html>", "x".repeat(400));
        Mock::given(method("POST"))
            .and(path("/tagmata"))
            .respond_with(ResponseTemplate::new(502).set_body_string(page))
            .mount(&server)
            .await;
        let err = ArcheionClient::builder(&server.uri())
            .access_token("sk-user-test")
            .build()
            .unwrap()
            .mint_enrollment_code()
            .await
            .expect_err("502");
        let api = err.downcast_ref::<ApiError>().expect("ApiError");
        assert_eq!(api.status, 502);
        assert!(api.message.ends_with('…'), "{}", api.message);
        assert_eq!(api.message.chars().count(), 201);
        assert!(api.code.is_none());
    }

    // -- the trace a call belongs to -----------------------------------------

    /// Serve `app` on an ephemeral port and hand back its base URL.
    async fn spawn_stub(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// An archeion answering the two calls under test, with the server span
    /// layered on so a recorder sees a request arrive.
    async fn spawn_archeion_stub() -> String {
        let app = kallipai_common::telemetry::with_server_span(
            axum::Router::new()
                .route(
                    "/healthz",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                )
                .route(
                    "/tagmata/enroll",
                    axum::routing::post(|| async {
                        axum::Json(serde_json::json!({
                            "tagma_id": "tagma-1",
                            "tagma_token": "sk-tagma-x",
                        }))
                    }),
                ),
        );
        spawn_stub(app).await
    }

    /// The span a caller would be in when it makes the call.
    fn caller_span() -> tracing::Span {
        kallipai_common::telemetry::request_span(
            "PUT",
            "/admin/tagmata",
            "/admin/tagmata",
            &axum::http::HeaderMap::new(),
        )
    }

    /// Both calls of this client keep the caller's trace: the archeion answers
    /// inside the span of the call.
    #[tokio::test]
    async fn a_call_carries_the_context_of_the_request_it_serves() {
        use kallipai_common::telemetry::test_observability::{
            client_span_of, recorder, span_of, spans_until,
        };
        use tracing::Instrument as _;

        let base = spawn_archeion_stub().await;
        let device = DeviceKey::generate();
        let (recorder, subscriber) = recorder("kallipai-archeion-client");
        {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            let client = ArcheionClient::builder(&base).build().unwrap();
            async {
                client.healthz().await.expect("healthz");
                client
                    .enroll("sk-enroll-code", &device)
                    .await
                    .expect("enroll");
            }
            .instrument(caller_span())
            .await;
        }

        let spans = spans_until(&recorder, 5).await;
        let asked = span_of(&spans, "PUT /admin/tagmata", "server");
        for (method, path) in [("GET", "/healthz"), ("POST", "/tagmata/enroll")] {
            let call = client_span_of(&spans, method, path);
            let answered = span_of(&spans, &format!("{method} {path}"), "server");
            assert_eq!(
                call.parent_span_id.as_deref(),
                Some(asked.span_id.as_str()),
                "{method} {path}: the call hangs under the request that made it"
            );
            assert_eq!(
                answered.parent_span_id.as_deref(),
                Some(call.span_id.as_str()),
                "{method} {path}: the archeion answered inside the span of the call"
            );
            assert_eq!(
                answered.trace_id, call.trace_id,
                "{method} {path}: one trace spans the call, not a second one at the archeion"
            );
            assert_eq!(
                call.trace_id, asked.trace_id,
                "{method} {path}: the request's trace"
            );
        }
    }

    /// The same call on a client with no middleware: nothing puts a header on
    /// the wire, so the archeion starts a trace of its own. Without this, the
    /// test above would pass on a header read back at the caller.
    #[tokio::test]
    async fn a_call_that_carries_no_header_leaves_the_archeion_its_own_trace() {
        use kallipai_common::telemetry::test_observability::{recorder, span_of, spans_until};
        use tracing::Instrument as _;

        let base = spawn_archeion_stub().await;
        let (recorder, subscriber) = recorder("kallipai-archeion-client");
        {
            let _subscribed = tracing::subscriber::set_default(subscriber);
            // The bypass, written out: a client with no middleware.
            let bare = reqwest_middleware::ClientWithMiddleware::from(reqwest::Client::new());
            let client = ArcheionClient::builder(&base)
                .http_client(bare)
                .build()
                .unwrap();
            async { client.healthz().await.expect("healthz") }
                .instrument(caller_span())
                .await;
        }

        let spans = spans_until(&recorder, 2).await;
        let asked = span_of(&spans, "PUT /admin/tagmata", "server");
        let answered = span_of(&spans, "GET /healthz", "server");
        assert_eq!(
            answered.parent_span_id, None,
            "a request with no header is a root at the archeion"
        );
        assert_ne!(
            answered.trace_id, asked.trace_id,
            "the archeion did not join the caller's trace"
        );
    }
}
