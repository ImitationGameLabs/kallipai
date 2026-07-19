//! Two-layer auth, principal-scoped: Authentication (who are you) then
//! Authorization (can you do this). Mirrors `kallip-daemon/src/auth.rs`'s shape
//! but resolves to agora principals (admin / user / tagma) rather than the
//! daemon's operator/agent supervisor chain.
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

use crate::db::entity::{sessions, tagma_tokens, tagmata, users};
use crate::db::map_db_err;
use crate::session::read_session_cookie;
use axum::extract::FromRequestParts;
use kallip_agora_common::ids::{TagmaId, UserId};
use kallip_common::auth_header::extract_bearer_token;
use kallip_common::authtoken::TokenHash;
use kallip_common::protocol::ApiError;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;

use crate::state::SharedState;

/// Resolved identity from either an `Authorization: Bearer` header (admin /
/// tagma) or the `kallip_session` cookie (user).
#[derive(Debug, Clone)]
pub enum Principal {
    Admin,
    /// A signed-in user. Reached via the session cookie.
    User(UserId),
    /// A herald's long-lived tagma token. Always via bearer.
    Tagma(TagmaId),
}

/// axum extractor that resolves a request to a [`Principal`].
///
/// Layer 1 (Authentication): if an `Authorization: Bearer` header is present,
/// hash it and match the admin hash (constant-time) first, then look up the
/// tagma token in the durable store. Otherwise, if a `kallip_session` cookie is
/// present, look up the (non-expired) session row and resolve to the owning
/// user. A request with neither is anonymous and rejected with 401.
///
/// The admin path is DB-free (constant-time compare); the tagma path is one
/// indexed `tagma_tokens` lookup (plus an owner-disabled re-check); the cookie
/// path is an indexed `sessions` lookup plus an owner-disabled re-check. A
/// revoked tagma token or a disabled owner takes effect immediately on every
/// request.
#[derive(Debug, Clone)]
pub struct AuthPrincipal(pub Principal);

impl FromRequestParts<SharedState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        // Prefer an explicit bearer credential when present.
        if let Ok(bearer) = extract_bearer_token(&parts.headers) {
            return resolve_bearer(state, bearer).await.map(AuthPrincipal);
        }
        // Otherwise fall back to the session cookie.
        if let Some(cookie_value) = read_session_cookie(&parts.headers) {
            return resolve_session(state, &cookie_value)
                .await
                .map(AuthPrincipal);
        }
        Err(ApiError::unauthorized("authentication required"))
    }
}

/// Resolve a bearer token to admin (constant-time) or a tagma. A revoked tagma
/// token or a token whose owner is disabled never authenticates.
async fn resolve_bearer(state: &SharedState, token: &str) -> Result<Principal, ApiError> {
    let hash = TokenHash::of(token);
    if state.admin_token_hash.ct_eq(&hash) {
        return Ok(Principal::Admin);
    }
    let row = tagma_tokens::Entity::find()
        .filter(tagma_tokens::Column::TokenHash.eq(hash.as_bytes().to_vec()))
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    if let Some(row) = row {
        // A revoked tagma token never authenticates.
        if row.revoked_at.is_some() {
            return Err(ApiError::unauthorized("invalid token"));
        }
        // A token owned by a disabled account never authenticates either:
        // disabling a user must also cut off their herald. The tagma's owner is
        // loaded by id; a missing tagma or owner (shouldn't happen given the FK)
        // is treated as not-disabled, never as a silent allow.
        let owner_disabled = match tagmata::Entity::find_by_id(row.tagma_id.clone())
            .one(&state.db)
            .await
            .map_err(map_db_err)?
        {
            Some(tagma) => match users::Entity::find_by_id(tagma.owner_user_id)
                .one(&state.db)
                .await
                .map_err(map_db_err)?
            {
                Some(owner) => owner.disabled_at.is_some(),
                None => false,
            },
            None => false,
        };
        if owner_disabled {
            return Err(ApiError::unauthorized("invalid token"));
        }
        let tagma_id = TagmaId::from(row.tagma_id);
        return Ok(Principal::Tagma(tagma_id));
    }
    Err(ApiError::unauthorized("invalid token"))
}

/// Resolve a session cookie value to the owning user. The opaque cookie value
/// is hashed; the `sessions` row must exist and not have expired, and the owning
/// user must not be disabled. The disabled-user check here is what makes an
/// admin disable take effect immediately on every authenticated request, not
/// only at the next login.
async fn resolve_session(state: &SharedState, cookie_value: &str) -> Result<Principal, ApiError> {
    let hash = TokenHash::of(cookie_value);
    let row = sessions::Entity::find()
        .filter(sessions::Column::TokenHash.eq(hash.as_bytes().to_vec()))
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    let Some(row) = row else {
        return Err(ApiError::unauthorized("invalid session"));
    };
    if row.expires_at <= OffsetDateTime::now_utc() {
        return Err(ApiError::unauthorized("session expired"));
    }
    let user = users::Entity::find_by_id(row.user_id.clone())
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    let Some(user) = user else {
        return Err(ApiError::unauthorized("invalid session"));
    };
    if user.disabled_at.is_some() {
        return Err(ApiError::unauthorized("invalid session"));
    }
    Ok(Principal::User(UserId::from(user.id)))
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

/// The authenticated user (for user-scoped endpoints). Reached via the session
/// cookie.
pub fn require_user(principal: &Principal) -> Result<&UserId, ApiError> {
    match principal {
        Principal::User(id) => Ok(id),
        _ => Err(ApiError::forbidden("user access required")),
    }
}

/// The authenticated tagma (for the herald routes). Reached via tagma bearer
/// only; a cookie-sourced principal never resolves to `Tagma`, so this is the
/// deputy guard for herald routes.
pub fn require_tagma(principal: &Principal) -> Result<&TagmaId, ApiError> {
    match principal {
        Principal::Tagma(id) => Ok(id),
        _ => Err(ApiError::forbidden("tagma access required")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::sessions;
    use crate::test_helpers::{make_state, seed_user};
    use crate::token::SESSION;
    use kallip_common::authtoken::MintedToken;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    use time::{Duration, OffsetDateTime};

    /// A disabled user's already-issued session is rejected on the very next
    /// resolve: the hot-path disabled check is what makes "disable" take effect
    /// immediately, not just at the next login.
    #[tokio::test]
    async fn resolve_session_rejects_disabled_user() {
        let state = make_state(std::time::Duration::from_secs(2)).await;
        let user_id = seed_user(&state, "frozen", "frozen@example.test").await;
        let session = MintedToken::generate(SESSION);
        let now = OffsetDateTime::now_utc();
        sessions::ActiveModel {
            token_hash: Set(session.hash().as_bytes().to_vec()),
            user_id: Set(user_id.to_string()),
            created_at: Set(now),
            expires_at: Set(now + Duration::hours(1)),
        }
        .insert(&state.db)
        .await
        .expect("insert session");

        // The live session resolves to its owner.
        assert!(matches!(
            resolve_session(&state, session.secret()).await,
            Ok(Principal::User(_))
        ));

        // Disable the owner; the same session is now rejected.
        let row = users::Entity::find_by_id(user_id.to_string())
            .one(&state.db)
            .await
            .expect("load user")
            .expect("user present");
        let mut am: users::ActiveModel = row.into();
        am.disabled_at = Set(Some(now));
        am.update(&state.db).await.expect("disable user");
        assert!(resolve_session(&state, session.secret()).await.is_err());
    }
}
