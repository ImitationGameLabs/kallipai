//! The `/user/sets` face: the caller's ordered configuration sets and the
//! wire shapes they read and write.

use super::collections::collection_default_guard;
use super::shared::owner_of;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::UserSession;
use crate::registry::{self, profile_set, set_member};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserSetView {
    pub name: String,
    pub description: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserSetViews {
    pub sets: Vec<UserSetView>,
}

/// PATCH /user/sets/{name}: `description` and `members` are independent
/// (absent means unchanged; a present member list replaces the whole
/// membership).
#[derive(Deserialize)]
pub(super) struct UserSetUpdate {
    pub description: Option<String>,
    pub members: Option<Vec<String>>,
}

/// GET /user/sets: the caller's configuration sets with their ordered
/// members.
pub(super) async fn list_sets(
    State(state): State<AppState>,
    usr: UserSession,
) -> Result<Json<UserSetViews>, ApiError> {
    let owner = owner_of(&usr);
    let sets = profile_set::Entity::find()
        .filter(profile_set::Column::Owner.eq(owner.clone()))
        .order_by_asc(profile_set::Column::Name)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut views = Vec::with_capacity(sets.len());
    for set in sets {
        views.push(user_set_view(&state.db, owner.clone(), set).await?);
    }
    Ok(Json(UserSetViews { sets: views }))
}

/// GET /user/sets/{name}.
pub(super) async fn get_set(
    State(state): State<AppState>,
    usr: UserSession,
    Path(name): Path<String>,
) -> Result<Json<UserSetView>, ApiError> {
    let owner = owner_of(&usr);
    let set = registry::set_by_id(&state.db, &owner, &name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such set: {name}")))?;
    Ok(Json(user_set_view(&state.db, owner, set).await?))
}

/// PATCH /user/sets/{name}: `description` and `members` are independent
/// (absent means unchanged; a present member list replaces the whole
/// membership, order = failover order).
pub(super) async fn update_set(
    State(state): State<AppState>,
    usr: UserSession,
    Path(name): Path<String>,
    Json(body): Json<UserSetUpdate>,
) -> Result<Json<UserSetView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let set = registry::set_by_id(&txn, &owner, &name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such set: {name}")))?;
    let before = user_set_view(&txn, owner.clone(), set).await?;
    if let Some(description) = &body.description {
        let mut active = profile_set::ActiveModel::from(
            profile_set::Entity::find_by_id((owner.clone(), name.clone()))
                .one(&txn)
                .await
                .map_err(db::map_db_err)?
                .expect("checked above"),
        );
        active.description = Set(description.clone());
        active.update(&txn).await.map_err(db::map_db_err)?;
    }
    if let Some(members) = &body.members {
        replace_set_members(&txn, &owner, &name, members).await?;
    }
    let set = registry::set_by_id(&txn, &owner, &name)
        .await
        .map_err(db::map_db_err)?
        .expect("the row was just read");
    let after = user_set_view(&txn, owner.clone(), set).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "set",
            entity_id: name.clone(),
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

/// DELETE /user/sets/{name}: the membership rows cascade.
pub(super) async fn delete_set(
    State(state): State<AppState>,
    usr: UserSession,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let set = registry::set_by_id(&txn, &owner, &name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such set: {name}")))?;
    let before = user_set_view(&txn, owner.clone(), set).await?;
    collection_default_guard(&txn, &owner, &name, "user").await?;
    profile_set::Entity::delete_by_id((owner.clone(), name.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "set",
            entity_id: name.clone(),
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

/// Replace a set's whole membership (the list order is the failover
/// order); every member must exist in the caller's space.
pub(super) async fn replace_set_members<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    name: &str,
    members: &[String],
) -> Result<(), ApiError> {
    set_member::Entity::delete_many()
        .filter(set_member::Column::Owner.eq(owner))
        .filter(set_member::Column::SetName.eq(name))
        .exec(txn)
        .await
        .map_err(db::map_db_err)?;
    for (position, profile_id) in members.iter().enumerate() {
        registry::profile_by_id(txn, owner, profile_id)
            .await
            .map_err(db::map_db_err)?
            .ok_or_else(|| ApiError::not_found(format!("no such profile: {profile_id}")))?;
        set_member::ActiveModel {
            owner: Set(owner.to_owned()),
            set_name: Set(name.to_owned()),
            profile_id: Set(profile_id.clone()),
            position: Set(position as i32),
        }
        .insert(txn)
        .await
        .map_err(db::map_db_err)?;
    }
    Ok(())
}

pub(super) async fn user_set_view<C: sea_orm::ConnectionTrait>(
    db: &C,
    owner: String,
    set: profile_set::Model,
) -> Result<UserSetView, ApiError> {
    let members = set_member::Entity::find()
        .filter(set_member::Column::Owner.eq(owner))
        .filter(set_member::Column::SetName.eq(set.name.clone()))
        .order_by_asc(set_member::Column::Position)
        .all(db)
        .await
        .map_err(db::map_db_err)?;
    Ok(UserSetView {
        name: set.name,
        description: set.description,
        members: members.into_iter().map(|m| m.profile_id).collect(),
    })
}
