//! GET /admin/delivery-events: the admin management face (matrix row 8).
//! The query reads the append-only delivery log; blob content stays closed
//! to the admin principal -- `authorize` denies every content operation for
//! `Principal::Admin`, which is what keeps row 8's "management only" shape.

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;

use crate::auth::AuthPrincipal;
use crate::state::AppState;
use kallip_archeion_common::principal::Principal;
use kallip_common::protocol::ApiError;

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// Restrict the log to one blob.
    pub blob_id: Option<String>,
    /// Page size, capped server-side.
    pub limit: Option<u64>,
}

/// One delivery event as served. `happened_at` is RFC 3339.
#[derive(Debug, Serialize)]
pub struct DeliveryEventView {
    pub id: uuid::Uuid,
    pub happened_at: String,
    pub from_principal: String,
    pub to_principal: String,
    pub blob_id: String,
    pub source_record_id: Option<uuid::Uuid>,
    pub target_record_id: Option<uuid::Uuid>,
}

/// The server-side page cap: a client can ask for fewer, never more.
const MAX_LIMIT: u64 = 1000;

/// GET /admin/delivery-events?blob_id=&limit=
pub async fn list_events(
    State(state): State<AppState>,
    AuthPrincipal(principal): AuthPrincipal,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<DeliveryEventView>>, ApiError> {
    // Row 8: management face is admin-only. A non-admin gets the same
    // shape of denial the content routes give them.
    if !matches!(principal, Principal::Admin) {
        return Err(ApiError::forbidden("admin access required"));
    }
    let limit = query.limit.unwrap_or(100).min(MAX_LIMIT);
    let events =
        crate::metadata::repo::list_delivery_events(&state.db, query.blob_id.as_deref(), limit)
            .await
            .map_err(ApiError::internal)?;
    let views = events
        .into_iter()
        .map(|event| {
            let happened_at = event
                .happened_at
                .format(&Rfc3339)
                .unwrap_or_else(|_| event.happened_at.to_string());
            DeliveryEventView {
                id: event.id,
                happened_at,
                from_principal: event.from_principal,
                to_principal: event.to_principal,
                blob_id: event.blob_id,
                source_record_id: event.source_record_id,
                target_record_id: event.target_record_id,
            }
        })
        .collect();
    Ok(Json(views))
}
