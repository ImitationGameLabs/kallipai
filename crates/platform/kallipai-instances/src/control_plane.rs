//! Thin HTTP client for the archeion's service-to-service auth calls.
//!
//! The lesche reaches the archeion through `HttpControlPlane` (private to that
//! crate, and implementing the full six-method `ControlPlane` trait); this
//! proxy needs exactly two calls (bearer + session verify), so it carries
//! a verifier of its own rather than pulling the relay crate in or
//! refactoring a shared client out (a tracked follow-up if a third
//! consumer appears).

use kallipai_archeion_common::control_plane::{ControlPlaneError, VerifiedSession};
use kallipai_archeion_common::internal_api::{
    VerifyBearerRequest, VerifyBearerResponse, VerifySessionRequest,
};
use kallipai_archeion_common::principal::Principal;

/// Per-call timeout: a tiny JSON round trip against a local archeion; 10s is a
/// generous backstop, matching the lesche's client.
const INTERNAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What the auth guard needs from an auth backend: verify one bearer token
/// or one session cookie.
#[async_trait::async_trait]
pub trait AuthVerifier: Send + Sync {
    /// `Err` = the backend could not be reached or answered outside the
    /// contract (the guard fails closed on this); `Ok(None)` = the token is
    /// simply not valid.
    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError>;

    /// Verify a `kallipai_session` cookie value against the archeion. Same error
    /// contract as [`Self::verify_bearer`]: `Ok(None)` = absent/expired/
    /// disabled, `Ok(Some)` carries the `local_admin` flag the guard's
    /// cookie channel admits on.
    async fn verify_session(
        &self,
        cookie: &str,
    ) -> Result<Option<VerifiedSession>, ControlPlaneError>;
}

/// Archeion-backed [`AuthVerifier`]: one POST per call to the archeion's
/// `/internal/verify-bearer` / `/internal/verify-session`, guarded by the
/// shared internal secret.
#[derive(Clone)]
pub struct ArcheionVerifier {
    /// Archeion internal root (e.g. `http://127.0.0.1:7100`).
    base_url: String,
    /// Shared secret matching the archeion's provisioned internal token (the
    /// value in KALLIPAI_POLIS_INTERNAL_TOKEN_FILE).
    internal_token: String,
    http: reqwest::Client,
}

impl ArcheionVerifier {
    pub fn new(base_url: String, internal_token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(INTERNAL_TIMEOUT)
            .build()
            .expect("build reqwest client");
        Self {
            base_url,
            internal_token,
            http,
        }
    }
}

#[async_trait::async_trait]
impl AuthVerifier for ArcheionVerifier {
    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError> {
        let response = self
            .http
            .post(format!("{}/internal/verify-bearer", self.base_url))
            .bearer_auth(&self.internal_token)
            .json(&VerifyBearerRequest {
                token: token.to_string(),
            })
            .send()
            .await
            .map_err(|e| ControlPlaneError::Backend(e.to_string()))?;
        match response.status().as_u16() {
            200 => response
                .json::<VerifyBearerResponse>()
                .await
                .map(|r| Some(Principal::from(r.principal)))
                .map_err(|e| ControlPlaneError::Backend(e.to_string())),
            404 => Ok(None),
            status => Err(ControlPlaneError::Backend(format!(
                "archeion /internal/verify-bearer returned HTTP {status}"
            ))),
        }
    }

    async fn verify_session(
        &self,
        cookie: &str,
    ) -> Result<Option<VerifiedSession>, ControlPlaneError> {
        let response = self
            .http
            .post(format!("{}/internal/verify-session", self.base_url))
            .bearer_auth(&self.internal_token)
            .json(&VerifySessionRequest {
                cookie: cookie.to_string(),
            })
            .send()
            .await
            .map_err(|e| ControlPlaneError::Backend(e.to_string()))?;
        match response.status().as_u16() {
            200 => response
                .json::<VerifiedSession>()
                .await
                .map(Some)
                .map_err(|e| ControlPlaneError::Backend(e.to_string())),
            404 => Ok(None),
            status => Err(ControlPlaneError::Backend(format!(
                "archeion /internal/verify-session returned HTTP {status}"
            ))),
        }
    }
}
