//! The management face: authenticated admin CRUD over the registry and the
//! credential store, with a fail-closed change audit.
//!
//! Two credential families are kept apart by route family, not by port.
//! A tagma's platform bearer authorizes the distribution reads (the
//! root family on the management listener) and the forwarding face
//! (resolved against the archeion); the archeion's admin principal
//! authorizes the /admin family. The families share no path, no
//! extractor, and no failure path: an /admin request is authenticated
//! by [`AdminToken`] alone, and a tagma bearer never carries admin
//! rights, so no compound judgment can mix the families (a stolen
//! tagma token says nothing to /admin and vice versa).
//!
//! The admin credential is the platform identity (the archeion), not a
//! second secret the operator must juggle: a request passes either with
//! an admin bearer (a `Principal::Admin` token, the machine/CLI channel)
//! or with the `kallipai_session` cookie of the fixed local-admin
//! account (the browser channel, `verify_session`'s `local_admin`
//! flag). A valid non-admin identity is authenticated but unauthorized
//! (403); an invalid or absent credential is 401; an unreachable
//! archeion fails closed with 503. Security posture versus a shared
//! static secret: equivalent strength (opaque session hashes,
//! HttpOnly + SameSite=Strict cookies, the same admin-account gate as
//! instances) with attribution strengthened -- a shared secret could not
//! say who acted; the session and the audit row can.
//!
//! With no archeion wiring configured the face is closed, not absent:
//! [`AdminAuth::Disabled`] rejects every request with 401 while the
//! distribution and forwarding faces run unaffected.
//!
//! CSRF mirrors the archeion's, the lesche's, and instances' guard (the
//! fourth copy, duplicated on purpose so this crate does not pull the
//! registry's session module): a `SameSite=Strict` cookie plus a custom
//! `X-Requested-With` header the browser cannot synthesize cross-origin
//! without a preflight. Only the cookie channel opens that surface:
//! stateless methods (GET/HEAD/OPTIONS) and bearer-authenticated
//! requests pass through untouched.

pub mod accounts;
pub mod collections;
pub mod groups;
pub mod parking;
pub mod providers;
pub mod sets;
#[cfg(test)]
pub(crate) mod testkit;
pub mod user_routes;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::IntoResponse;
use kallipai_archeion_common::control_plane::{
    ControlPlaneError, EnrollmentLookup, UserIdentity, VerifiedSession,
};
use kallipai_archeion_common::internal_api::{
    EnrollmentLookupRequest, UserSearchRequest, UserSearchResponse, VerifyBearerRequest,
    VerifyBearerResponse, VerifySessionRequest,
};
use kallipai_archeion_common::principal::Principal;
use kallipai_common::auth_header::extract_bearer_token;
use kallipai_common::protocol::{ApiError, Modality};

use crate::state::AppState;

/// What the admin face needs from the platform identity service: verify
/// one bearer token or one session cookie. `Err` = the backend could not
/// be reached or answered outside the contract (the face fails closed on
/// this); `Ok(None)` = the credential is simply not valid.
#[async_trait::async_trait]
pub trait AuthVerifier: Send + Sync {
    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError>;

    async fn verify_session(
        &self,
        cookie: &str,
    ) -> Result<Option<VerifiedSession>, ControlPlaneError>;

    /// The enrollment authority read: which user owns a tagma, plus the
    /// full enrolled set of that user's space (the requested tagma
    /// normally included). `None` = the archeion cannot address the
    /// tagma (unknown, pending, revoked, owner-disabled -- one
    /// collapsed answer, no oracle); `Err` = the backend could not be
    /// reached (the caller fails closed).
    async fn enrollment_lookup(
        &self,
        tagma_id: &str,
    ) -> Result<Option<EnrollmentLookup>, ControlPlaneError>;

