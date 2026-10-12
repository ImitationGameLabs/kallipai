//! The `/user/groups` face: the caller's groups, their members, and the
//! platform catalog a collection can be published into.

use super::shared::owner_of;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::distribution::visibility;
use crate::management::UserSession;
use crate::registry::EVERYONE_GROUP_ID;
use crate::registry::{group, group_member};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UserGroupView {
    pub group_id: String,
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UserGroupViews {
    pub groups: Vec<UserGroupView>,
}

/// POST /user/groups body: a member-managed group in the caller's space.
#[derive(Deserialize)]
pub(crate) struct UserGroupCreate {
    pub name: String,
    pub members: Option<Vec<String>>,
}

/// PATCH /user/groups/{group_id}: `name` and `members` are independent
/// (absent means unchanged; a present member list replaces the whole
/// membership).
#[derive(Deserialize)]
pub(crate) struct UserGroupUpdate {
    pub name: Option<String>,
    pub members: Option<Vec<String>>,
}

/// GET /user/platform-collections: one published platform collection
/// the caller's platform reach carries.
#[derive(Clone, Debug, Serialize)]
pub(super) struct UserPlatformCollectionView {
    pub owner: String,
    pub name: String,
    pub description: String,
    pub sets: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserPlatformCollectionViews {
    pub collections: Vec<UserPlatformCollectionView>,
}

/// GET /user/groups: the caller's groups with their members.
pub(super) async fn list_groups(
    State(state): State<AppState>,
    usr: UserSession,
) -> Result<Json<UserGroupViews>, ApiError> {
    let owner = owner_of(&usr);
    let rows = group::Entity::find()
        .filter(group::Column::Owner.eq(owner.clone()))
        .order_by_asc(group::Column::GroupId)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        views.push(user_group_view(&state.db, row).await?);
    }
    Ok(Json(UserGroupViews { groups: views }))
}

/// GET /user/groups/{group_id}.
pub(super) async fn get_group(
    State(state): State<AppState>,
    usr: UserSession,
    Path(group_id): Path<String>,
) -> Result<Json<UserGroupView>, ApiError> {
    let owner = owner_of(&usr);
    let row = owned_group(&state.db, &owner, &group_id).await?;
    Ok(Json(user_group_view(&state.db, row).await?))
}

/// POST /user/groups: a member-managed group in the caller's space (the
/// group id is minted here; the name is unique per owner).
pub(super) async fn create_group(
    State(state): State<AppState>,
    usr: UserSession,
    Json(body): Json<UserGroupCreate>,
) -> Result<Json<UserGroupView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    if body.name.is_empty() {
        return Err(ApiError::bad_request("group name must not be empty"));
    }
    if body.name == EVERYONE_GROUP_ID {
        return Err(ApiError::bad_request("the group name is reserved"));
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let clash = group::Entity::find()
        .filter(group::Column::Owner.eq(owner.clone()))
        .filter(group::Column::Name.eq(body.name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?;
    if clash.is_some() {
        return Err(ApiError::conflict("group already exists"));
    }
    let group_id = uuid::Uuid::new_v4().to_string();
    group::ActiveModel {
        group_id: Set(group_id.clone()),
        owner: Set(owner.clone()),
        name: Set(body.name.clone()),
        ..Default::default()
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    if let Some(members) = &body.members {
        insert_group_members(&txn, &group_id, members).await?;
    }
    let row = group::Entity::find_by_id(group_id.clone())
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .expect("the row was just inserted");
    let view = user_group_view(&txn, row).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "group",
            entity_id: group_id.clone(),
            actor,
            before: None,
            after: Some(serde_json::to_value(&view).expect("view serializes")),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(view))
}

/// PATCH /user/groups/{group_id}: `name` and `members` are independent
/// (absent means unchanged).
pub(super) async fn update_group(
    State(state): State<AppState>,
    usr: UserSession,
    Path(group_id): Path<String>,
    Json(body): Json<UserGroupUpdate>,
) -> Result<Json<UserGroupView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = owned_group(&txn, &owner, &group_id).await?;
    let before = user_group_view(&txn, row).await?;
    if let Some(name) = &body.name {
        if name.is_empty() {
            return Err(ApiError::bad_request("group name must not be empty"));
        }
        if name == EVERYONE_GROUP_ID {
            return Err(ApiError::bad_request("the group name is reserved"));
        }
        let clash = group::Entity::find()
            .filter(group::Column::Owner.eq(owner.clone()))
            .filter(group::Column::Name.eq(name.clone()))
            .one(&txn)
            .await
            .map_err(db::map_db_err)?;
        if clash.is_some() {
            return Err(ApiError::conflict("group already exists"));
        }
        let active = group::Entity::find_by_id(group_id.clone())
            .one(&txn)
            .await
            .map_err(db::map_db_err)?
            .expect("checked above");
        let mut active = group::ActiveModel::from(active);
        active.name = Set(name.clone());
        active.update(&txn).await.map_err(db::map_db_err)?;
    }
    if let Some(members) = &body.members {
        group_member::Entity::delete_many()
            .filter(group_member::Column::GroupId.eq(group_id.clone()))
            .exec(&txn)
            .await
            .map_err(db::map_db_err)?;
        insert_group_members(&txn, &group_id, members).await?;
    }
    let row = group::Entity::find_by_id(group_id.clone())
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .expect("the row was just read");
    let after = user_group_view(&txn, row).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "group",
            entity_id: group_id.clone(),
            actor,
            before: Some(serde_json::to_value(&before).expect("view serializes")),
            after: Some(serde_json::to_value(&after).expect("view serializes")),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(after))
}

/// DELETE /user/groups/{group_id}: the members and publications cascade.
pub(super) async fn delete_group(
    State(state): State<AppState>,
    usr: UserSession,
    Path(group_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = owned_group(&txn, &owner, &group_id).await?;
    let before = user_group_view(&txn, row).await?;
    group::Entity::delete_by_id(group_id.clone())
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "group",
            entity_id: group_id.clone(),
            actor,
            before: Some(serde_json::to_value(&before).expect("view serializes")),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// The caller's group, or 404 (another owner's group id does not exist
/// on this face).
async fn owned_group<C: sea_orm::ConnectionTrait>(
    db: &C,
    owner: &str,
    group_id: &str,
) -> Result<group::Model, ApiError> {
    let row = group::Entity::find_by_id(group_id.to_owned())
        .one(db)
        .await
        .map_err(db::map_db_err)?;
    row.filter(|g| g.owner == owner)
        .ok_or_else(|| ApiError::not_found(format!("no such group: {group_id}")))
}

async fn insert_group_members(
    txn: &sea_orm::DatabaseTransaction,
    group_id: &str,
    members: &[String],
) -> Result<(), ApiError> {
    for member in members {
        if member.is_empty() {
            return Err(ApiError::bad_request("member account must not be empty"));
        }
        group_member::ActiveModel {
            group_id: Set(group_id.to_owned()),
            member_account: Set(member.clone()),
        }
        .insert(txn)
        .await
        .map_err(db::map_db_err)?;
    }
    Ok(())
}

async fn user_group_view<C: sea_orm::ConnectionTrait>(
    db: &C,
    row: group::Model,
) -> Result<UserGroupView, ApiError> {
    let members = group_member::Entity::find()
        .filter(group_member::Column::GroupId.eq(row.group_id.clone()))
        .all(db)
        .await
        .map_err(db::map_db_err)?;
    Ok(UserGroupView {
        group_id: row.group_id,
        name: row.name,
        members: members.into_iter().map(|m| m.member_account).collect(),
    })
}

// -- platform catalog -------------------------------------------------------

/// GET /user/platform-collections: the platform catalog the caller's
/// platform reach carries (the everyone publications plus the caller's
/// platform groups). Read-only: the catalog is platform-published
/// material surfaced on the user face.
pub(super) async fn list_platform_collections(
    State(state): State<AppState>,
    usr: UserSession,
) -> Result<Json<UserPlatformCollectionViews>, ApiError> {
    let account = owner_of(&usr);
    let viewer = visibility::Viewer::resolve(&state.db, account)
        .await
        .map_err(db::map_db_err)?;
    let entries = visibility::platform_catalog(&state.db, &viewer)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(UserPlatformCollectionViews {
        collections: entries
            .into_iter()
            .map(|e| UserPlatformCollectionView {
                owner: e.owner,
                name: e.collection_name,
                description: e.description,
                sets: e.set_names,
            })
            .collect(),
    }))
}
