//! Request authentication for the relay. The `AuthPrincipal` extractor resolves
//! a request to a [`Principal`] by delegating credential verification to the
//! registry through the
//! [`ControlPlane`](kallip_agora_common::control_plane::ControlPlane) trait.
//! The deputy guard (cookie -> User, bearer -> Tagma/Admin) is preserved by
//! construction: `verify_session` can only return a user, `verify_bearer` only
//! an admin or tagma.

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use kallip_agora_common::control_plane::ControlPlaneError;
use kallip_agora_common::principal::Principal;
use kallip_common::auth_header::extract_bearer_token;
use kallip_common::protocol::ApiError;

use crate::state::SharedConvState;

/// Session cookie name. Mirrors the registry's `kallip_session`; a stable wire
/// literal kept here (duplicated from `kallip-agora`) so this crate does not
/// pull the registry's session module.
const SESSION_COOKIE_NAME: &str = "kallip_session";

/// axum extractor that resolves a request to a [`Principal`] via the registry.
/// A bearer credential is preferred when present; otherwise the session cookie.
/// A request with neither is anonymous and rejected with 401.
#[derive(Debug, Clone)]
pub struct AuthPrincipal(pub Principal);

impl FromRequestParts<SharedConvState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedConvState,
    ) -> Result<Self, Self::Rejection> {
        // Prefer an explicit bearer credential when present.
        if let Ok(bearer) = extract_bearer_token(&parts.headers) {
            let principal = state
                .control
                .verify_bearer(bearer)
                .await
                .map_err(control_to_internal)?
                .ok_or_else(|| ApiError::unauthorized("invalid token"))?;
            return Ok(AuthPrincipal(principal));
        }
        // Otherwise fall back to the session cookie.
        if let Some(cookie_value) = read_session_cookie(&parts.headers) {
            let user = state
                .control
                .verify_session(&cookie_value)
                .await
                .map_err(control_to_internal)?
                .ok_or_else(|| ApiError::unauthorized("invalid session"))?;
            return Ok(AuthPrincipal(Principal::User(user)));
        }
        Err(ApiError::unauthorized("authentication required"))
    }
}

fn control_to_internal(e: ControlPlaneError) -> ApiError {
    ApiError::internal(format_args!("registry error: {e}"))
}

/// Read the session cookie value from a request's `Cookie` header, if present.
/// Mirrors the registry's helper: multiple `Cookie` headers and multiple
/// `name=value` pairs within one are both tolerated; first match wins.
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

// Re-export the authorization helpers the relay's handlers use. (`require_admin`
// is not needed here: the relay has no admin routes.)
pub use kallip_agora_common::principal::{require_tagma, require_user};