    /// The admin member picker's account lookup: an id, username, or
    /// email prefix; the entries carry the minimal identity fields
    /// (the email is a match key only, never an answer field).
    async fn search_accounts(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<UserIdentity>, ControlPlaneError>;
}

/// Per-call timeout: a tiny JSON round trip against a local archeion;
/// 10s is a generous backstop (the lesche's and instances' client).
const INTERNAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Archeion-backed [`AuthVerifier`]: one POST per call to the archeion's
/// `/internal/verify-bearer` / `/internal/verify-session`, guarded by the
/// shared internal secret (the instances twin).
#[derive(Clone)]
pub struct ArcheionVerifier {
    /// Archeion internal root (e.g. `http://127.0.0.1:7100`).
    base_url: String,
    /// Shared secret matching the archeion's provisioned internal token
    /// (the value in KALLIPAI_MODEL_GATEWAY_INTERNAL_TOKEN_FILE).
    internal_token: String,
    http: reqwest::Client,
    /// The TTL cache under the ownership gates: the enrollment answer
    /// per tagma, stale-while-error (see [`EnrollmentCache`]).
    enrollment_cache: EnrollmentCache,
}

impl ArcheionVerifier {
    pub fn new(base_url: String, internal_token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(INTERNAL_TIMEOUT)
            .build()
            .expect("build reqwest client");
        Self {
            enrollment_cache: EnrollmentCache::default(),
            base_url,
            internal_token,
            http,
        }
    }

    /// The raw read behind the enrollment cache: one POST to the
    /// archeion's enrollment authority. `404` reads as `None` (the
    /// contract's collapsed no-answer); anything but 200/404 is a
    /// backend fault.
    async fn probe_enrollment(
        &self,
        tagma_id: &str,
    ) -> Result<Option<EnrollmentLookup>, ControlPlaneError> {
        let response = self
            .http
            .post(format!("{}/internal/enrollment-lookup", self.base_url))
            .bearer_auth(&self.internal_token)
            .json(&EnrollmentLookupRequest {
                tagma_id: kallipai_archeion_common::ids::TagmaId::from(tagma_id.to_owned()),
            })
            .send()
            .await
            .map_err(|e| ControlPlaneError::Backend(e.to_string()))?;
        match response.status().as_u16() {
            200 => response
                .json::<EnrollmentLookup>()
                .await
                .map(Some)
                .map_err(|e| ControlPlaneError::Backend(e.to_string())),
            404 => Ok(None),
            status => Err(ControlPlaneError::Backend(format!(
                "archeion /internal/enrollment-lookup returned HTTP {status}"
            ))),
        }
    }
}

/// One cached answer: what the authority said, and when.
type EnrollmentEntry = (Option<EnrollmentLookup>, std::time::Instant);

/// The ownership gates' answer cache: tagma id to (enrollment answer,
/// fetched at), stale-while-error.
/// The TTL bounds how long an enrollment change takes to bite; a
/// backend outage keeps serving the last answer instead of failing
/// every gate.
#[derive(Clone, Default)]
pub(crate) struct EnrollmentCache {
    entries: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, EnrollmentEntry>>>,
}

/// How long an enrollment answer serves without a re-probe. A constant:
/// bounded staleness, no configuration surface.
const ENROLLMENT_TTL: std::time::Duration = std::time::Duration::from_secs(60);

impl EnrollmentCache {
    /// The cached answer plus whether it is still fresh.
    fn get(&self, id: &str) -> Option<(Option<EnrollmentLookup>, bool)> {
        let entries = self.entries.lock().ok()?;
        let (answer, at) = entries.get(id)?;
        Some((answer.clone(), at.elapsed() < ENROLLMENT_TTL))
    }

    fn store(&self, id: &str, answer: Option<EnrollmentLookup>) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(id.to_owned(), (answer, std::time::Instant::now()));
        }
    }
}

