//! Signup username-availability probe (route + handler).

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use kallipai_common::protocol::ApiError;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use crate::db::entity::users;
use crate::db::map_db_err;
use crate::state::SharedState;
use crate::username;

/// The signup availability probe: `GET /auth/username-availability`.
/// Unauthenticated (pre-signup callers hold no credential) and therefore an
/// explicit, full username-enumeration oracle -- accepted pre-release and
/// mounted with the same per-IP rate-limit family as the other
/// unauthenticated reads, mirroring `GET /users/{username}`'s posture
/// (see `routes::router` and `public_profiles`).
pub fn availability_router() -> Router<SharedState> {
    Router::new().route("/auth/username-availability", get(username_availability))
}

#[derive(Deserialize)]
pub struct AvailabilityQuery {
    pub username: Option<String>,
}

/// The probe's outcome. Always a 200: the signup form's debounce loop is a
/// read protocol, so every outcome -- including a malformed shape or a
/// missing query param -- is a body status, not an HTTP error path. The
/// refusal order is deterministic: shape, then the reserved list, then the
/// single indexed `users` lookup, so a reserved name reports `reserved`
/// even when a legacy row also holds it.
#[derive(Debug, Serialize)]
pub struct UsernameAvailability {
    /// The canonical (normalized) form when the shape is valid; the raw
    /// query echoed back when it is not, so the client can match this
    /// response to the keystroke batch that issued the probe.
    pub username: String,
    pub status: UsernameAvailabilityStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsernameAvailabilityStatus {
    /// Normalizes, is not reserved, and no user row holds it.
    Available,
    /// A user row (live or disabled) already holds it.
    Taken,
    /// On the reserved list -- refused at every signup rail.
    Reserved,
    /// Fails `normalize` (shape/length/charset) or is missing.
    Invalid,
}

pub(super) async fn username_availability(
    State(state): State<SharedState>,
    Query(req): Query<AvailabilityQuery>,
) -> Result<Json<UsernameAvailability>, ApiError> {
    let invalid = |username: String| UsernameAvailability {
        username,
        status: UsernameAvailabilityStatus::Invalid,
    };
    let Some(raw) = req.username else {
        return Ok(Json(invalid(String::new())));
    };
    let Ok(username) = username::normalize(&raw) else {
        return Ok(Json(invalid(raw)));
    };
    if username::is_reserved(&username) {
        return Ok(Json(UsernameAvailability {
            username,
            status: UsernameAvailabilityStatus::Reserved,
        }));
    }
    let taken = users::Entity::find()
        .filter(users::Column::Username.eq(&username))
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .is_some();
    let status = if taken {
        UsernameAvailabilityStatus::Taken
    } else {
        UsernameAvailabilityStatus::Available
    };
    Ok(Json(UsernameAvailability { username, status }))
}
