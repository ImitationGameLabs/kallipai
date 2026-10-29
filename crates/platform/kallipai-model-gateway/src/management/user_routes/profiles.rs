//! The `/user/profiles` face: the caller's configurations and the wire
//! shapes they read and write. Every row lives in one of the caller's
//! collections, so the item routes name it and the list filter does.

use super::shared::owner_of;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::UserSession;
use crate::management::profiles::record_set_state_updates;
use crate::management::{
    deserialize_explicit_option, resolve_profile_for_update, set_state_required,
    validate_modalities, validate_profile_id,
};
use crate::registry::{self, collection, profile};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserProfileView {
    pub profile_id: String,
    pub collection_name: String,
    pub provider_id: String,
    pub family: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
    pub set: Option<String>,
    pub position: i32,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserProfileViews {
    pub profiles: Vec<UserProfileView>,
}

/// GET /user/profiles query: the collection to read (`None` = all of the
/// caller's collections).
/// The query shape is closed: any other key is a 400.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserProfileFilter {
    pub collection_name: Option<String>,
}

/// POST /user/collections/{collection}/profiles body: a configuration
/// referencing a provider in the caller's own space. A new row starts
/// unassigned and serves nothing until a set carries it; assigning one is
/// the PUT's `set` field.
#[derive(Deserialize)]
pub(super) struct UserProfileCreate {
    pub profile_id: String,
    pub provider_id: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
}

/// PUT /user/collections/{collection}/profiles/{profile_id}: full-field
/// replacement of the row's content, membership included. `set` is
/// three-state on the wire: a name joins that set (last place), `null`
/// leaves the row unassigned, and a body without the key is a 400.
#[derive(Deserialize)]
pub(super) struct UserProfilePut {
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_explicit_option")]
    pub set: Option<Option<String>>,
}

/// GET /user/profiles: the caller's configurations, optionally narrowed to
/// one collection.
pub(super) async fn list_profiles(
    State(state): State<AppState>,
    usr: UserSession,
    Query(filter): Query<UserProfileFilter>,
) -> Result<Json<UserProfileViews>, ApiError> {
    let owner = owner_of(&usr);
    let mut query = profile::Entity::find().filter(profile::Column::Owner.eq(owner));
    if let Some(collection_name) = filter.collection_name {
        query = query.filter(profile::Column::CollectionName.eq(collection_name));
    }
    let rows = query
        .order_by_asc(profile::Column::CollectionName)
        .order_by_asc(profile::Column::ProfileId)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        views.push(user_profile_view(&state.db, row).await?);
    }
    Ok(Json(UserProfileViews { profiles: views }))
}