/// The ownership authority: the archeion's enrollment answer for a
/// tagma, read at call time. An unconfigured management face has no
/// authority to ask, which reads as `None` (nothing resolves or
/// mints -- the same closed posture as the face's own routes). `Err`
/// = the backend could not be reached (the caller fails closed).
pub(crate) async fn enrollment_lookup(
    state: &AppState,
    tagma_id: &str,
) -> Result<Option<EnrollmentLookup>, ApiError> {
    match &state.management {
        AdminAuth::Disabled => Ok(None),
        AdminAuth::Platform(verifier) => verifier
            .enrollment_lookup(tagma_id)
            .await
            .map_err(|_| backend_unavailable()),
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

    async fn enrollment_lookup(
        &self,
        tagma_id: &str,
    ) -> Result<Option<EnrollmentLookup>, ControlPlaneError> {
        let cached = self.enrollment_cache.get(tagma_id);
        if let Some((answer, true)) = cached.clone() {
            return Ok(answer);
        }
        match self.probe_enrollment(tagma_id).await {
            Ok(answer) => {
                self.enrollment_cache.store(tagma_id, answer.clone());
                Ok(answer)
            }
            // Stale-while-error: the last answer keeps the gates it
            // guards answering through a backend outage; no cached
            // answer at all fails closed.
            Err(err) => match cached {
                Some((answer, _)) => Ok(answer),
                None => Err(err),
            },
        }
    }
    async fn search_accounts(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<UserIdentity>, ControlPlaneError> {
        let response = self
            .http
            .post(format!("{}/internal/user-search", self.base_url))
            .bearer_auth(&self.internal_token)
            .json(&UserSearchRequest {
                query: query.to_string(),
                limit,
            })
            .send()
            .await
            .map_err(|e| ControlPlaneError::Backend(e.to_string()))?;
        match response.status().as_u16() {
            200 => response
                .json::<UserSearchResponse>()
                .await
                .map(|r| r.users)
                .map_err(|e| ControlPlaneError::Backend(e.to_string())),
            status => Err(ControlPlaneError::Backend(format!(
                "archeion /internal/user-search returned HTTP {status}"
            ))),
        }
    }
}

/// The admin face's authentication source. [`AdminAuth::Disabled`] = no
/// archeion wiring configured: the routes exist (their absence would leak
/// that fact differently) but every request is refused.
#[derive(Clone)]
pub enum AdminAuth {
    Disabled,
    Platform(std::sync::Arc<dyn AuthVerifier>),
}

impl std::fmt::Debug for AdminAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The verifier is a trait object; name the mode, never a credential.
        match self {
            Self::Disabled => f.write_str("Disabled"),
            Self::Platform(_) => f.write_str("Platform(..)"),
        }
    }
}

/// The authenticated actor behind a management request, stashed on the
/// request extensions for the audit trail and the `profiles.owner`
/// attribution.
#[derive(Debug, Clone)]
pub enum Operator {
    /// The fixed local-platform admin account -- attributed as `admin`
    /// regardless of channel (bearer or cookie); the single constant is
    /// this account's ownership/audit spelling in both.
    LocalAdmin,
    /// A signed-in platform user (the session-cookie channel, the user
    /// routes); the attribution is the archeion user id -- the same
    /// spelling the audit rows carry for the consuming account.
    User { account_id: String },
}

impl Operator {
    /// The stable attribution string for audit rows and ownership.
    /// `'static`: the admin side is a constant and the user side owns
    /// its id, so the audit row can take the value by value.
    pub fn as_str(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::LocalAdmin => std::borrow::Cow::Borrowed("admin"),
            Self::User { account_id } => std::borrow::Cow::Owned(account_id.clone()),
        }
    }
}

/// Extractor: authenticate the management request or fail closed with
/// 401/503. Every management route takes this before touching state;
/// there is no unauthenticated management handler by construction.
pub struct AdminToken {
    /// The authenticated operator, consumed by handlers that attribute
    /// the change (audit rows, ownership).
    pub operator: Operator,
}

/// 503 fail-closed: the archeion could not be reached or answered
/// off-contract (the instances fault twin).
pub(crate) fn backend_unavailable() -> ApiError {
    ApiError {
        status: 503,
        message: "the auth backend could not be reached; access is denied".to_owned(),
        dangling: None,
        code: Some("auth_backend_unavailable".to_owned()),
    }
}

impl FromRequestParts<AppState> for AdminToken {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let verifier = match &state.management {
            crate::management::AdminAuth::Disabled => {
                return Err(ApiError::unauthorized("management face is not configured"));
            }
            crate::management::AdminAuth::Platform(verifier) => verifier,
        };
        // Prefer an explicit bearer credential when present (the machine
        // channel); fall back to the session cookie (the browser channel).
        if let Ok(token) = extract_bearer_token(&parts.headers) {
            match verifier.verify_bearer(token).await {
                Ok(Some(Principal::Admin)) => {}
                // A valid non-admin identity: authenticated but not allowed.
                Ok(Some(_)) => {
                    return Err(ApiError::forbidden(
                        "model gateway management is restricted to platform administrators",
                    ));
                }
                Ok(None) => return Err(ApiError::unauthorized("invalid bearer token")),
                // Archeion unreachable or off-contract: fail closed.
                Err(error) => {
                    tracing::warn!(%error, "archeion verify_bearer failed");
                    return Err(backend_unavailable());
                }
            }
        } else if let Some(cookie) = read_session_cookie(&parts.headers) {
            match verifier.verify_session(&cookie).await {
                // The admin login IS the management credential: only the
                // fixed local-admin account's session carries admin rights.
                Ok(Some(session)) if session.local_admin => {}
                Ok(Some(_)) => {
                    return Err(ApiError::forbidden(
                        "model gateway management is restricted to platform administrators",
                    ));
                }
                // Absent / expired / disabled session.
                Ok(None) => {
                    return Err(ApiError::unauthorized_with_code(
                        "sign in as the platform administrator to manage the model gateway",
                        "admin_session_required",
                    ));
                }
                Err(error) => {
                    tracing::warn!(%error, "archeion verify_session failed");
                    return Err(backend_unavailable());
                }
            }
        } else {
            return Err(ApiError::unauthorized(
                "a bearer token or administrator session is required",
            ));
        }
        let operator = Operator::LocalAdmin;
        parts.extensions.insert(operator.clone());
        Ok(Self { operator })
    }
}

