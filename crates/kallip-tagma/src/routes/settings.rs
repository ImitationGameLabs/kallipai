//! Runtime settings surface: the display timezone.

use axum::Json;
use axum::extract::State;
use kallip_common::protocol::ApiError;
use serde_json::json;

use crate::auth::{AuthIdentity, require_operator};
use crate::state::SharedState;

/// GET /settings/timezone — the configured IANA timezone name.
///
/// Any authenticated principal may read: agents render lists through the
/// CLI with this setting in effect. The `AuthIdentity` extractor rejects
/// unauthenticated requests before this handler runs.
pub async fn get_timezone(
    State(_state): State<SharedState>,
    _auth: AuthIdentity,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        json!({ "timezone": crate::settings::load_timezone() }),
    ))
}

/// PUT /settings/timezone — set (or clear with null) the timezone.
///
/// Operator-only: it changes every rendering surface on the tagma.
pub async fn put_timezone(
    State(_state): State<SharedState>,
    auth: AuthIdentity,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_operator(auth.identity())?;
    let name = match body.get("timezone") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(serde_json::Value::Null) | None => None,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "timezone must be an IANA name string or null, got {other}"
            )));
        }
    };
    if let Some(name) = &name
        && !crate::settings::timezone_name_valid(name)
    {
        return Err(ApiError::bad_request(format!(
            "{name} is not a valid IANA timezone name"
        )));
    }
    crate::settings::save_timezone(name.as_deref())
        .map_err(|e| ApiError::internal(format!("persisting settings: {e}")))?;
    Ok(Json(json!({ "timezone": name })))
}
