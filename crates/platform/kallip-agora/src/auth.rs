//! Two-layer auth, principal-scoped: Authentication (who are you) then
//! Authorization (can you do this). Mirrors `kallip-tagma/src/auth.rs`'s shape
//! but resolves to agora principals (admin / user / tagma) rather than the
//! tagma's operator/agent supervisor chain.
//!
//! # Origin / deputy guard
//!
//! A `User` principal is reached ONLY via the `kallip_session` cookie; a
//! `Tagma` principal is reached ONLY via an `sk-tagma-` bearer; the admin ONLY
//! via the `sk-admin-` bearer. So the deputy threats a multi-origin design
//! would face — a session cookie authenticating a herald route, a tagma bearer
//! reaching `/v1/me` — are already impossible by construction: `require_tagma`
//! never sees a `User` and `require_user` never sees a `Tagma`. An
//! `Origin { Bearer, Cookie }` tag on each principal is therefore not yet
//! needed; it becomes relevant only when personal access tokens introduce a
//! second `User` origin that genuinely needs distinguishing, so adding a
//! single-valued enum variant now would be premature.

use crate::control_plane::DbControlPlane;
use crate::session::read_session_cookie;
use axum::extract::FromRequestParts;
use kallip_agora_common::control_plane::ControlPlane;
use kallip_common::auth_header::extract_bearer_token;
use kallip_common::protocol::ApiError;

use crate::state::SharedState;

// `Principal` and the `require_*` authorization helpers live in the shared
// `kallip-agora-common` crate so both the registry and the relay enforce the
// same deputy guard. Re-export them here so existing `crate::auth::Principal` /
// `super::require_*` references keep resolving.
pub use kallip_agora_common::principal::{Principal, require_admin, require_user};

/// axum extractor that resolves a request to a [`Principal`].
///
/// Layer 1 (Authentication): if an `Authorization: Bearer` header is present,
/// hash it and match the admin hash (constant-time) first, then look up the
/// tagma token in the durable store. Otherwise, if a `kallip_session` cookie is
/// present, look up the (non-expired) session row and resolve to the owning
/// user. A request with neither is anonymous and rejected with 401.
///
/// The admin path is DB-free (constant-time compare); the tagma path is one
/// indexed `tagma_tokens` lookup plus the owning tagma row (a revoked tagma or
/// a disabled owner re-check); the cookie path is an indexed `sessions` lookup
/// plus an owner-disabled re-check. A revoked tagma or a disabled owner takes
/// effect immediately on every request.
#[derive(Debug, Clone)]
pub struct AuthPrincipal(pub Principal);

impl FromRequestParts<SharedState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        // Credential verification is delegated to the shared DB-backed
        // `ControlPlane` impl (the same one the data-plane relay consumes), so
        // the registry and the relay enforce identical auth semantics.
        let control = DbControlPlane::new(state.db.clone(), state.admin_token_hash.clone());
        // Prefer an explicit bearer credential when present.
        if let Ok(bearer) = extract_bearer_token(&parts.headers) {
            let principal = control
                .verify_bearer(bearer)
                .await
                .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
                .ok_or_else(|| ApiError::unauthorized("invalid token"))?;
            return Ok(AuthPrincipal(principal));
        }
        // Otherwise fall back to the session cookie.
        if let Some(cookie_value) = read_session_cookie(&parts.headers) {
            let user = control
                .verify_session(&cookie_value)
                .await
                .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
                .ok_or_else(|| ApiError::unauthorized("invalid session"))?;
            return Ok(AuthPrincipal(Principal::User(user)));
        }
        Err(ApiError::unauthorized("authentication required"))
    }
}

// ---------------------------------------------------------------------------
// Layer 2: Authorization helpers (`require_admin` / `require_user` /
// `require_tagma`) live in `kallip-agora-common::principal` and are
// re-exported above. The disabled-user / revoked-tagma auth semantics are
// tested at the `DbControlPlane` impl (`crate::control_plane`).
// ---------------------------------------------------------------------------
