//! Request authentication for the files service, mirroring the relay's
//! shape: an [`AuthPrincipal`] extractor resolves a request to a
//! [`Principal`] by delegating credential verification to the control-plane
//! client. A bearer credential is preferred; the `kallipai_session` cookie is
//! the fallback; neither means anonymous -> 401. Verification is per request
//! with no cache (the deliberate control-plane design): revocation latency
//! is one round trip, not a TTL.
//!
//! Failure mapping: a control-plane transport/backend failure means the
//! registry cannot answer, and an unanswered registry must never open the
//! door -- it maps to 503 (fail-closed, the degrade default; the
//! `KALLIPAI_FILES_DEGRADE=soft` switch narrows which decisions degrade, see
//! `crate::acl`, it does not weaken verification).

use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use kallipai_archeion_common::control_plane::{
    ControlPlane, ControlPlaneError, EnrollmentLookup, TagmaProfile, UserIdentity, VerifiedSession,
};
use kallipai_archeion_common::ids::{TagmaId, UserId};
use kallipai_archeion_common::internal_api::{
    EnrollmentLookupRequest, EnrollmentLookupResponse, TagmaProfilesRequest, TagmaProfilesResponse,
    TunnelProofTsRequest, TunnelProofTsResponse, UserIdentitiesRequest, UserIdentitiesResponse,
    UserIdentityByUsernameRequest, UserIdentityResponse, VerifyBearerRequest, VerifyBearerResponse,
    VerifySessionRequest, VerifySessionResponse,
};
use kallipai_archeion_common::principal::Principal;
use kallipai_common::auth_header::extract_bearer_token;
use kallipai_common::protocol::ApiError;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::state::AppState;

/// Per-call timeout for an `/internal/*` round trip. Pinned to the same 10s
/// contract the relay's client carries (see its timeout-contract test):
/// same-host tiny JSON calls, a generous backstop rather than expected
/// latency.
const INTERNAL_TIMEOUT: Duration = Duration::from_secs(10);

/// A reqwest-backed [`ControlPlane`] calling the archeion's `/internal/*` API,
/// the files service's twin of the relay's client.
#[derive(Clone)]
pub struct FilesControlPlane {
    /// Archeion internal root (e.g. `http://127.0.0.1:7100`); `/internal/...`
    /// is appended per call.
    base_url: String,
    /// Plaintext shared secret sent as `Authorization: Bearer <token>`;
    /// must equal the archeion's provisioned internal secret (the value in
    /// KALLIPAI_POLIS_INTERNAL_TOKEN_FILE).
    token: String,
    http: reqwest::Client,
}

impl FilesControlPlane {
    /// Build the client. `base_url` is the archeion's internal root; `token`
    /// is the shared secret.
    pub fn new(base_url: String, token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(INTERNAL_TIMEOUT)
            .build()
            .expect("build reqwest client");
        Self {
            base_url,
            token,
            http,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// POST `body` to `path`; map `200` -> `Some(deserialized)`, `404` ->
    /// `None`, any other status or transport error -> `Backend`.
    async fn post<Req, Resp>(
        &self,
        path: &str,
        body: &Req,
    ) -> Result<Option<Resp>, ControlPlaneError>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| ControlPlaneError::Backend(e.to_string()))?;
        match resp.status().as_u16() {
            200 => resp
                .json::<Resp>()
                .await
                .map(Some)
                .map_err(|e| ControlPlaneError::Backend(e.to_string())),
            404 => Ok(None),
            status => Err(ControlPlaneError::Backend(format!(
                "archeion {path} returned HTTP {status}"
            ))),
        }
    }
}

#[async_trait::async_trait]
impl ControlPlane for FilesControlPlane {
    async fn verify_session(
        &self,
        cookie_value: &str,
    ) -> Result<Option<VerifiedSession>, ControlPlaneError> {
        let resp: Option<VerifySessionResponse> = self
            .post(
                "/internal/verify-session",
                &VerifySessionRequest {
                    cookie: cookie_value.to_string(),
                },
            )
            .await?;
        // The wire body aliases VerifiedSession: no field mapping.
        Ok(resp)
    }

    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError> {
        let resp: Option<VerifyBearerResponse> = self
            .post(
                "/internal/verify-bearer",
                &VerifyBearerRequest {
                    token: token.to_string(),
                },
            )
            .await?;
        // The wire enum is the bearer-reachable subset of Principal; the
        // From impl (in internal_api) is the single mapping site.
        Ok(resp.map(|r| Principal::from(r.principal)))
    }

