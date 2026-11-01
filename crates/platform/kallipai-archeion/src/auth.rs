//! Two-layer auth, principal-scoped: Authentication (who are you) then
//! Authorization (can you do this). Mirrors `kallipai-tagma/src/auth.rs`'s shape
//! but resolves to archeion principals (admin / user / tagma) rather than the
//! tagma's operator/agent supervisor chain.
//!
//! # One extractor, two decisions
//!
//! [`AuthPrincipal`] is the only extractor: it accepts a bearer or the session
//! cookie and resolves to a [`Principal`] plus, when a stored record carried
//! the call, that record's [`CredentialScope`].
//!
//! Two functions decide what such a principal may do. [`require_perm`] asks
//! whether the call carries one permission string; the routes that act on the
//! caller's own data gate themselves that way. [`require_session`] asks for the
//! cookie origin itself and backs the routes that mint or destroy credentials,
//! change the account's sign-in methods, or mint an enrollment code -- a token
//! that could reach those would mint its own successor.
//!
//! # Deputy guard
//!
//! A `Tagma` principal still arrives only via an `sk-` bearer, and a `User`
//! either via the cookie (`credential: None`) or via a personal access token
//! (`credential: Some`). The record is what distinguishes the two user origins;
//! it lives beside the principal rather than on it, so an identity stays the
//! same thing across a token reset.
//!
//! A bearer never resolves to a `User` or a `Tagma` with no record: only the
//! admin bearer is a bare secret, and the wire decode refuses every other
//! recordless shape (`VerifiedBearer`'s `TryFrom`), so a personal access token
//! cannot arrive here as the whole identity.

use crate::control_plane::DbControlPlane;
use crate::session::read_session_cookie;
use axum::extract::FromRequestParts;
use kallipai_archeion_common::control_plane::{ControlPlane, CredentialScope};
use kallipai_archeion_common::ids::UserId;
use kallipai_common::auth_header::extract_bearer_token;
use kallipai_common::protocol::ApiError;

use crate::state::SharedState;

// `Principal` and the `require_*` authorization helpers live in the shared
// `kallipai-archeion-common` crate so the registry and the data-plane services
// enforce the same deputy guard. Re-export them here so existing `crate::auth::Principal` /
// `super::require_*` references keep resolving.
pub use kallipai_archeion_common::principal::{Principal, require_admin, require_user};

/// A resolved identity plus the credential that carried it.
#[derive(Debug, Clone)]
pub struct AuthPrincipal {
    pub principal: Principal,
    /// `None` when no stored record carried the call: the session cookie and
    /// the admin bearer are potencies of the identity itself, not grants a
    /// record can narrow.
    pub credential: Option<CredentialScope>,
}

impl AuthPrincipal {
    /// A caller with no credential record behind it (the session cookie, the
    /// admin bearer, a handler test).
    pub fn unscoped(principal: Principal) -> Self {
        Self {
            principal,
            credential: None,
        }
    }

    /// A caller holding a record, bounded by that record's stored set.
    pub fn scoped(principal: Principal, credential: CredentialScope) -> Self {
        Self {
            principal,
            credential: Some(credential),
        }
    }
}

/// Does this call carry `perm`?
///
/// A caller with no record passes (a session cookie or the admin bearer holds
/// the whole identity's rights). A credential passes only when its set names
/// the string, so an emptied set authenticates and does nothing -- a state the
/// owner can create on purpose, and not the same thing as a revoked identity.
pub fn require_perm(auth: &AuthPrincipal, perm: &str) -> Result<(), ApiError> {
    kallipai_archeion_common::perm::require(auth.credential.as_ref(), perm)
}

fn control_plane(state: &SharedState) -> DbControlPlane {
    DbControlPlane::new(
        state.db.clone(),
        // Snapshot under the read lock: a token rotation swaps the value
        // behind this lock, and the next request picks up the new hash.
        state
            .admin_token_hash
            .read()
            .expect("admin token lock")
            .clone(),
    )
}

impl FromRequestParts<SharedState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        // Credential verification is delegated to the shared DB-backed
        // `ControlPlane` impl (the same one the data-plane services consume),
        // so the registry and the services enforce identical auth semantics.
        let control = control_plane(state);
        // An explicit bearer credential wins when present.
        if let Ok(bearer) = extract_bearer_token(&parts.headers) {
            let verified = control
                .verify_bearer(bearer)
                .await
                .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
                .ok_or_else(|| ApiError::unauthorized("invalid token"))?;
            return Ok(match verified.credential {
                Some(credential) => Self::scoped(verified.principal, credential),
                None => Self::unscoped(verified.principal),
            });
        }
        // Otherwise fall back to the session cookie.
        if let Some(cookie_value) = read_session_cookie(&parts.headers) {
            let user = control
                .verify_session(&cookie_value)
                .await
                .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
                .ok_or_else(|| ApiError::unauthorized("invalid session"))?;
            // The archeion's own routes need only the user id; the verified
            // session's username/display_name are consumed by the data planes
            // over the /internal/verify-session HTTP surface, not here.
            return Ok(Self::unscoped(Principal::User(user.user_id)));
        }
        Err(ApiError::unauthorized("authentication required"))
    }
}

/// The signed-in user, and only the user themselves.
///
/// Runs after authentication so a bearer is first recognized and then refused
/// with a 403 that names the rule. A 401 here would say "this token is not a
/// token", which is false, and would send a script hunting for a rotated
/// secret instead of reading the message.
pub fn require_session(auth: &AuthPrincipal) -> Result<&UserId, ApiError> {
    match (&auth.principal, &auth.credential) {
        (Principal::User(id), None) => Ok(id),
        (Principal::User(_), Some(_)) => Err(ApiError::forbidden(
            "a personal access token cannot change the account or its credentials",
        )),
        _ => Err(ApiError::forbidden("a signed-in user is required")),
    }
}