/// Extractor: authenticate a user session (the browser channel) for
/// the user-domain routes. Bearer credentials are not a user
/// credential (they are the machine/admin channel). Every signed-in
/// account owns its user domain: the rows attribute to the session's
/// own user id, an admin session's included.
#[derive(Debug)]
pub struct UserSession {
    /// The verified session: the archeion user id and display identity.
    pub session: VerifiedSession,
}

impl FromRequestParts<AppState> for UserSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let verifier = match &state.management {
            crate::management::AdminAuth::Disabled => {
                return Err(ApiError::unauthorized("management face is not configured"));
            }
            crate::management::AdminAuth::Platform(verifier) => verifier,
        };
        let cookie = read_session_cookie(&parts.headers).ok_or_else(|| {
            ApiError::unauthorized_with_code(
                "sign in to use the model gateway",
                "user_session_required",
            )
        })?;
        match verifier.verify_session(&cookie).await {
            Ok(Some(session)) => {
                let operator = Operator::User {
                    account_id: session.user_id.to_string(),
                };
                parts.extensions.insert(operator);
                Ok(Self { session })
            }
            // Absent / expired / disabled session.
            Ok(None) => Err(ApiError::unauthorized_with_code(
                "sign in to use the model gateway",
                "user_session_required",
            )),
            Err(error) => {
                tracing::warn!(%error, "archeion verify_session failed");
                Err(backend_unavailable())
            }
        }
    }
}

// -- the CSRF custom-header guard ----------------------------------------

/// The custom-header CSRF marker name. Lowercase: HTTP headers are
/// case-insensitive, and axum canonicalises on read.
pub const CSRF_HEADER: &str = "x-requested-with";

/// The custom-header CSRF marker value.
pub const CSRF_HEADER_VALUE: &str = "kallipai";

/// Session cookie name. Mirrors the archeion's `kallipai_session`; a stable
/// wire literal kept here (duplicated, as in the lesche and instances, so
/// this crate does not pull the registry's session module).
const SESSION_COOKIE_NAME: &str = "kallipai_session";

/// Reject a cookie-bearing mutating request that lacks the CSRF marker
/// (the browser cannot synthesize the custom header cross-origin without
/// a preflight). Stateless methods and bearer-authenticated requests pass
/// untouched: only the cookie channel opens the CSRF surface.
pub async fn csrf_guard(
    headers: axum::http::HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let is_state_changing = !matches!(
        method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if is_state_changing
        && read_session_cookie(&headers).is_some()
        && extract_bearer_token(&headers).is_err()
    {
        let has_marker = headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case(CSRF_HEADER_VALUE))
            .unwrap_or(false);
        if !has_marker {
            return (axum::http::StatusCode::FORBIDDEN, "missing CSRF marker").into_response();
        }
    }
    next.run(request).await
}

/// Read the session cookie value from a request's `Cookie` header, if
/// present. Mirrors the archeion/lesche/instances helper: multiple
/// `Cookie` headers and multiple pairs within one are both tolerated;
/// the first match wins.
pub(crate) fn read_session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    for header in headers.get_all(axum::http::header::COOKIE) {
        let Ok(s) = header.to_str() else {
            continue;
        };
        for cookie in cookie::Cookie::split_parse(s) {
            let Ok(cookie) = cookie else { continue };
            if cookie.name() == SESSION_COOKIE_NAME {
                return Some(cookie.value().to_string());
            }
        }
    }
    None
}

