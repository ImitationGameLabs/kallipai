//! Request authentication for the relay. The `AuthPrincipal` extractor resolves
//! a request to a [`Principal`] by delegating credential verification to the
//! registry through the
//! [`ControlPlane`](kallipai_archeion_common::control_plane::ControlPlane) trait.
//! A third origin exists: a personal access token resolves through
//! `verify_bearer` to a `User` plus that token's permission set. This service
//! judges identity and room membership, not permission strings, so it refuses
//! that origin outright rather than reading it as the whole user.

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use kallipai_archeion_common::control_plane::ControlPlaneError;
use kallipai_archeion_common::principal::Principal;
use kallipai_common::auth_header::extract_bearer_token;
use kallipai_common::protocol::ApiError;

use crate::state::SharedConvState;

/// Session cookie name. Mirrors the registry's `kallipai_session`; a stable wire
/// literal kept here (duplicated from `kallipai-archeion`) so this crate does not
/// pull the registry's session module.
const SESSION_COOKIE_NAME: &str = "kallipai_session";

/// axum extractor that resolves a request to a [`Principal`] via the registry.
/// A bearer credential is preferred when present; otherwise the session cookie.
/// A request with neither is anonymous and rejected with 401.
#[derive(Debug, Clone)]
pub struct AuthPrincipal(pub Principal);

/// The authoritative display identity for a cookie-authenticated user, stashed
/// on the request by [`AuthPrincipal`]'s extractor. `None` for bearer (tagma /
/// admin) requests. Consumed by handlers that stamp room identity so a human
/// sender's name/handle is relay-resolved, not client-supplied.
#[derive(Debug, Clone)]
pub struct UserDisplay {
    pub username: String,
    /// Read by the roster path (a follow-up); the message-stamp path builds the
    /// stable handle from `username` only.
    #[allow(dead_code)]
    pub display_name: Option<String>,
}

/// Optional extractor for [`UserDisplay`] (present only for cookie/user
/// requests). Handlers read it to render an authoritative human identity;
/// `None` means the request is bearer-authenticated (no user display).
#[derive(Debug, Clone, Default)]
pub struct OptUserDisplay(pub Option<UserDisplay>);

impl FromRequestParts<SharedConvState> for OptUserDisplay {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &SharedConvState,
    ) -> Result<Self, Self::Rejection> {
        Ok(OptUserDisplay(
            parts.extensions.get::<UserDisplay>().cloned(),
        ))
    }
}

impl FromRequestParts<SharedConvState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedConvState,
    ) -> Result<Self, Self::Rejection> {
        // Prefer an explicit bearer credential when present.
        if let Ok(bearer) = extract_bearer_token(&parts.headers) {
            let verified = state
                .control
                .verify_bearer(bearer)
                .await
                .map_err(control_failure)?
                .ok_or_else(|| ApiError::unauthorized("invalid token"))?;
            // A tagma's own credential carries a record too; only a user behind
            // one is a personal access token (see `is_personal_token`).
            if verified.is_personal_token() {
                return Err(ApiError::forbidden(
                    "a personal access token is not accepted here",
                ));
            }
            return Ok(AuthPrincipal(verified.principal));
        }
        // Otherwise fall back to the session cookie.
        if let Some(cookie_value) = read_session_cookie(&parts.headers) {
            let user = state
                .control
                .verify_session(&cookie_value)
                .await
                .map_err(control_failure)?
                .ok_or_else(|| ApiError::unauthorized("invalid session"))?;
            // Stash the authoritative display identity on the request so handlers
            // that stamp room identity (the rooms send path) can use it without a
            // second verify_session call -- the relay is the sole source of room
            // identity, retiring the advisory app-supplied `Participant.handle`.
            parts.extensions.insert(UserDisplay {
                username: user.username.clone(),
                display_name: user.display_name.clone(),
            });
            return Ok(AuthPrincipal(Principal::User(user.user_id)));
        }
        Err(ApiError::unauthorized("authentication required"))
    }
}

/// Control-plane failures never authenticate anyone. The two kinds answer
/// apart because the caller's next move differs: `Backend` is a retryable
/// `503`, `Contract` a permanent `500` that only an upgrade clears.
fn control_failure(e: ControlPlaneError) -> ApiError {
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
pub use kallipai_archeion_common::principal::{require_tagma, require_user};

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use kallipai_archeion_common::bytes::Ed25519PublicKey;
    use kallipai_archeion_common::ids::{TagmaId, UserId};

    use crate::test_support::make_state;

    fn parts_with_bearer(token: &str) -> axum::http::request::Parts {
        Request::builder()
            .uri("/rooms")
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .expect("build request")
            .into_parts()
            .0
    }

    /// A personal access token resolves to a user, and this service takes no
    /// credentials: the extractor refuses it with 403 rather than reading it as
    /// the whole user (its room routes judge membership, not permission strings).
    #[tokio::test]
    async fn a_personal_access_token_is_refused_outright() {
        let (state, control) = make_state();
        control.seed_personal_token("sk-pat-mock", UserId::random());

        let mut parts = parts_with_bearer("sk-pat-mock");
        let err = AuthPrincipal::from_request_parts(&mut parts, &state)
            .await
            .expect_err("a personal token is not accepted here");
        assert_eq!(err.status, 403);
        assert!(err.message.contains("personal access token"), "{err:?}");
    }

    /// The two control-plane failures answer with different statuses: a
    /// backend outage may clear on a retry, a contract violation never does.
    #[test]
    fn a_backend_failure_is_retryable_and_a_contract_violation_is_not() {
        let backend = control_failure(ControlPlaneError::Backend("down".to_owned()));
        assert_eq!(backend.status, 503);
        assert_eq!(backend.code.as_deref(), Some("auth_backend_unavailable"));

        let contract = control_failure(ControlPlaneError::Contract("no record".to_owned()));
        assert_eq!(contract.status, 500);
        assert_eq!(contract.code.as_deref(), Some("auth_contract_violation"));
    }

    /// A tagma's own credential is a record as well, and it is the tagma: the
    /// refusal keys on the user origin, never on "carries a record".
    #[tokio::test]
    async fn a_tagma_credential_still_resolves_to_its_tagma() {
        let (state, control) = make_state();
        let tagma = TagmaId::from("t1".to_string());
        control.enroll_tagma(
            &tagma,
            UserId::random(),
            Ed25519PublicKey(vec![0u8; 32]),
            "sk-tagma-mock",
        );

        let mut parts = parts_with_bearer("sk-tagma-mock");
        let AuthPrincipal(principal) = AuthPrincipal::from_request_parts(&mut parts, &state)
            .await
            .expect("a tagma credential is how the tagma itself talks");
        assert!(matches!(principal, Principal::Tagma(id) if id == tagma));
    }
}
