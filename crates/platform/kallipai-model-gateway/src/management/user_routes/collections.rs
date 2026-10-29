//! The `/user/collections` face: the caller's collections, their publications
//! and their set members, plus the cascade helpers the admin face borrows when
//! a collection, group or set disappears.

use super::profiles::user_profile_view;
use super::sets::{UserSetView, replace_set_members, user_set_view};
use super::shared::owner_of;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::UserSession;
use crate::management::validate_set_name;
use crate::registry::EVERYONE_GROUP_ID;
use crate::registry::{
    self, collection, collection_publication, group, profile, profile_set, set_member,
};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use kallipai_common::protocol::ApiError;
use sea_orm::sea_query::{Expr, Query};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UserCollectionView {
    pub name: String,
    pub description: String,
    pub sets: Vec<String>,
    /// The collection's default-set anchor (null = none chosen yet).
    pub default_set: Option<String>,
    /// How many profile rows the collection holds (unassigned rows included):
    /// the delete confirmation states this number, so the cost is never a
    /// guess.
    pub profile_count: u64,
    /// How many sets the collection anchors (the cascade's other half).
    pub set_count: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserCollectionViews {
    pub collections: Vec<UserCollectionView>,
}

/// POST /user/collections body: a fresh collection starts empty (sets
/// join only through the nested creation door).
#[derive(Deserialize)]
pub(crate) struct UserCollectionCreate {
    pub name: String,
    pub description: String,
}

/// PATCH /user/collections/{name}: `description` only (absent means
/// unchanged); the set membership is not a collection attribute.
/// Set membership changes only by creating or deleting a set.
#[derive(Deserialize)]
pub(crate) struct UserCollectionUpdate {
    pub description: Option<String>,
}

/// POST /user/collections/{name}/sets body: a set created directly
/// inside the named collection (the collection anchors the set).
#[derive(Deserialize)]
pub(super) struct UserCollectionSetCreate {
    pub name: String,
    pub description: String,
    pub members: Option<Vec<String>>,
}

/// PATCH /user/collections/{name}/default body: the member set that
/// takes over as the collection's default.
#[derive(Deserialize)]
pub(crate) struct UserCollectionDefaultBody {
    pub set_name: String,
}

/// POST /user/collections/{name}/publications body: the audience group
/// the collection becomes visible to.
#[derive(Deserialize)]
pub(crate) struct UserPublicationBody {
    pub audience: String,
}

/// GET /user/collections: the caller's collections with their (unordered)
/// set members.
pub(super) async fn list_collections(
    State(state): State<AppState>,
    usr: UserSession,
) -> Result<Json<UserCollectionViews>, ApiError> {
    let owner = owner_of(&usr);
    let rows = collection::Entity::find()
        .filter(collection::Column::Owner.eq(owner.clone()))
        .order_by_asc(collection::Column::Name)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        views.push(user_collection_view(&state.db, owner.clone(), row).await?);
    }
    Ok(Json(UserCollectionViews { collections: views }))
}

/// GET /user/collections/{name}.
pub(super) async fn get_collection(
    State(state): State<AppState>,
    usr: UserSession,
    Path(name): Path<String>,
) -> Result<Json<UserCollectionView>, ApiError> {
    let owner = owner_of(&usr);
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&state.db)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    Ok(Json(user_collection_view(&state.db, owner, row).await?))
}

