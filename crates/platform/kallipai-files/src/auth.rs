//! Request authentication for the files service, mirroring the relay's
//! shape: an [`AuthPrincipal`] extractor resolves a request to a
//! [`Principal`] by delegating credential verification to the control-plane
//! client. A bearer credential is preferred; the `kallipai_session` cookie is
//! the fallback; neither means anonymous -> 401. Verification is per request
//! with no cache (the deliberate control-plane design): revocation latency
//! is one round trip, not a TTL.
//!
//! Failure mapping: the two control-plane error kinds answer apart, because
//! the caller's next move differs. [`ControlPlaneError::Backend`] (the
//! registry could not be reached, answered a status this client does not
//! read as part of the contract, or sent a body that will not decode) may
//! clear on a retry: it maps to 503 with code `auth_backend_unavailable`.
//! [`ControlPlaneError::Contract`] (a `200` naming a user or tagma without
//! the record behind it) never clears on a retry: it maps to 500 with code
//! `auth_contract_violation`, and an upgrade answers it. Either way a
//! registry that gives no usable answer never opens the door. That is
//! fail-closed, the degrade default; the `KALLIPAI_FILES_DEGRADE=soft`
//! switch narrows which decisions degrade (`crate::api::tagma_facts` and
//! `crate::api::send` carry the branch) and does not weaken verification.

use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use kallipai_archeion_common::control_plane::{
    ControlPlane, ControlPlaneError, CredentialScope, EnrollmentLookup, TagmaProfile, UserIdentity,
    VerifiedBearer, VerifiedSession,
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
use kallipai_common::telemetry;
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
    http: telemetry::HttpClient,
}

impl FilesControlPlane {
    /// Build the client. `base_url` is the archeion's internal root; `token`
    /// is the shared secret.
    pub fn new(base_url: String, token: String) -> Self {
        let http = telemetry::http_client(reqwest::Client::builder().timeout(INTERNAL_TIMEOUT))
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

    async fn verify_bearer(
        &self,
        token: &str,
    ) -> Result<Option<VerifiedBearer>, ControlPlaneError> {
        let resp: Option<VerifyBearerResponse> = self
            .post(
                "/internal/verify-bearer",
                &VerifyBearerRequest {
                    token: token.to_string(),
                },
            )
            .await?;
        // The wire body is the resolved identity plus its credential; the
        // `TryFrom` in internal_api is the single mapping site, and it refuses a
        // user or tagma bearer that arrives with no record behind it.
        resp.map(VerifiedBearer::try_from)
            .transpose()
            .map_err(|e| ControlPlaneError::Contract(e.to_string()))
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

/// The axum extractor resolving a request to a [`Principal`] and the credential
/// record it presented, via the registry. Bearer preferred, cookie fallback,
/// neither -> 401. `None` from a verify call (revoked/unknown) -> 401:
/// revocation takes effect on the very next request.
#[derive(Debug, Clone)]
pub struct AuthPrincipal {
    pub principal: Principal,
    /// `None` when no stored record carried the call: the session cookie is a
    /// potency of the user's identity, not a grant a record can narrow.
    pub credential: Option<CredentialScope>,
}

impl AuthPrincipal {
    /// A caller with no credential record behind it (a session cookie, a
    /// handler test).
    pub fn unscoped(principal: Principal) -> Self {
        Self {
            principal,
            credential: None,
        }
    }
}

/// Does this call carry `perm`? A caller with no record passes; a credential
/// passes only the strings its set names, so an emptied set authenticates and
/// does nothing.
pub fn require_perm(auth: &AuthPrincipal, perm: &str) -> Result<(), ApiError> {
    kallipai_archeion_common::perm::require(auth.credential.as_ref(), perm)
}

impl FromRequestParts<AppState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Prefer an explicit bearer credential when present.
        if let Ok(bearer) = extract_bearer_token(&parts.headers) {
            let verified = state
                .control
                .verify_bearer(bearer)
                .await
                .map_err(control_failure)?
                .ok_or_else(|| ApiError::unauthorized("invalid token"))?;
            return Ok(AuthPrincipal {
                principal: verified.principal,
                credential: verified.credential,
            });
        }
        // Otherwise fall back to the session cookie.
        if let Some(cookie_value) = read_session_cookie(&parts.headers) {
            let user = state
                .control
                .verify_session(&cookie_value)
                .await
                .map_err(control_failure)?
                .ok_or_else(|| ApiError::unauthorized("invalid session"))?;
            return Ok(AuthPrincipal::unscoped(Principal::User(user.user_id)));
        }
        Err(ApiError::unauthorized("authentication required"))
    }
}

/// Control-plane failures never open the door: the registry is a hard
/// dependency of every authenticated decision. The two kinds answer apart
/// because the caller's next move differs (see the module docs): `Backend`
/// is a retryable `503`, `Contract` a permanent `500` that no retry clears.
pub(crate) fn control_failure(e: ControlPlaneError) -> ApiError {
    match e {
        ControlPlaneError::Backend(detail) => {
            tracing::warn!(%detail, "registry backend failure");
            ApiError::unavailable_with_code(
                "the auth backend could not be verified; access is denied",
                "auth_backend_unavailable",
            )
        }
        ControlPlaneError::Contract(detail) => {
            tracing::error!(%detail, "registry answered outside its contract");
            ApiError::internal_with_code(
                "the auth backend answered outside its contract; the services are running different versions",
                "auth_contract_violation",
            )
        }
    }
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