use axum::Router;
use axum::routing::{delete, get, patch, post, put};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::forward::dialect::WIRE_FAMILIES;
use crate::registry::{self, profile, profile_set, set_member};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

/// The response wrapper for successful deletions. `warning` carries the
/// static risk hint on set deletion (the proxy does
/// not track per-agent binds, so it cannot enumerate what dangles).
#[derive(Serialize)]
pub struct Deleted {
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Deserialize)]
pub struct SecretPut {
    pub upstream_base_url: String,
    pub upstream_api_key: String,
}

/// The wire families the forwarding path can drive (the client stack's
/// dispatch set, canonical in [`crate::forward::dialect`]). Registration
/// rejects anything else: a profile no backend can serve must not enter
/// the registry (the registry module's "admin-face registration
/// concern").
pub(crate) fn validate_family(family: &str) -> Result<(), ApiError> {
    if WIRE_FAMILIES.contains(&family) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "unsupported family {family:?}: the wire families are {WIRE_FAMILIES:?}"
        )))
    }
}

/// Set names ride in URL path segments on the detail routes, so a name
/// containing the segment separator would create an unaddressable set.
pub(crate) fn validate_set_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty() {
        return Err(ApiError::bad_request("set name must not be empty"));
    }
    if name.contains('/') {
        return Err(ApiError::bad_request(
            "set name must not contain '/' (names are path-addressed)",
        ));
    }
    // ':' is the owner-prefix separator (a bare name
    // addresses the platform catalog); a name carrying it would be
    // ambiguous under that addressing form.
    if name.contains(':') {
        return Err(ApiError::bad_request(
            "set name must not contain ':' (':' is the owner-prefix separator)",
        ));
    }
    Ok(())
}

/// Profile ids ride in URL path segments on the parking detail routes, so
/// an id containing the segment separator would create an unaddressable
/// draft (same rule as [`validate_set_name`]).
pub(crate) fn validate_profile_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty() {
        return Err(ApiError::bad_request("profile_id must not be empty"));
    }
    if id.contains('/') {
        return Err(ApiError::bad_request(
            "profile_id must not contain '/' (ids are path-addressed)",
        ));
    }
    Ok(())
}

pub(crate) fn validate_modalities(modalities: &Option<Vec<String>>) -> Result<(), ApiError> {
    match modalities {
        None => Ok(()),
        Some(v)
            if v.iter()
                .all(|m| Modality::ALL.iter().any(|a| a.as_str() == m)) =>
        {
            Ok(())
        }
        Some(_) => Err(ApiError::bad_request(
            "modality entries must be one of the wire modalities (text, image, audio, video)",
        )),
    }
}

pub(crate) fn encode_modalities(modalities: &Option<Vec<String>>) -> Option<String> {
    modalities
        .as_ref()
        .map(|v| serde_json::to_string(v).expect("Vec<String> always serializes"))
}

/// The display mask for an upstream key (the shared mask convention: head,
/// `***`, tail -- identification without exposure).
pub(crate) fn mask_key(key: &str) -> String {
    kallipai_common::authtoken::mask_token(key, kallipai_common::authtoken::TokenKind(""))
}

/// `absent` leaves the field unchanged, `null` clears it, a value
/// writes it. The inner option separates a JSON null from the
/// missing field, which serde would otherwise fold into one `None`.
pub(crate) fn deserialize_explicit_option<'de, D>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

/// The audit JSON for a set: identity plus ordered member ids. `None` =
/// no such set (the handler turns that into the face's 404).
pub(crate) async fn set_state_json<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    name: &str,
) -> Result<Option<serde_json::Value>, ApiError> {
    let Some(set) = profile_set::Entity::find_by_id((owner.to_owned(), name.to_owned()))
        .one(txn)
        .await
        .map_err(db::map_db_err)?
    else {
        return Ok(None);
    };
    let members = set_member::Entity::find()
        .filter(set_member::Column::SetName.eq(name))
        .filter(set_member::Column::Owner.eq(owner))
        .order_by_asc(set_member::Column::Position)
        .all(txn)
        .await
        .map_err(db::map_db_err)?;
    Ok(Some(serde_json::json!({
        "name": set.name,
        "description": set.description,
        "owner": set.owner,
        "members": members.into_iter().map(|m| m.profile_id).collect::<Vec<_>>(),
    })))
}

