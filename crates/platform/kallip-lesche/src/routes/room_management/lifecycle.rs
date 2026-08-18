//! Room creation and listing: the founding of a room (caller is the founding
//! member), the caller's own-room listing, and the public-room discovery list.
//! Ported from the agora registry; reads and writes the lesche-local membership
//! graph.

use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::{room_members, rooms};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::state::SharedConvState;
use axum::Json;
use axum::extract::State;
use kallip_agora_common::ids::{ParticipantId, ParticipantKind};
use kallip_common::protocol::ApiError;
use kallip_lesche_common::rooms::{RoomId, Visibility};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub(super) struct RoomView {
    pub(super) room_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub(super) created_at: OffsetDateTime,
    pub(super) name: String,
    pub(super) description: String,
    pub(super) visibility: Visibility,
}

impl From<&rooms::Model> for RoomView {
    fn from(r: &rooms::Model) -> Self {
        Self {
            room_id: r.id.clone(),
            created_at: r.created_at,
            name: r.name.clone(),
            description: r.description.clone(),
            visibility: Visibility::from_db(&r.visibility),
        }
    }
}

/// Body of `POST /v1/rooms`. `name` is required (validated non-empty by the
/// handler); `description` and `visibility` default, so only the name is
/// mandatory.
#[derive(Deserialize)]
pub(super) struct CreateRoomRequest {
    pub(super) name: String,
    #[serde(default)]
    pub(super) description: String,
    #[serde(default)]
    pub(super) visibility: Visibility,
}

/// Create a room. The caller is the founding member and `created_by`. The room
/// id is a fresh UUID. `name` must be non-empty (400 otherwise). Name,
/// description, and visibility are immutable after create.
pub(super) async fn create_room(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(req): Json<CreateRoomRequest>,
) -> Result<Json<RoomView>, ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::bad_request("room name is required"));
    }
    let user_id = require_user(&principal)?;
    let db = state.require_db()?;
    let now = OffsetDateTime::now_utc();
    let room_id = RoomId::from(Uuid::new_v4().to_string());
    let participant_id = ParticipantId::for_user(user_id);
    // Store the trimmed name/description verbatim (validation only rejected
    // whitespace-only names); the response echoes the same trimmed values.
    let name = req.name.trim().to_string();
    let description = req.description.trim().to_string();
    let result = db
        .transaction::<_, _, TxnError>(|txn| {
            let room_id = room_id.to_string();
            let user_id = user_id.to_string();
            let participant_id = participant_id.as_ref().to_string();
            let visibility = req.visibility.as_str().to_string();
            let name = name.clone();
            let description = description.clone();
            Box::pin(async move {
                rooms::ActiveModel {
                    id: Set(room_id.clone()),
                    created_by_user_id: Set(user_id.clone()),
                    created_at: Set(now),
                    membership_epoch: Set(1),
                    name: Set(name),
                    description: Set(description),
                    visibility: Set(visibility),
                }
                .insert(txn)
                .await?;
                room_members::ActiveModel {
                    room_id: Set(room_id),
                    member_id: Set(participant_id),
                    kind: Set(ParticipantKind::Human.as_str().to_string()),
                    source_id: Set(user_id.clone()),
                    joined_at: Set(now),
                    added_by: Set(user_id),
                }
                .insert(txn)
                .await?;
                Ok(())
            })
        })
        .await;
    flatten_txn(result)?;
    Ok(Json(RoomView {
        room_id: room_id.to_string(),
        created_at: now,
        name,
        description,
        visibility: req.visibility,
    }))
}

/// List the caller's rooms (where they are a current member), newest-joined
/// first. Membership is the only filter.
pub(super) async fn list_rooms(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<Vec<RoomView>>, ApiError> {
    let user_id = require_user(&principal)?;
    let db = state.require_db()?;
    let participant_id = ParticipantId::for_user(user_id);
    // The caller's member rows, newest-joined first. The room rows are fetched
    // separately (sea-orm has no easy join here) and indexed by id, then emitted
    // in the membership order -- `Id.is_in` returns rows in DB/heap order, so a
    // direct `rows.iter()` would lose the `joined_at DESC` ordering.
    let memberships = room_members::Entity::find()
        .filter(room_members::Column::MemberId.eq(participant_id.as_ref().to_string()))
        .order_by_desc(room_members::Column::JoinedAt)
        .all(db)
        .await
        .map_err(map_db_err)?;
    if memberships.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let room_ids: Vec<String> = memberships.iter().map(|m| m.room_id.clone()).collect();
    let rows = rooms::Entity::find()
        .filter(rooms::Column::Id.is_in(room_ids))
        .all(db)
        .await
        .map_err(map_db_err)?;
    let by_id: std::collections::HashMap<&str, &rooms::Model> =
        rows.iter().map(|r| (r.id.as_str(), r)).collect();
    let items = memberships
        .iter()
        .filter_map(|m| by_id.get(m.room_id.as_str()).map(|r| RoomView::from(*r)))
        .collect();
    Ok(Json(items))
}

/// List public rooms (plaintext, open-access) the caller may join without an
/// invite, newest-created first. Any authenticated user. This is the discovery
/// surface for the public-room "browse + join" flow; private rooms are never
/// listed here. A sequential scan is fine for the MVP public-room set; a future
/// `idx_rooms_visibility` index can be added if scale demands it.
pub(super) async fn list_public_rooms(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<Vec<RoomView>>, ApiError> {
    let _user_id = require_user(&principal)?;
    let db = state.require_db()?;
    let rows = rooms::Entity::find()
        .filter(rooms::Column::Visibility.eq(Visibility::Public.as_str()))
        .order_by_desc(rooms::Column::CreatedAt)
        .all(db)
        .await
        .map_err(map_db_err)?;
    let items = rows.iter().map(RoomView::from).collect();
    Ok(Json(items))
}
