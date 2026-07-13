//! Two-layer auth, principal-scoped: Authentication (who are you) then
//! Authorization (can you do this). Mirrors `kallip-daemon/src/auth.rs`'s shape
//! but resolves to agora principals (admin / user / team) rather than the
//! daemon's operator/agent supervisor chain.

use axum::extract::FromRequestParts;
use kallip_agora_common::ids::{TeamId, UserId};
use kallip_common::auth_header::extract_bearer_token;
use kallip_common::authtoken::TokenHash;
use kallip_common::protocol::ApiError;

use crate::state::SharedState;

/// Resolved identity from the Authorization header.
#[derive(Debug, Clone)]
pub enum Principal {
    Admin,
    User(UserId),
    Team(TeamId),
}

/// axum extractor that resolves a Bearer token to a [`Principal`].
///
/// Layer 1 (Authentication): parse `Authorization: Bearer <token>`, hash it,
/// and match against the admin hash (constant-time) first, then the
/// access-token and team-token hash indexes.
#[derive(Debug, Clone)]
pub struct AuthPrincipal(pub Principal);

impl FromRequestParts<SharedState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer_token(&parts.headers)?;
        let hash = TokenHash::of(token);

        if state.admin_token_hash.ct_eq(&hash) {
            return Ok(AuthPrincipal(Principal::Admin));
        }

        let registry = state.read()?;
        if let Some(user_id) = registry.access_tokens.get(&hash) {
            return Ok(AuthPrincipal(Principal::User(user_id.clone())));
        }
        if let Some(team_id) = registry.team_tokens.get(&hash) {
            return Ok(AuthPrincipal(Principal::Team(team_id.clone())));
        }

        Err(ApiError::unauthorized("invalid token"))
    }
}

// ---------------------------------------------------------------------------
// Layer 2: Authorization helpers
// ---------------------------------------------------------------------------

/// Only the admin may proceed (provisioning endpoints).
pub fn require_admin(principal: &Principal) -> Result<(), ApiError> {
    match principal {
        Principal::Admin => Ok(()),
        _ => Err(ApiError::forbidden("admin access required")),
    }
}

/// The authenticated user (for user-scoped endpoints).
pub fn require_user(principal: &Principal) -> Result<&UserId, ApiError> {
    match principal {
        Principal::User(id) => Ok(id),
        _ => Err(ApiError::forbidden("user access required")),
    }
}

/// The authenticated team (for the herald-tunnel route).
pub fn require_team(principal: &Principal) -> Result<&TeamId, ApiError> {
    match principal {
        Principal::Team(id) => Ok(id),
        _ => Err(ApiError::forbidden("team access required")),
    }
}