/// POST /user/collections/{collection}/profiles: a configuration
/// referencing a provider in the caller's own space (the composite FK
/// refuses a cross-space provider at the store; the explicit check answers
/// a missing one with 404). The row is born unassigned; the PUT's `set`
/// field is what a set carries it into.
pub(super) async fn create_profile(
    State(state): State<AppState>,
    usr: UserSession,
    Path(collection): Path<String>,
    Json(body): Json<UserProfileCreate>,
) -> Result<Json<UserProfileView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    validate_profile_id(&body.profile_id)?;
    if body.model.is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    validate_modalities(&body.modalities)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    if collection::Entity::find_by_id((owner.clone(), collection.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .is_none()
    {
        return Err(ApiError::not_found(format!(
            "no such collection: {collection}"
        )));
    }
    if registry::profile_by_id(&txn, &owner, &collection, &body.profile_id)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict(
            "profile already exists in this collection",
        ));
    }
    registry::provider_by_id(&txn, &owner, &body.provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {}", body.provider_id)))?;
    profile::ActiveModel {
        profile_id: Set(body.profile_id.clone()),
        collection_name: Set(collection.clone()),
        provider_id: Set(body.provider_id.clone()),
        model: Set(body.model.clone()),
        max_context_window: Set(body.max_context_window),
        effort: Set(body.effort.clone()),
        modalities: Set(crate::management::encode_modalities(&body.modalities)),
        set_name: Set(None),
        position: Set(0),
        store: Set(None),
        owner: Set(owner.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    // Inserted by this transaction just above: no other request can remove
    // the row before this read, so the read cannot miss it.
    let row = registry::profile_by_id(&txn, &owner, &collection, &body.profile_id)
        .await
        .map_err(db::map_db_err)?
        .expect("the row was just inserted");
    let view = user_profile_view(&txn, row).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "profile",
            entity_id: body.profile_id.clone(),
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

/// PUT /user/collections/{collection}/profiles/{profile_id}: full-field
/// replacement of the row's content, membership included.
pub(super) async fn update_profile(
    State(state): State<AppState>,
    usr: UserSession,
    Path((collection, profile_id)): Path<(String, String)>,
    Json(body): Json<UserProfilePut>,
) -> Result<Json<UserProfileView>, ApiError> {
    let owner = owner_of(&usr);
    let actor: std::borrow::Cow<'static, str> = std::borrow::Cow::Owned(owner.clone());
    if body.model.is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    validate_modalities(&body.modalities)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    // The lock is the row itself: this write owns the row's `set_name` and
    // `position`, so an append to the target set serializes here (one write
    // order across both faces: the row first, its siblings second).
    let current = resolve_profile_for_update(&txn, &owner, &collection, &profile_id).await?;
    // A missing `set` key is not "leave it alone": a full replacement that
    // declines to say where the row lives has no answer, and both legal
    // forms are explicit (a set name, or null).
    let Some(set) = body.set else {
        return Err(ApiError::bad_request(
            "the set field is required: name the set this profile joins, or pass null to leave it unassigned",
        ));
    };
    // Joining a set appends to its end; staying put keeps the value it had.
    // `position` carries no uniqueness claim, and the read order breaks a
    // tie by `profile_id`.
    let position = match &set {
        Some(set_name) => {
            if registry::set_by_id(&txn, &owner, &collection, set_name)
                .await
                .map_err(db::map_db_err)?
                .is_none()
            {
                return Err(ApiError::not_found(format!(
                    "no such set: {set_name} in collection {collection}"
                )));
            }
            if current.set_name.as_deref() == Some(set_name.as_str()) {
                current.position
            } else {
                registry::set_next_position(&txn, &owner, &collection, set_name)
                    .await
                    .map_err(db::map_db_err)?
            }
        }
        None => current.position,
    };
    // A membership move reorders two sets: the one the row leaves and the
    // one it joins. Both before-states are captured here, under the row
    // lock and before the membership column moves; a same-set PUT reorders
    // nothing and lands no set row.
    let mut set_befores: Vec<(String, serde_json::Value)> = Vec::new();
    if current.set_name != set {
        for name in [current.set_name.as_deref(), set.as_deref()]
            .into_iter()
            .flatten()
        {
            let before_state = set_state_required(&txn, &owner, &collection, name).await?;
            set_befores.push((name.to_owned(), before_state));
        }
    }

    let before = user_profile_view(&txn, current.clone()).await?;
    let mut active = profile::ActiveModel::from(current);
    active.model = Set(body.model.clone());
    active.max_context_window = Set(body.max_context_window);
    active.effort = Set(body.effort.clone());
    active.modalities = Set(crate::management::encode_modalities(&body.modalities));
    active.set_name = Set(set);
    active.position = Set(position);
    let updated = active.update(&txn).await.map_err(db::map_db_err)?;
    let after = user_profile_view(&txn, updated).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "profile",
            entity_id: profile_id.clone(),
            actor: actor.clone(),
            before: Some(serde_json::to_value(&before).expect("view serializes")),
            after: Some(serde_json::to_value(&after).expect("view serializes")),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    // One row per set whose member order this write moved.
    record_set_state_updates(&txn, &owner, &collection, actor, set_befores).await?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(after))
}

/// DELETE /user/collections/{collection}/profiles/{profile_id}: the row
/// carries its membership with it (membership is the row).
pub(super) async fn delete_profile(
    State(state): State<AppState>,
    usr: UserSession,
    Path((collection, profile_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor: std::borrow::Cow<'static, str> = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::profile_by_id(&txn, &owner, &collection, &profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no such profile: {profile_id} in collection {collection}"
            ))
        })?;
    // The delete takes the row out of its set: that set's order is the
    // audit's before half, captured while the row is still here.
    let set_befores = match row.set_name.as_deref() {
        Some(set_name) => vec![(
            set_name.to_owned(),
            set_state_required(&txn, &owner, &collection, set_name).await?,
        )],
        None => Vec::new(),
    };

    let before = user_profile_view(&txn, row).await?;
    profile::Entity::delete_by_id((owner.clone(), collection.clone(), profile_id.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "profile",
            entity_id: profile_id.clone(),
            actor: actor.clone(),
            before: Some(serde_json::to_value(&before).expect("view serializes")),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    // The set keeps its own audit row: the order the row left.
    record_set_state_updates(&txn, &owner, &collection, actor, set_befores).await?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

fn decode_modalities(raw: &Option<String>) -> Option<Vec<String>> {
    raw.as_ref()
        // the column is written by this face's encode; degrade rather than
        // panic the serving path if anything unexpected is stored
        .and_then(|s| serde_json::from_str(s).ok())
}

/// The user view of one profile: the provider's wire family rides along
/// (the family is the provider row's, read through the provider the profile
/// points at), and `collection_name` names the container the row lives in.
pub(super) async fn user_profile_view<C: sea_orm::ConnectionTrait>(
    db: &C,
    row: profile::Model,
) -> Result<UserProfileView, ApiError> {
    let family = registry::provider_by_id(db, &row.owner, &row.provider_id)
        .await
        .map_err(db::map_db_err)?
        .map(|p| p.family)
        .unwrap_or_default();
    Ok(UserProfileView {
        profile_id: row.profile_id,
        collection_name: row.collection_name,
        provider_id: row.provider_id,
        family,
        model: row.model,
        max_context_window: row.max_context_window,
        effort: row.effort,
        modalities: decode_modalities(&row.modalities),
        set: row.set_name,
        position: row.position,
    })
}
