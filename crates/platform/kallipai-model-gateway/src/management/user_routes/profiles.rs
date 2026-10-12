//! The `/user/profiles` face: the caller's configurations and the wire
//! shapes they read and write.

use super::shared::owner_of;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::UserSession;
use crate::management::{validate_modalities, validate_profile_id};
use crate::registry::{self, profile};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserProfileView {
    pub profile_id: String,
    pub provider_id: String,
    pub family: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
    pub parked: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserProfileViews {
    pub profiles: Vec<UserProfileView>,
}

/// POST /user/profiles body: a configuration referencing a provider in
/// the caller's own space. New profiles are parked; the user domain has
/// no promote step (serving reads stay a platform concern).
#[derive(Deserialize)]
pub(super) struct UserProfileCreate {
    pub profile_id: String,
    pub provider_id: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
}

/// PUT /user/profiles/{profile_id}: full-field replacement (plain PUT
/// semantics; `parked` is not caller-settable here).
#[derive(Deserialize)]
pub(super) struct UserProfilePut {
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
}

/// GET /user/profiles: the caller's configurations.
pub(super) async fn list_profiles(
    State(state): State<AppState>,
    usr: UserSession,
) -> Result<Json<UserProfileViews>, ApiError> {
    let owner = owner_of(&usr);
    let rows = profile::Entity::find()
        .filter(profile::Column::Owner.eq(owner))
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

/// POST /user/profiles: a configuration referencing a provider in the
/// caller's own space (the composite FK refuses a cross-space provider
/// at the store; the explicit check answers a missing one with 404).
/// New profiles are parked; the user domain has no promote step.
pub(super) async fn create_profile(
    State(state): State<AppState>,
    usr: UserSession,
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
    if registry::profile_by_id(&txn, &owner, &body.profile_id)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict("profile already exists"));
    }
    registry::provider_by_id(&txn, &owner, &body.provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {}", body.provider_id)))?;
    profile::ActiveModel {
        profile_id: Set(body.profile_id.clone()),
        provider_id: Set(body.provider_id.clone()),
        model: Set(body.model.clone()),
        max_context_window: Set(body.max_context_window),
        effort: Set(body.effort.clone()),
        modalities: Set(crate::management::encode_modalities(&body.modalities)),
        parked: Set(true),
        store: Set(None),
        owner: Set(owner.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    let row = registry::profile_by_id(&txn, &owner, &body.profile_id)
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

/// PUT /user/profiles/{profile_id}: full-field replacement.
pub(super) async fn update_profile(
    State(state): State<AppState>,
    usr: UserSession,
    Path(profile_id): Path<String>,
    Json(body): Json<UserProfilePut>,
) -> Result<Json<UserProfileView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    if body.model.is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    validate_modalities(&body.modalities)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::profile_by_id(&txn, &owner, &profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such profile: {profile_id}")))?;
    let before = user_profile_view(&txn, row.clone()).await?;
    let mut active = profile::ActiveModel::from(row);
    active.model = Set(body.model.clone());
    active.max_context_window = Set(body.max_context_window);
    active.effort = Set(body.effort.clone());
    active.modalities = Set(crate::management::encode_modalities(&body.modalities));
    let updated = active.update(&txn).await.map_err(db::map_db_err)?;
    let after = user_profile_view(&txn, updated).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "profile",
            entity_id: profile_id.clone(),
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

/// DELETE /user/profiles/{profile_id}: the member rows cascade with it.
pub(super) async fn delete_profile(
    State(state): State<AppState>,
    usr: UserSession,
    Path(profile_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::profile_by_id(&txn, &owner, &profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such profile: {profile_id}")))?;
    let before = user_profile_view(&txn, row).await?;
    profile::Entity::delete_by_id((owner.clone(), profile_id.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "profile",
            entity_id: profile_id.clone(),
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

fn decode_modalities(raw: &Option<String>) -> Option<Vec<String>> {
    raw.as_ref()
        // the column is written by this face's encode; degrade rather than
        // panic the serving path if anything unexpected is stored
        .and_then(|s| serde_json::from_str(s).ok())
}

/// The user view of one profile: the provider's wire family rides along
/// (the family left the profile row when it moved to the pool).
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
        provider_id: row.provider_id,
        family,
        model: row.model,
        max_context_window: row.max_context_window,
        effort: row.effort,
        modalities: decode_modalities(&row.modalities),
        parked: row.parked,
    })
}