/// POST /user/collections: the publishable bundle above sets.
pub(super) async fn create_collection(
    State(state): State<AppState>,
    usr: UserSession,
    Json(body): Json<UserCollectionCreate>,
) -> Result<Json<UserCollectionView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    validate_set_name(&body.name)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    if collection::Entity::find_by_id((owner.clone(), body.name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict("collection already exists"));
    }
    collection::ActiveModel {
        owner: Set(owner.clone()),
        name: Set(body.name.clone()),
        description: Set(body.description.clone()),
        default_set_name: Set(None),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    let row = collection::Entity::find_by_id((owner.clone(), body.name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .expect("the row was just inserted");
    let view = user_collection_view(&txn, owner.clone(), row).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "collection",
            entity_id: body.name.clone(),
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

/// PATCH /user/collections/{name}: `description` and `sets` are
/// independent (absent means unchanged).
pub(super) async fn update_collection(
    State(state): State<AppState>,
    usr: UserSession,
    Path(name): Path<String>,
    Json(body): Json<UserCollectionUpdate>,
) -> Result<Json<UserCollectionView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    let before = user_collection_view(&txn, owner.clone(), row).await?;
    if let Some(description) = &body.description {
        let active = collection::Entity::find_by_id((owner.clone(), name.clone()))
            .one(&txn)
            .await
            .map_err(db::map_db_err)?
            .expect("checked above");
        let mut active = collection::ActiveModel::from(active);
        active.description = Set(description.clone());
        active.update(&txn).await.map_err(db::map_db_err)?;
    }
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .expect("the row was just read");
    let after = user_collection_view(&txn, owner.clone(), row).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "collection",
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

/// DELETE /user/collections/{name}: the anchored sets, the profiles
/// they leave unreferenced, and the publication rows go with it.
pub(super) async fn delete_collection(
    State(state): State<AppState>,
    usr: UserSession,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    let before = user_collection_view(&txn, owner.clone(), row).await?;
    delete_collection_cascade(&txn, &owner, &name, &actor, CascadeFace::User).await?;
    collection::Entity::delete_by_id((owner.clone(), name.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "collection",
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

/// POST /user/collections/{name}/publications: make the collection
/// visible to a group (the publication is a row; a duplicate conflicts).
pub(super) async fn publish_collection(
    State(state): State<AppState>,
    usr: UserSession,
    Path(name): Path<String>,
    Json(body): Json<UserPublicationBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    // The reserved audience has no row: the lookup and the same-space
    // check apply only to user-managed groups.
    if body.audience != EVERYONE_GROUP_ID {
        let group_row = group::Entity::find_by_id(body.audience.clone())
            .one(&txn)
            .await
            .map_err(db::map_db_err)?
            .ok_or_else(|| ApiError::not_found(format!("no such group: {}", body.audience)))?;
        if group_row.owner != owner {
            return Err(ApiError::not_found(format!(
                "no such group: {}",
                body.audience
            )));
        }
    }
    if collection_publication::Entity::find_by_id((
        owner.clone(),
        name.clone(),
        body.audience.clone(),
    ))
    .one(&txn)
    .await
    .map_err(db::map_db_err)?
    .is_some()
    {
        return Err(ApiError::conflict(
            "collection is already published to this group",
        ));
    }
    collection_publication::ActiveModel {
        owner: Set(owner.clone()),
        collection_name: Set(name.clone()),
        group_id: Set(body.audience.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "publish",
            entity: "collection",
            entity_id: name.clone(),
            actor,
            before: None,
            after: Some(serde_json::json!({ "group_id": body.audience })),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(serde_json::json!({ "audience": body.audience })))
}

/// DELETE /user/collections/{name}/publications/{group}: withdraw from one group.
pub(super) async fn unpublish_collection(
    State(state): State<AppState>,
    usr: UserSession,
    Path((name, group)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    collection_publication::Entity::find_by_id((owner.clone(), name.clone(), group.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("not published to group {group}")))?;
    collection_publication::Entity::delete_by_id((owner.clone(), name.clone(), group.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "unpublish",
            entity: "collection",
            entity_id: name.clone(),
            actor,
            before: Some(serde_json::json!({ "group_id": group })),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// POST /user/collections/{collection}/sets: a set created directly inside
/// the collection -- the row carries the collection in `collection_name`,
/// the membership authority. The first set in a default-less collection
/// takes the default anchor, and the name is unique inside this collection
/// only.
pub(super) async fn create_collection_set(
    State(state): State<AppState>,
    usr: UserSession,
    Path(collection): Path<String>,
    Json(body): Json<UserCollectionSetCreate>,
) -> Result<Json<UserSetView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    validate_set_name(&body.name)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    collection::Entity::find_by_id((owner.clone(), collection.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {collection}")))?;
    if registry::set_by_id(&txn, &owner, &collection, &body.name)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict("set already exists"));
    }
    profile_set::ActiveModel {
        name: Set(body.name.clone()),
        description: Set(body.description.clone()),
        owner: Set(owner.clone()),
        collection_name: Set(collection.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    anchor_collection_default(&txn, &owner, &collection, &body.name).await?;
    if let Some(members) = &body.members {
        replace_set_members(&txn, &owner, &collection, &body.name, members).await?;
    }
    let set = registry::set_by_id(&txn, &owner, &collection, &body.name)
        .await
        .map_err(db::map_db_err)?
        .expect("the row was just inserted");
    let view = user_set_view(&txn, owner.clone(), set).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "set",
            entity_id: body.name.clone(),
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

/// PATCH /user/collections/{name}/default: transfer the collection's
/// default-set anchor. The target must be a set anchored to this
/// collection (the ownership column is the membership truth);
/// anything else, including a set anchored to another collection,
/// set, is a 404.
pub(super) async fn set_collection_default(
    State(state): State<AppState>,
    usr: UserSession,
    Path(name): Path<String>,
    Json(body): Json<UserCollectionDefaultBody>,
) -> Result<Json<UserCollectionView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    let before = user_collection_view(&txn, owner.clone(), row).await?;
    if registry::set_by_id(&txn, &owner, &name, &body.set_name)
        .await
        .map_err(db::map_db_err)?
        .is_none()
    {
        return Err(ApiError::not_found(format!(
            "no such set: {} in collection {name}",
            body.set_name
        )));
    }
    // The existence guard rides the statement: a set deleted or moved
    // between the read above and this update fails the EXISTS, and a
    // concurrently deleted collection leaves the filter empty -- both
    // land zero rows and the transfer refuses.
    let member_of_collection = Query::select()
        .expr(Expr::val(1))
        .from(profile_set::Entity)
        .and_where(profile_set::Column::Owner.eq(owner.as_str()))
        .and_where(profile_set::Column::Name.eq(body.set_name.as_str()))
        .and_where(profile_set::Column::CollectionName.eq(name.as_str()))
        .take();
    let transfer = collection::Entity::update_many()
        .col_expr(
            collection::Column::DefaultSetName,
            Expr::value(body.set_name.clone()),
        )
        .filter(collection::Column::Owner.eq(owner.as_str()))
        .filter(collection::Column::Name.eq(name.as_str()))
        .filter(Expr::exists(member_of_collection))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    if transfer.rows_affected != 1 {
        return Err(ApiError::conflict(format!(
            "collection {name} or its default-set member changed concurrently; re-read and retry"
        )));
    }
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .expect("the row was just read");
    let after = user_collection_view(&txn, owner.clone(), row).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "default",
            entity: "collection",
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
pub(crate) async fn user_collection_view<C: sea_orm::ConnectionTrait>(
    db: &C,
    owner: String,
    row: collection::Model,
) -> Result<UserCollectionView, ApiError> {
    let sets = profile_set::Entity::find()
        .filter(profile_set::Column::Owner.eq(owner.clone()))
        .filter(profile_set::Column::CollectionName.eq(row.name.clone()))
        .order_by_asc(profile_set::Column::Name)
        .all(db)
        .await
        .map_err(db::map_db_err)?;
    let set_count = sets.len() as u64;
    let profile_count = profile::Entity::find()
        .filter(profile::Column::Owner.eq(owner))
        .filter(profile::Column::CollectionName.eq(row.name.clone()))
        .count(db)
        .await
        .map_err(db::map_db_err)?;
    Ok(UserCollectionView {
        name: row.name,
        description: row.description,
        sets: sets.into_iter().map(|s| s.name).collect(),
        default_set: row.default_set_name,
        profile_count,
        set_count,
    })
}

/// Which face a collection cascade runs for: the audit shapes and
/// entity spellings follow the face the delete came through (the
/// user face's `set` over the user view, the admin face's
/// `profile_set` over the state json).
#[derive(Copy, Clone, Debug)]
pub(crate) enum CascadeFace {
    User,
    Admin,
}

impl CascadeFace {
    fn set_entity(self) -> &'static str {
        match self {
            CascadeFace::User => "set",
            CascadeFace::Admin => "profile_set",
        }
    }
}

/// Take the default-set anchor when the collection has none: the
/// conditional update makes concurrent creators race-safe (a loser
/// finds the slot taken and moves on).
pub(crate) async fn anchor_collection_default<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    collection: &str,
    set_name: &str,
) -> Result<(), ApiError> {
    collection::Entity::update_many()
        .col_expr(
            collection::Column::DefaultSetName,
            Expr::value(set_name.to_owned()),
        )
        .filter(collection::Column::Owner.eq(owner))
        .filter(collection::Column::Name.eq(collection))
        .filter(collection::Column::DefaultSetName.is_null())
        .exec(txn)
        .await
        .map_err(db::map_db_err)?;
    Ok(())
}

/// Refuse to delete a set that carries a collection's default anchor:
/// a dangling default breaks the collection's default-fallback
/// resolution. The anchor is per collection, so the guard names the
/// collection the set belongs to. `face` names the transfer path in the
/// message.
pub(crate) async fn collection_default_guard<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    collection_name: &str,
    set_name: &str,
    face: &str,
) -> Result<(), ApiError> {
    let anchored = collection::Entity::find()
        .filter(collection::Column::Owner.eq(owner))
        .filter(collection::Column::Name.eq(collection_name))
        .filter(collection::Column::DefaultSetName.eq(set_name))
        .one(txn)
        .await
        .map_err(db::map_db_err)?;
    if let Some(row) = anchored {
        return Err(ApiError::conflict(format!(
            "set is the default set of collection {}; PATCH /{face}/collections/{}/default to another set first",
            row.name, row.name
        )));
    }
    Ok(())
}

/// Delete a collection's anchored sets and the profiles the collection
/// holds, auditing every deleted row. Collection isolation makes the sweep
/// exact: a profile belongs to exactly one collection and membership never
/// leaves it, so deleting the collection takes every profile it holds --
/// members and unassigned rows alike -- and nothing outside it. The member rows
/// cascade with the set and profile rows; each deletion lands its own audit
/// event inside the caller's transaction. The collection row itself is the
/// caller's to delete (its audit shape is face-specific).
pub(crate) async fn delete_collection_cascade<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    name: &str,
    actor: &str,
    face: CascadeFace,
) -> Result<(), ApiError> {
    let sets = profile_set::Entity::find()
        .filter(profile_set::Column::Owner.eq(owner))
        .filter(profile_set::Column::CollectionName.eq(name))
        .order_by_asc(profile_set::Column::Name)
        .all(txn)
        .await
        .map_err(db::map_db_err)?;
    // Before-views while the rows exist: the member lists go with the set
    // rows (the FK cascade), so they are read first.
    let mut before_sets = Vec::with_capacity(sets.len());
    for set in &sets {
        let members: Vec<String> = set_member::Entity::find()
            .filter(set_member::Column::Owner.eq(owner))
            .filter(set_member::Column::CollectionName.eq(name))
            .filter(set_member::Column::SetName.eq(&set.name))
            .order_by_asc(set_member::Column::Position)
            .all(txn)
            .await
            .map_err(db::map_db_err)?
            .into_iter()
            .map(|m| m.profile_id)
            .collect();
        let view = match face {
            CascadeFace::User => serde_json::to_value(UserSetView {
                name: set.name.clone(),
                collection_name: set.collection_name.clone(),
                description: set.description.clone(),
                members,
            })
            .expect("view serializes"),
            CascadeFace::Admin => serde_json::json!({
                "name": set.name.clone(),
                "collection_name": set.collection_name.clone(),
                "description": set.description.clone(),
                "owner": set.owner.clone(),
                "members": members,
            }),
        };
        before_sets.push(view);
    }
    // The profiles the collection holds, with their before-views, read
    // while the rows are still there.
    let profiles = profile::Entity::find()
        .filter(profile::Column::Owner.eq(owner))
        .filter(profile::Column::CollectionName.eq(name))
        .order_by_asc(profile::Column::ProfileId)
        .all(txn)
        .await
        .map_err(db::map_db_err)?;
    // The module's lock order reaches here too: profile rows first, taken
    // in id order (the read above is ordered by id), before the cascade
    // below takes the member rows with its set rows. The views below only
    // read, so locking ahead of them costs nothing.
    for row in &profiles {
        registry::profile_by_id_for_update(txn, owner, name, &row.profile_id)
            .await
            .map_err(db::map_db_err)?;
    }
    let mut profile_befores = Vec::with_capacity(profiles.len());
    for row in profiles {
        let profile_id = row.profile_id.clone();
        let before = match face {
            CascadeFace::User => {
                serde_json::to_value(user_profile_view(txn, row).await?).expect("view serializes")
            }
            CascadeFace::Admin => {
                let served = registry::served_profile(txn, row)
                    .await
                    .map_err(db::map_db_err)?
                    .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
                serde_json::to_value(crate::management::sets::AdminProfile::from(served))
                    .expect("view serializes")
            }
        };
        profile_befores.push((profile_id, before));
    }
    profile_set::Entity::delete_many()
        .filter(profile_set::Column::Owner.eq(owner))
        .filter(profile_set::Column::CollectionName.eq(name))
        .exec(txn)
        .await
        .map_err(db::map_db_err)?;
    for (set, before) in sets.iter().zip(&before_sets) {
        management_event::record(
            txn,
            ManagementEventRow {
                action: "delete",
                entity: face.set_entity(),
                entity_id: set.name.clone(),
                actor: std::borrow::Cow::Owned(actor.to_owned()),
                before: Some(before.clone()),
                after: None,
            },
        )
        .await
        .map_err(db::map_db_err)?;
    }
    profile::Entity::delete_many()
        .filter(profile::Column::Owner.eq(owner))
        .filter(profile::Column::CollectionName.eq(name))
        .exec(txn)
        .await
        .map_err(db::map_db_err)?;
    for (profile_id, before) in profile_befores {
        management_event::record(
            txn,
            ManagementEventRow {
                action: "delete",
                entity: "profile",
                entity_id: profile_id,
                actor: std::borrow::Cow::Owned(actor.to_owned()),
                before: Some(before),
                after: None,
            },
        )
        .await
        .map_err(db::map_db_err)?;
    }
    Ok(())
}
