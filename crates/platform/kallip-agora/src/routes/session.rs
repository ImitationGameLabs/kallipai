//! The cookie-authenticated session surface: logout and the `/v1/me` profile
//! read, plus the router assembly that mounts the self-service passkey
//! (`routes::passkeys`) and email (`routes::emails`) management routes.
//!
//! This is distinct from `crate::session`, which holds the low-level cookie /
//! session-token primitives (`build_set_cookie`, `read_session_cookie`, ...);
//! this module is the HTTP route layer that consumes them.

use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::{emails, external_identities, passkeys, sessions, users};
use crate::db::map_db_err;
use crate::routes::oauth::ExternalIdentitySummary;
use crate::session::{build_clear_cookie, read_session_cookie};
use crate::state::SharedState;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::header;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use kallip_common::authtoken::TokenHash;
use kallip_common::protocol::ApiError;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
use serde::Serialize;
use time::OffsetDateTime;

/// The cookie-authenticated session surface (no rate limiting): logout, the
/// `/v1/me` profile read, and the self-service passkey (`routes::passkeys`) and
/// email (`routes::emails`) management routes. The send-triggering
/// `POST /me/emails` and the unauthenticated `POST /me/emails/verify` are
/// layered with the per-IP rate limiter at router assembly (see `routes::router`)
/// and merged separately.
pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/auth/logout", post(logout))
        .route("/me", get(me))
        .merge(super::passkeys::router())
        .merge(super::emails::session_router())
}

async fn logout(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    // Require a signed-in user (cookie) so an anonymous token cannot force a
    // cookie clear. The actual session row deletion is best-effort keyed by the
    // presented cookie hash.
    require_user(&principal)?;
    let Some(cookie_value) = read_session_cookie(&headers) else {
        return Err(ApiError::unauthorized("no session"));
    };
    let hash = TokenHash::of(&cookie_value);
    sessions::Entity::delete_by_id(hash.as_bytes().to_vec())
        .exec(&state.db)
        .await
        .map_err(map_db_err)?;
    let clear = build_clear_cookie(&state.session_cfg);
    let mut resp = StatusCode::OK.into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear)
            .map_err(|e| ApiError::internal(format_args!("bad set-cookie: {e}")))?,
    );
    Ok(resp)
}

async fn me(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<MeResponse>, ApiError> {
    let user_id = require_user(&principal)?;
    let user = users::Entity::find_by_id(user_id.to_string())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown user"))?;
    let passkey_count = passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .count(&state.db)
        .await
        .map_err(map_db_err)? as i64;
    // Load linked emails. The primary (if any) drives `primary_email`; the rest
    // surface in `emails`. A user may have none until they add one in settings.
    let owned_emails = emails::Entity::find()
        .filter(emails::Column::AccountId.eq(user_id.to_string()))
        .all(&state.db)
        .await
        .map_err(map_db_err)?;
    let primary_email = owned_emails
        .iter()
        .find(|e| e.is_primary)
        .map(|e| e.address.clone());
    let email_summaries: Vec<EmailSummary> =
        owned_emails.into_iter().map(EmailSummary::from).collect();
    // Load linked external identities (OAuth providers). A user may have none
    // (passkey-only account) until they link one in settings.
    let external_identities = external_identities::Entity::find()
        .filter(external_identities::Column::UserId.eq(user_id.to_string()))
        .all(&state.db)
        .await
        .map_err(map_db_err)?;
    let external_identities: Vec<ExternalIdentitySummary> =
        external_identities.into_iter().map(Into::into).collect();
    Ok(Json(MeResponse {
        user_id: user_id.to_string(),
        display_name: user.display_name,
        username: user.username,
        emails: email_summaries,
        primary_email,
        external_identities,
        created_at: user.created_at,
        passkey_count,
    }))
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MeResponse {
    user_id: String,
    username: String,
    /// All linked addresses; empty until the user adds one in settings.
    emails: Vec<EmailSummary>,
    /// The primary address, if any. `None` when the user has no email.
    primary_email: Option<String>,
    /// Linked OAuth provider identities; empty for a passkey-only account.
    external_identities: Vec<ExternalIdentitySummary>,
    display_name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    passkey_count: i64,
}

/// A linked email address as returned by `/v1/me` and `/v1/me/emails`.
#[derive(Serialize, Clone, Debug)]
pub(crate) struct EmailSummary {
    pub id: String,
    pub address: String,
    pub is_primary: bool,
    /// `None` until the user completes verification.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub verified_at: Option<OffsetDateTime>,
}

impl From<emails::Model> for EmailSummary {
    fn from(m: emails::Model) -> Self {
        Self {
            id: m.id.to_string(),
            address: m.address,
            is_primary: m.is_primary,
            verified_at: m.verified_at,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Handler-level tests for `/v1/me`: the profile shape for a freshly seeded
    //! user (no email) and for a user with a linked primary address.

    use axum::Json;
    use axum::extract::State;

    use super::{MeResponse, me};
    use crate::auth::{AuthPrincipal, Principal};
    use crate::test_helpers::{make_state, seed_email, seed_user};

    /// `/v1/me` returns the signed-in user's profile. A freshly seeded user
    /// has no linked email, so `emails` is empty and `primary_email` is `None`.
    #[tokio::test]
    async fn me_returns_signed_in_user() {
        let state = make_state().await;
        let user_id = seed_user(&state, "alice").await;
        let Json(MeResponse {
            user_id: got,
            username,
            emails,
            primary_email,
            display_name,
            passkey_count,
            ..
        }) = me(
            State(state),
            AuthPrincipal(Principal::User(user_id.clone())),
        )
        .await
        .expect("me ok");
        assert_eq!(got, user_id.to_string());
        assert_eq!(username, "alice");
        assert!(emails.is_empty());
        assert_eq!(primary_email, None);
        assert_eq!(display_name, None);
        assert_eq!(passkey_count, 0);
    }

    /// `/v1/me` reflects a linked, primary email when one is seeded.
    #[tokio::test]
    async fn me_lists_linked_email() {
        let state = make_state().await;
        let user_id = seed_user(&state, "alice").await;
        seed_email(&state, &user_id, "alice@example.test", true, false).await;
        let Json(MeResponse {
            emails,
            primary_email,
            ..
        }) = me(State(state), AuthPrincipal(Principal::User(user_id)))
            .await
            .expect("me ok");
        assert_eq!(emails.len(), 1);
        assert_eq!(emails[0].address, "alice@example.test");
        assert!(emails[0].is_primary);
        assert_eq!(primary_email.as_deref(), Some("alice@example.test"));
    }
}
