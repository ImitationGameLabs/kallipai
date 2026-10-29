//! The `/user/sets` face: the caller's ordered configuration sets and the
//! wire shapes they read and write. A set lives inside one of the caller's
//! collections, so the item routes name it and the list filter does.

use super::collections::collection_default_guard;
use super::shared::owner_of;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::{
    UserSession, lock_set_order, refuse_set_members, resolve_set, write_set_order,
};
use crate::registry::{self, profile, profile_set};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ColumnTrait, EntityTrait, QueryFilter, QueryOrder, TransactionTrait, sea_query::Expr,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserSetView {
    pub name: String,
    pub collection_name: String,
    pub description: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserSetViews {
    pub sets: Vec<UserSetView>,
}

/// GET /user/sets query: the collection to read (`None` = all of the
/// caller's collections).
/// The query shape is closed: any other key is a 400.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserSetFilter {
    pub collection_name: Option<String>,
}

/// PATCH /user/collections/{collection}/sets/{name}: the description.
/// Membership and order are the profile face's `set` field and the set's
/// `/order` route.
#[derive(Deserialize)]
pub(super) struct UserSetUpdate {
    pub description: Option<String>,
    /// Present only to refuse: membership is the profile's own `set` field
    /// (who belongs to a set), and order is the set's `/order` route.
    #[serde(default)]
    pub members: Option<serde_json::Value>,
}

/// GET /user/sets: the caller's configuration sets with their ordered
/// members, optionally narrowed to one collection.
pub(super) async fn list_sets(
    State(state): State<AppState>,
    usr: UserSession,
    Query(filter): Query<UserSetFilter>,
) -> Result<Json<UserSetViews>, ApiError> {
    let owner = owner_of(&usr);
    let mut query =
        profile_set::Entity::find().filter(profile_set::Column::Owner.eq(owner.clone()));
    if let Some(collection_name) = filter.collection_name {
        query = query.filter(profile_set::Column::CollectionName.eq(collection_name));
    }
    let sets = query
        .order_by_asc(profile_set::Column::CollectionName)
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

/// GET /user/collections/{collection}/sets/{name}.
pub(super) async fn get_set(
    State(state): State<AppState>,
    usr: UserSession,
    Path((collection, name)): Path<(String, String)>,
) -> Result<Json<UserSetView>, ApiError> {
    let owner = owner_of(&usr);
    let set = registry::set_by_id(&state.db, &owner, &collection, &name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!("no such set: {name} in collection {collection}"))
        })?;
    Ok(Json(user_set_view(&state.db, owner, set).await?))
}

/// PATCH /user/collections/{collection}/sets/{name}: rewrite the set's
/// description. Membership is the profile face's `set` field, order the
/// set's `/order` route.
pub(super) async fn update_set(
    State(state): State<AppState>,
    usr: UserSession,
    Path((collection, name)): Path<(String, String)>,
    Json(body): Json<UserSetUpdate>,
) -> Result<Json<UserSetView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    refuse_set_members(&body.members)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let set = resolve_set(&txn, &owner, &collection, &name).await?;
    let before = user_set_view(&txn, owner.clone(), set).await?;
    // A set deleted after the resolve above answers 404 from the write
    // itself, the same exit every other lost-set read takes. This handler
    // has no 409: the sibling `/order` route keeps one as a guard, and its
    // locks make that guard stand for a lock that missed a row rather than
    // a race a caller hits.
    if let Some(description) = &body.description {
        let written = profile_set::Entity::update_many()
            .col_expr(
                profile_set::Column::Description,
                Expr::value(description.clone()),
            )
            .filter(profile_set::Column::Owner.eq(owner.clone()))
            .filter(profile_set::Column::CollectionName.eq(collection.clone()))
            .filter(profile_set::Column::Name.eq(name.clone()))
            .exec(&txn)
            .await
            .map_err(db::map_db_err)?;
        if written.rows_affected != 1 {
            return Err(ApiError::not_found(format!(
                "no such set: {name} in collection {collection}"
            )));
        }
    }
    let set = resolve_set(&txn, &owner, &collection, &name).await?;
    let after = user_set_view(&txn, owner.clone(), set).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "profile_set",
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

/// DELETE /user/collections/{collection}/sets/{name}: the set row goes, and
/// its members stay as unassigned rows (their `set_name` clears), not deleted.
pub(super) async fn delete_set(
    State(state): State<AppState>,
    usr: UserSession,
    Path((collection, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let set = resolve_set(&txn, &owner, &collection, &name).await?;
    let before = user_set_view(&txn, owner.clone(), set).await?;
    collection_default_guard(&txn, &owner, &collection, &name, "user").await?;
    profile_set::Entity::delete_by_id((owner.clone(), collection.clone(), name.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "profile_set",
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

/// PUT /user/collections/{collection}/sets/{name}/order body: the set's
/// members in the order they should fail over. A permutation, not a
/// membership edit: the ids must be exactly the set's current ones.
#[derive(Deserialize)]
pub(super) struct UserSetOrder {
    pub members: Vec<String>,
}

/// PUT /user/collections/{collection}/sets/{name}/order: rewrite the
/// failover order in place. `lock_set_order` holds the rows this write
/// could touch and checks the request is a permutation of the membership;
/// `write_set_order` writes the positions. Both faces of this route share
/// that one protocol; this handler resolves the set, audits, and commits.
/// An id outside the set, a missing id, or a repeat is a 400, a set that is
/// gone is a 404, and the 409 `write_set_order` can answer is a guard: every
/// row it renumbers is held, so that answer stands for a lock that missed a
/// row rather than a membership change a caller can race.
pub(super) async fn reorder_set(
    State(state): State<AppState>,
    usr: UserSession,
    Path((collection, name)): Path<(String, String)>,
    Json(body): Json<UserSetOrder>,
) -> Result<Json<UserSetView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    // A missing set answers 404 before any lock: there are no rows to hold.
    resolve_set(&txn, &owner, &collection, &name).await?;
    lock_set_order(&txn, &owner, &collection, &name, &body.members).await?;
    let set = resolve_set(&txn, &owner, &collection, &name).await?;
    let before = user_set_view(&txn, owner.clone(), set).await?;
    write_set_order(&txn, &owner, &collection, &name, &body.members).await?;
    let set = resolve_set(&txn, &owner, &collection, &name).await?;
    let after = user_set_view(&txn, owner.clone(), set).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "profile_set",
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

pub(super) async fn user_set_view<C: sea_orm::ConnectionTrait>(
    db: &C,
    owner: String,
    set: profile_set::Model,
) -> Result<UserSetView, ApiError> {
    let members = profile::Entity::find()
        .filter(profile::Column::Owner.eq(owner))
        .filter(profile::Column::CollectionName.eq(set.collection_name.clone()))
        .filter(profile::Column::SetName.eq(set.name.clone()))
        .order_by_asc(profile::Column::Position)
        .order_by_asc(profile::Column::ProfileId)
        .all(db)
        .await
        .map_err(db::map_db_err)?;
    Ok(UserSetView {
        name: set.name,
        collection_name: set.collection_name,
        description: set.description,
        members: members.into_iter().map(|m| m.profile_id).collect(),
    })
}
