//! Public profile reads: unauthenticated, minimal-disclosure identity cards used
//! to deep-link a user or tagma profile from a room sender header (or anywhere a
//! handle/id is shown). These are a deliberate, bounded extension of the
//! existence-oracle surface -- see the policy notes below.
//!
//! - `GET /v1/users/{username}` -- `{ username, display_name, created_at }`.
//! - `GET /v1/tagmata/{id}/profile` -- `{ tagma_id, label, created_at,
//!    owner_username, owner_display_name }`.
//!
//! Both are public (no `AuthPrincipal`), per-IP rate-limited at the mount site,
//! and return a single fixed `404 "unknown ..."` for any missing / invalid /
//! disabled / pending / revoked subject -- no reason leak. Disclosure is the
//! minimal set needed to render a profile card: never `email`, `user_id`,
//! `pinned_public_key`, `owner_user_id`, or the enrolled/revoked flags.
//!
//! Policy (reversed/extended vs. the owner-only `GET /v1/tagmata/{id}`):
//! - Usernames are already exposed to room peers via the `@handle`; this surface
//!   extends that to any unauthenticated caller, so `GET /v1/users/{username}`
//!   is a (rate-limited) username-enumeration oracle. Accepted pre-release; must
//!   be re-reviewed before prod.
//! - Tagma ids are 128-bit random UUIDs (unguessable); only a holder of a real
//!   id (a room peer / invite recipient, who already sees the owner `@handle`)
//!   can resolve one. The owner username is therefore not a new disclosure to
//!   room participants.
//!
//! Input is normalized before any DB hit so `/user/Alice` and `/user/alice`
//! resolve identically and a malformed shape returns the same 404 as a
//! nonexistent one (no shape-based oracle).

use crate::db::entity::{tagmata, users};
use crate::db::map_db_err;
use crate::state::SharedState;
use crate::username;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use kallip_common::protocol::ApiError;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::Serialize;
use time::OffsetDateTime;

/// The unauthenticated public-profile surface. Rate-limited at the mount site
/// (per-IP, mirroring `enroll`) so the username path cannot be swept freely.
pub fn public_router() -> Router<SharedState> {
    Router::new()
        .route("/users/{username}", get(public_user_profile))
        .route("/tagmata/{id}/profile", get(public_tagma_profile))
}

#[derive(Serialize)]
struct PublicUserProfile {
    username: String,
    display_name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

/// A malformed or nonexistent username is the same 404: normalizing first keeps a
/// bad shape indistinguishable from a miss (no case/shape oracle), and a disabled
/// account is hidden (mirrors the session deny).
async fn public_user_profile(
    State(state): State<SharedState>,
    Path(raw): Path<String>,
) -> Result<Json<PublicUserProfile>, ApiError> {
    let username = username::normalize(&raw).map_err(|_| ApiError::not_found("unknown user"))?;
    let user = users::Entity::find()
        .filter(users::Column::Username.eq(username.clone()))
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown user"))?;
    if user.disabled_at.is_some() {
        return Err(ApiError::not_found("unknown user"));
    }
    Ok(Json(PublicUserProfile {
        username: user.username,
        display_name: user.display_name,
        created_at: user.created_at,
    }))
}

#[derive(Serialize)]
struct PublicTagmaProfile {
    tagma_id: String,
    label: Option<String>,
    owner_username: String,
    owner_display_name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

/// A tagma is profile-visible only when enrolled and live: pending (no device key
/// yet) and revoked tagmas 404, as does one whose owner is disabled. Each gate
/// returns the same 404 so a caller cannot distinguish the states.
async fn public_tagma_profile(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<PublicTagmaProfile>, ApiError> {
    let tagma = tagmata::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown tagma"))?;
    if tagma.enrolled_at.is_none() || tagma.revoked_at.is_some() {
        return Err(ApiError::not_found("unknown tagma"));
    }
    let owner = users::Entity::find_by_id(tagma.owner_user_id.clone())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown tagma"))?;
    if owner.disabled_at.is_some() {
        return Err(ApiError::not_found("unknown tagma"));
    }
    Ok(Json(PublicTagmaProfile {
        tagma_id: tagma.id,
        label: tagma.label,
        owner_username: owner.username,
        owner_display_name: owner.display_name,
        created_at: tagma.created_at,
    }))
}