/// Resolve one profile row in a fixed space. Every caller passes
/// the catalog space; the read is exact. A cross-space duplicate
/// is invisible to a scoped read, which is the point.
pub(crate) async fn resolve_profile<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    profile_id: &str,
) -> Result<profile::Model, ApiError> {
    registry::profile_by_id(txn, owner, profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such profile: {profile_id}")))
}

/// The set twin of [`resolve_profile`].
pub(crate) async fn resolve_set<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    name: &str,
) -> Result<profile_set::Model, ApiError> {
    registry::set_by_id(txn, owner, name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such set: {name}")))
}

/// The management route table: registry CRUD (sets, parking), the provider
/// credential store, and the fail-closed audit that
/// wraps every mutation.
///
/// Invalidation contract: the reads here hit the store, and the
/// registry reads the distribution face serves resolve per request from
/// the same tables -- no process-local cache sits between a management
/// write and the next read. The one cached read is the tagma identity
/// (the archeion's bearer verification), TTL-bounded by the secret
/// domain's staleness bound.
///
/// Mutation shape (every POST/PUT/PATCH/DELETE here): open a transaction, read
/// the current state, apply the change, land the `management_events` row,
/// commit. An audit failure therefore rolls the change back (fail-closed):
/// a management change is the only trace of itself, so the row and the
/// change commit or roll back together.
///
/// Secret contract: provider API keys cross this face inbound only. Every
/// response -- writes included -- carries the mask, never the plaintext,
/// and the audit rows carry the mask too.
pub fn router() -> Router<AppState> {
    Router::new()
        // -- the set face: family-nested create/list, flat item routes ---
        .route(
            "/collections/{name}/sets",
            get(sets::list_collection_sets).post(sets::create_set),
        )
        .route(
            "/sets/{name}",
            get(sets::get_set)
                .patch(sets::update_set)
                .delete(sets::delete_set),
        )
        // -- parking --------------------------------------------------------
        .route(
            "/parking",
            post(parking::create_parked).get(parking::list_parked),
        )
        .route(
            "/parking/{profile_id}",
            get(parking::get_parked)
                .put(parking::update_parked)
                .delete(parking::delete_parked),
        )
        // -- the provider pool -----------------------------------------------
        .route(
            "/providers",
            get(providers::list_providers).post(providers::create_provider),
        )
        .route(
            "/providers/{provider_id}",
            patch(providers::update_provider).delete(providers::delete_provider),
        )
        .route(
            "/providers/{provider_id}/credential",
            put(providers::put_provider_credential).delete(providers::delete_provider_credential),
        )
        // -- the collection face -------------------------------------------------
        .route(
            "/collections",
            post(collections::create_collection).get(collections::list_collections),
        )
        // -- the group face -----------------------------------------------------
        .route(
            "/groups",
            post(groups::create_group).get(groups::list_groups),
        )
        .route(
            "/groups/{group_id}",
            get(groups::get_group)
                .patch(groups::update_group)
                .delete(groups::delete_group),
        )
        .route("/groups/{group_id}/members", post(groups::add_group_member))
        .route(
            "/groups/{group_id}/members/{member_account}",
            delete(groups::remove_group_member),
        )
        .route("/accounts/search", post(accounts::search_accounts))
        .route(
            "/collections/{name}",
            get(collections::get_collection)
                .patch(collections::update_collection)
                .delete(collections::delete_collection),
        )
        .route(
            "/collections/{name}/publications",
            post(collections::publish_collection),
        )
        .route(
            "/collections/{name}/publications/{group}",
            delete(collections::unpublish_collection),
        )
        .route(
            "/collections/{name}/default",
            patch(collections::set_collection_default),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::testkit::*;
    use crate::test_support::TEST_USER_BEARER;
    use axum::extract::FromRequestParts;
    use axum::http::StatusCode;

    // -- pure validation (local legs, no container) -----------------------

    #[test]
    fn validate_family_rejects_non_wire_families() {
        assert!(validate_family("deepseek").is_ok());
        assert!(validate_family("openai-compatible").is_ok());
        assert!(validate_family("anthropic").is_ok());
        assert!(validate_family("openai-responses").is_ok());
        assert!(validate_family("bedrock").is_err());
        assert!(validate_family("").is_err());
    }

    #[test]
    fn set_name_validation_blocks_empty_and_separators() {
        assert!(validate_set_name("gamma").is_ok());
        assert!(validate_set_name("").is_err());
        assert!(validate_set_name("a/b").is_err());
        assert!(validate_set_name("a:b").is_err());
    }

    #[test]
    fn profile_id_validation_blocks_empty_and_separators() {
        assert!(validate_profile_id("gamma").is_ok());
        assert!(validate_profile_id("").is_err());
        assert!(validate_profile_id("a/b").is_err());
    }

    #[test]
    fn mask_key_shows_head_and_tail_only() {
        let masked = mask_key("sk-plaintext-value-123");
        assert!(masked.contains("***"));
        assert!(!masked.contains("plaintext"));
    }

    // -- authentication: the family separation pin -------------------------

    #[tokio::test]
    async fn management_face_rejects_every_wrong_credential() {
        let state = seeded_state().await;
        // No credential at all.
        let (status, _) = send(
            app(state.clone()),
            req("GET", "/admin/collections", None, None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // An unknown bearer.
        let (status, _) = send(
            app(state.clone()),
            req("GET", "/admin/collections", Some("unknown-bearer"), None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // A valid tagma bearer is authenticated but forbidden: the
        // platform identity names a tagma, and management is the
        // administrators' face.
        let (status, _) = send(
            app(state),
            req("GET", "/admin/collections", Some(TEST_TAGMA_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn unconfigured_management_face_is_closed_not_absent() {
        let mut state = seeded_state().await;
        state.management = crate::management::AdminAuth::Disabled;
        let (status, body) = send(
            app(state),
            req("GET", "/admin/collections", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("not configured")
        );
    }

    // -- sets -------------------------------------------------------------

    // -- parking -----------------------------------------------------------

    // -- the provider credentials -------------------------------------------

    // -- parking delete: the cascade and its audit -----------------------

    // -- owner: the set's ownership dimension ------------------------------

    // -- the user platform catalog -----------------------------------------

    // -- the platform-identity channels (bearer + cookie) -------------------

    /// The browser channel: a local-admin session passes the admin family.
    #[tokio::test]
    async fn admin_cookie_passes_the_admin_family() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state),
            cookie_req(
                "GET",
                "/admin/collections",
                Some(TEST_ADMIN_COOKIE),
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// A valid non-admin bearer is authenticated but unauthorized (403),
    /// not unknown (401): the 403 shape belongs to scoped identities.
    #[tokio::test]
    async fn non_admin_bearer_is_forbidden_not_unknown() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state),
            req("GET", "/admin/collections", Some(TEST_USER_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("restricted")
        );
    }

    /// A valid non-admin session is 403 on the cookie channel too.
    #[tokio::test]
    async fn non_admin_cookie_is_forbidden() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state),
            cookie_req(
                "GET",
                "/admin/collections",
                Some(TEST_USER_COOKIE),
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    /// An unknown session cookie is 401 with the browser channel's
    /// sign-in hint.
    #[tokio::test]
    async fn unknown_cookie_is_unauthorized() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state),
            cookie_req(
                "GET",
                "/admin/collections",
                Some("unknown-cookie"),
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "admin_session_required");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("administrator")
        );
    }

    /// A cookie-bearing mutating request without the CSRF marker is 403:
    /// the second CSRF pillar (SameSite=Strict is the first).
    #[tokio::test]
    async fn cookie_mutating_without_csrf_marker_is_forbidden() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state),
            cookie_req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_COOKIE),
                false,
                Some(serde_json::json!({"name": "csrf", "description": "x"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    /// The same request with the marker passes.
    #[tokio::test]
    async fn cookie_mutating_with_csrf_marker_passes() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state),
            cookie_req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_COOKIE),
                true,
                Some(serde_json::json!({"name": "csrf-ok", "description": "x"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "marker-bearing write must pass");
    }

    /// GET is exempt from the marker (stateless): a cookie-carrying read
    /// without the header still passes.
    #[tokio::test]
    async fn cookie_get_without_csrf_marker_passes() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state),
            cookie_req(
                "GET",
                "/admin/collections",
                Some(TEST_ADMIN_COOKIE),
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// A bearer-bearing mutating request is exempt from the marker (the
    /// Authorization header is itself proof of intent).
    #[tokio::test]
    async fn bearer_mutating_without_csrf_marker_passes() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state),
            req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "bearer-csrf", "description": "x"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "bearer writes carry no CSRF duty");
    }

    /// The identity cache carries its weight end to end: the second
    /// request with the same bearer answers without probing the
    /// authority again (the TTL bound's amortization, now with a
    /// probe-counting discriminator).
    #[tokio::test]
    async fn a_cached_identity_answers_without_reprobing_the_authority() {
        let mut state = seeded_state().await;
        let verifier = std::sync::Arc::new(crate::test_support::MockVerifier::enrolled(&[(
            "tagma-under-test",
            "acct-test",
        )]));
        state.management = crate::management::AdminAuth::Platform(verifier.clone());
        for _ in 0..2 {
            let (status, _) = send(
                app(state.clone()),
                req("GET", "/sets", Some(TEST_TAGMA_BEARER), None),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(
            verifier.verify_bearer_calls(),
            1,
            "the second request rides the identity cache"
        );
    }

    /// An unreachable archeion fails closed with 503 on both channels;
    /// the tagma-bearer face joins the same shape.
    #[tokio::test]
    async fn unreachable_auth_backend_fails_closed_503() {
        let mut state = seeded_state().await;
        state.management = crate::management::AdminAuth::Platform(std::sync::Arc::new(
            crate::test_support::MockVerifier::unreachable(),
        ));
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/admin/collections", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "auth_backend_unavailable");
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "GET",
                "/admin/collections",
                Some(TEST_ADMIN_COOKIE),
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        // The tagma-bearer face joins the fail-closed shape: bearer
        // verification probes the same backend, and with no cached
        // identity an unreachable archeion stops the user face too.
        let (status, _) = send(
            app(state),
            req("GET", "/sets", Some(TEST_TAGMA_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    // -- the user-session extractor (the identity pipeline) ------------------

    #[tokio::test]
    async fn user_session_admits_plain_users_and_stamps_the_operator() {
        let state = seeded_state().await;
        let mut parts = parts_with_cookie(TEST_USER_COOKIE);
        let extracted = crate::management::UserSession::from_request_parts(&mut parts, &state)
            .await
            .expect("the plain user session extracts");
        assert_eq!(extracted.session.user_id.to_string(), "user-plain");
        // The extensions carry the attribution for the audit face.
        let operator = parts
            .extensions
            .get::<crate::management::Operator>()
            .expect("the operator rides the extensions");
        assert_eq!(operator.as_str(), "user-plain");
    }

    #[tokio::test]
    async fn user_session_admits_the_admin_session_with_its_own_id() {
        let state = seeded_state().await;
        let mut parts = parts_with_cookie(TEST_ADMIN_COOKIE);
        let extracted = crate::management::UserSession::from_request_parts(&mut parts, &state)
            .await
            .expect("the admin session extracts as its own user identity");
        assert_eq!(extracted.session.user_id.to_string(), "user-testadmin");
        let operator = parts
            .extensions
            .get::<crate::management::Operator>()
            .expect("the operator rides the extensions");
        assert_eq!(operator.as_str(), "user-testadmin");
    }

    #[tokio::test]
    async fn user_session_refuses_the_credentialless() {
        let state = seeded_state().await;
        let (mut parts, _) = axum::http::Request::builder()
            .body(())
            .expect("request builds")
            .into_parts();
        let err = crate::management::UserSession::from_request_parts(&mut parts, &state)
            .await
            .expect_err("no cookie, no user");
        assert_eq!(err.status, 401);
    }

    // -- the user domain -------------------------------------------------

    // -- the reserved audience (everyone) ------------------------------------

    /// The admin session owns a user domain of its own: the same
    /// endpoints, the same owner column, the rows attributed to the
    /// admin account's own user id.
    #[tokio::test]
    async fn admin_user_domain_owns_its_objects() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/groups",
                admin,
                true,
                Some(serde_json::json!({"name": "admins", "members": ["user-testadmin"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let group_id = body["group_id"].as_str().expect("group id").to_owned();
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                admin,
                true,
                Some(serde_json::json!({
                    "name": "admin-bundle",
                    "description": "d"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Both rows read back on the admin's own domain.
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/user/collections", admin, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.to_string().contains("admin-bundle"));
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/user/groups", admin, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.to_string().contains(&group_id));
        // The audit rows attribute to the admin account's user id.
        let rows = audit_rows(&state.db).await;
        assert!(rows.iter().any(|r| r.actor == "user-testadmin"));
        // The plain user's domain shows neither row.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "GET",
                "/user/collections",
                Some(TEST_USER_COOKIE),
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body.to_string().contains("admin-bundle"));
    }

    // -- the admin group face -----------------------------------------------

    // -- the collection-default family ----------------------------------
}