    async fn tagma_profiles(
        &self,
        tagma_ids: &[TagmaId],
    ) -> Result<Vec<TagmaProfile>, ControlPlaneError> {
        // The files service never calls this (a relay face); served through
        // the same helper for contract completeness.
        let resp: Option<TagmaProfilesResponse> = self
            .post(
                "/internal/tagma-profiles",
                &TagmaProfilesRequest {
                    tagma_ids: tagma_ids.to_vec(),
                },
            )
            .await?;
        Ok(resp.map(|r| r.profiles).unwrap_or_default())
    }

    async fn user_identities(
        &self,
        user_ids: &[UserId],
    ) -> Result<Vec<UserIdentity>, ControlPlaneError> {
        let resp: Option<UserIdentitiesResponse> = self
            .post(
                "/internal/user-identities",
                &UserIdentitiesRequest {
                    user_ids: user_ids.to_vec(),
                },
            )
            .await?;
        Ok(resp.map(|r| r.users).unwrap_or_default())
    }

    async fn user_identity_by_username(
        &self,
        username: &str,
    ) -> Result<Option<UserIdentity>, ControlPlaneError> {
        let resp: Option<UserIdentityResponse> = self
            .post(
                "/internal/user-identity-by-username",
                &UserIdentityByUsernameRequest {
                    username: username.to_string(),
                },
            )
            .await?;
        Ok(resp)
    }

    async fn enrollment_lookup(
        &self,
        tagma_id: &TagmaId,
    ) -> Result<Option<EnrollmentLookup>, ControlPlaneError> {
        // The ACL's facts read (crate::acl): per request, no cache; the wire
        // body aliases the trait type -- no field mapping.
        let resp: Option<EnrollmentLookupResponse> = self
            .post(
                "/internal/enrollment-lookup",
                &EnrollmentLookupRequest {
                    tagma_id: tagma_id.clone(),
                },
            )
            .await?;
        Ok(resp)
    }

    async fn bump_tunnel_proof_ts(
        &self,
        tagma_id: &TagmaId,
        ts: i64,
    ) -> Result<bool, ControlPlaneError> {
        // A relay face, like the profiles reads; never called here.
        let resp: Option<TunnelProofTsResponse> = self
            .post(
                "/internal/tunnel-proof-ts",
                &TunnelProofTsRequest {
                    tagma_id: tagma_id.clone(),
                    ts,
                },
            )
            .await?;
        Ok(resp.map(|r| r.fresh).unwrap_or(false))
    }
}

/// The axum extractor resolving a request to a [`Principal`] via the
/// registry. Bearer preferred, cookie fallback, neither -> 401. `None` from
/// a verify call (revoked/unknown) -> 401: revocation takes effect on the
/// very next request.
#[derive(Debug, Clone)]
pub struct AuthPrincipal(pub Principal);

impl FromRequestParts<AppState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Prefer an explicit bearer credential when present.
        if let Ok(bearer) = extract_bearer_token(&parts.headers) {
            let principal = state
                .control
                .verify_bearer(bearer)
                .await
                .map_err(control_unavailable)?
                .ok_or_else(|| ApiError::unauthorized("invalid token"))?;
            return Ok(AuthPrincipal(principal));
        }
        // Otherwise fall back to the session cookie.
        if let Some(cookie_value) = read_session_cookie(&parts.headers) {
            let user = state
                .control
                .verify_session(&cookie_value)
                .await
                .map_err(control_unavailable)?
                .ok_or_else(|| ApiError::unauthorized("invalid session"))?;
            return Ok(AuthPrincipal(Principal::User(user.user_id)));
        }
        Err(ApiError::unauthorized("authentication required"))
    }
}

/// Control-plane failures never open the door: the registry is a hard
/// dependency of every authenticated decision, so a backend failure is a
/// 503 (fail-closed) rather than a 500 -- the caller should retry, not
/// report a bug.
fn control_unavailable(e: ControlPlaneError) -> ApiError {
    ApiError::unavailable(format!("registry unavailable: {e}"))
}

/// Read the session cookie value from a request's `Cookie` header, if
/// present. Mirrors the relay's helper: multiple `Cookie` headers and
/// multiple `name=value` pairs within one are both tolerated; first match
/// wins.
pub(crate) fn read_session_cookie(headers: &HeaderMap) -> Option<String> {
    for header in headers.get_all(axum::http::header::COOKIE) {
        let Ok(s) = header.to_str() else {
            continue;
        };
        for cookie in cookie::Cookie::split_parse(s) {
            let Ok(cookie) = cookie else {
                continue;
            };
            if cookie.name() == SESSION_COOKIE_NAME {
                return Some(cookie.value().to_string());
            }
        }
    }
    None
}

/// Session cookie name. Mirrors the registry's `kallipai_session`; a stable
/// wire literal kept here (duplicated from `kallipai-archeion`) so this crate
/// does not pull the registry's session module.
const SESSION_COOKIE_NAME: &str = "kallipai_session";
