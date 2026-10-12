//! The `/user/providers` face: the caller's pool providers and their
//! credentials, with the wire shapes they read and write.

use super::shared::owner_of;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::UserSession;
use crate::management::deserialize_explicit_option;
use crate::management::{SecretPut, mask_key, validate_family, validate_profile_id};
use crate::registry::{self, profile, provider};
use crate::secret::provider_credential;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};

/// The user view of one pool provider: identity and shape, the key only
/// as its mask. The owner is the caller and is not echoed.
#[derive(Clone, Debug, Serialize)]
pub(super) struct UserProviderView {
    pub provider_id: String,
    pub family: String,
    pub base_url: Option<String>,
    pub api_key_masked: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct UserProviderViews {
    pub providers: Vec<UserProviderView>,
}

/// POST /user/providers body: the pool row's identity and shape, plus
/// the optional seed credential (stored, never echoed back).
#[derive(Deserialize)]
pub(super) struct UserProviderCreate {
    pub provider_id: String,
    pub family: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

/// PATCH /user/providers/{provider_id} body. A missing field leaves the
/// field unchanged; `base_url: null` clears the URL (the explicit
/// option), a present `api_key` writes (or rewrites) the credential.
#[derive(Deserialize)]
pub(super) struct UserProviderUpdate {
    pub family: Option<String>,
    #[serde(default, deserialize_with = "deserialize_explicit_option")]
    pub base_url: Option<Option<String>>,
    pub api_key: Option<String>,
}
fn user_provider_view(
    row: provider::Model,
    credential: Option<provider_credential::Model>,
) -> UserProviderView {
    UserProviderView {
        provider_id: row.provider_id,
        family: row.family,
        base_url: row.base_url,
        api_key_masked: credential.as_ref().map(|c| mask_key(&c.api_key)),
    }
}

/// The audit JSON for a user-owned provider: identity plus the mask.
fn provider_json(view: &UserProviderView) -> serde_json::Value {
    serde_json::json!({
        "provider_id": view.provider_id,
        "family": view.family,
        "base_url": view.base_url,
        "api_key_masked": view.api_key_masked,
    })
}

fn provider_row_json(row: &provider::Model) -> serde_json::Value {
    serde_json::json!({
        "provider_id": row.provider_id,
        "family": row.family,
        "base_url": row.base_url,
    })
}

/// Refuse an empty-string credential (a blank key would store a secret
/// that matches nothing; the field is either absent or real).
fn ensure_key_nonblank(api_key: &Option<String>) -> Result<(), ApiError> {
    if api_key.as_deref() == Some("") {
        return Err(ApiError::bad_request("api_key must not be empty"));
    }
    Ok(())
}

/// Drop a credential row if present (delete-by-id on a missing row is a
/// no-op for this face's purposes).
async fn drop_credential(
    owner: &str,
    provider_id: &str,
    txn: &sea_orm::DatabaseTransaction,
) -> Result<(), ApiError> {
    provider_credential::Entity::delete_by_id((owner.to_owned(), provider_id.to_owned()))
        .exec(txn)
        .await
        .map_err(db::map_db_err)?;
    Ok(())
}

/// GET /user/providers: the caller's pool rows, registration order.
pub(super) async fn list_providers(
    State(state): State<AppState>,
    usr: UserSession,
) -> Result<Json<UserProviderViews>, ApiError> {
    let owner = owner_of(&usr);
    let rows = provider::Entity::find()
        .filter(provider::Column::Owner.eq(owner.clone()))
        .order_by_asc(provider::Column::ProviderId)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        let credential =
            provider_credential::Entity::find_by_id((owner.clone(), row.provider_id.clone()))
                .one(&state.db)
                .await
                .map_err(db::map_db_err)?;
        views.push(user_provider_view(row, credential));
    }
    Ok(Json(UserProviderViews { providers: views }))
}

/// POST /user/providers: mint a pool row in the caller's space, with an
/// optional seed credential.
pub(super) async fn create_provider(
    State(state): State<AppState>,
    usr: UserSession,
    Json(body): Json<UserProviderCreate>,
) -> Result<Json<UserProviderView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    validate_profile_id(&body.provider_id)?;
    validate_family(&body.family)?;
    ensure_key_nonblank(&body.api_key)?;
    if let Some(url) = &body.base_url
        && reqwest::Url::parse(url).is_err()
    {
        return Err(ApiError::bad_request("base_url must be an absolute URL"));
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    if registry::provider_by_id(&txn, &owner, &body.provider_id)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict("provider already exists"));
    }
    provider::ActiveModel {
        owner: Set(owner.clone()),
        provider_id: Set(body.provider_id.clone()),
        family: Set(body.family.clone()),
        base_url: Set(body.base_url.clone()),
        ..Default::default()
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    if let Some(key) = &body.api_key {
        provider_credential::ActiveModel {
            owner: Set(owner.clone()),
            provider_id: Set(body.provider_id.clone()),
            api_key: Set(key.clone()),
        }
        .insert(&txn)
        .await
        .map_err(db::map_db_err)?;
    }
    let row = registry::provider_by_id(&txn, &owner, &body.provider_id)
        .await
        .map_err(db::map_db_err)?
        .expect("the row was just inserted");
    let credential =
        provider_credential::Entity::find_by_id((owner.clone(), body.provider_id.clone()))
            .one(&txn)
            .await
            .map_err(db::map_db_err)?;
    let view = user_provider_view(row, credential);
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "provider",
            entity_id: body.provider_id.clone(),
            actor,
            before: None,
            after: Some(provider_json(&view)),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(view))
}

/// PATCH /user/providers/{provider_id}: the same three-state contract as
/// the admin face (absent leaves unchanged, null clears the URL, a
/// present api_key writes or rewrites the credential).
pub(super) async fn update_provider(
    State(state): State<AppState>,
    usr: UserSession,
    Path(provider_id): Path<String>,
    Json(body): Json<UserProviderUpdate>,
) -> Result<Json<UserProviderView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    if let Some(Some(url)) = &body.base_url
        && reqwest::Url::parse(url).is_err()
    {
        return Err(ApiError::bad_request("base_url must be an absolute URL"));
    }
    ensure_key_nonblank(&body.api_key)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {provider_id}")))?;
    let credential_before =
        provider_credential::Entity::find_by_id((owner.clone(), provider_id.clone()))
            .one(&txn)
            .await
            .map_err(db::map_db_err)?;
    let before = user_provider_view(row.clone(), credential_before);
    let mut active = provider::ActiveModel::from(row);
    if let Some(family) = &body.family {
        validate_family(family)?;
        active.family = Set(family.clone());
    }
    if let Some(url) = &body.base_url {
        active.base_url = Set(url.clone());
    }
    active.update(&txn).await.map_err(db::map_db_err)?;
    if let Some(key) = &body.api_key {
        let cred = provider_credential::ActiveModel {
            owner: Set(owner.clone()),
            provider_id: Set(provider_id.clone()),
            api_key: Set(key.clone()),
        };
        let existing =
            provider_credential::Entity::find_by_id((owner.clone(), provider_id.clone()))
                .one(&txn)
                .await
                .map_err(db::map_db_err)?;
        match existing {
            Some(_) => cred.update(&txn).await.map_err(db::map_db_err)?,
            None => cred.insert(&txn).await.map_err(db::map_db_err)?,
        };
    }
    let updated = registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("provider row vanished mid-update"))?;
    let credential_after =
        provider_credential::Entity::find_by_id((owner.clone(), provider_id.clone()))
            .one(&txn)
            .await
            .map_err(db::map_db_err)?;
    let after = user_provider_view(updated, credential_after);
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "provider",
            entity_id: provider_id.clone(),
            actor,
            before: Some(provider_json(&before)),
            after: Some(provider_json(&after)),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(after))
}

/// DELETE /user/providers/{provider_id}: refused while a profile in the
/// caller's space still references the row; the credential cascade goes
/// with the row.
pub(super) async fn delete_provider(
    State(state): State<AppState>,
    usr: UserSession,
    Path(provider_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {provider_id}")))?;
    let sharers = profile::Entity::find()
        .filter(profile::Column::Owner.eq(owner.clone()))
        .filter(profile::Column::ProviderId.eq(provider_id.clone()))
        .count(&txn)
        .await
        .map_err(db::map_db_err)?;
    if sharers > 0 {
        return Err(ApiError::conflict(format!(
            "provider still serves {sharers} profiles; re-point them before deleting it"
        )));
    }
    let before = provider_row_json(&row);
    provider::Entity::delete_by_id((owner.clone(), provider_id.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "provider",
            entity_id: provider_id.clone(),
            actor,
            before: Some(before),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// PUT /user/providers/{provider_id}/credential: write or rewrite the
/// credential half; the plaintext never comes back. A provider with no
/// base URL takes the request's upstream URL onto the row.
pub(super) async fn put_provider_credential(
    State(state): State<AppState>,
    usr: UserSession,
    Path(provider_id): Path<String>,
    Json(body): Json<SecretPut>,
) -> Result<Json<UserProviderView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    if body.upstream_api_key.is_empty() {
        return Err(ApiError::bad_request("api_key must not be empty"));
    }
    if reqwest::Url::parse(&body.upstream_base_url).is_err() {
        return Err(ApiError::bad_request(
            "upstream_base_url must be an absolute URL",
        ));
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {provider_id}")))?;
    let masked = mask_key(&body.upstream_api_key);
    let existing = provider_credential::Entity::find_by_id((owner.clone(), provider_id.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?;
    let had_credential = existing.is_some();
    let cred = provider_credential::ActiveModel {
        owner: Set(owner.clone()),
        provider_id: Set(provider_id.clone()),
        api_key: Set(body.upstream_api_key.clone()),
    };
    if had_credential {
        cred.update(&txn).await.map_err(db::map_db_err)?;
    } else {
        cred.insert(&txn).await.map_err(db::map_db_err)?;
    }
    if row.base_url.is_none() {
        let mut active = provider::ActiveModel::from(row);
        active.base_url = Set(Some(body.upstream_base_url.clone()));
        active.update(&txn).await.map_err(db::map_db_err)?;
    }
    let action = if had_credential { "update" } else { "create" };
    management_event::record(
        &txn,
        ManagementEventRow {
            action,
            entity: "provider_credential",
            entity_id: provider_id.clone(),
            actor,
            before: existing.map(|c| serde_json::json!({ "api_key_masked": mask_key(&c.api_key) })),
            after: Some(serde_json::json!({ "api_key_masked": masked })),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    let (fresh, credential) = load_provider(&owner, &provider_id, &state).await?;
    Ok(Json(user_provider_view(fresh, credential)))
}

/// DELETE /user/providers/{provider_id}/credential: drop the stored
/// plaintext (the row itself stays).
pub(super) async fn delete_provider_credential(
    State(state): State<AppState>,
    usr: UserSession,
    Path(provider_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {provider_id}")))?;
    let existing = provider_credential::Entity::find_by_id((owner.clone(), provider_id.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?;
    drop_credential(&owner, &provider_id, &txn).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "provider_credential",
            entity_id: provider_id.clone(),
            actor,
            before: existing.map(|c| serde_json::json!({ "api_key_masked": mask_key(&c.api_key) })),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

async fn load_provider(
    owner: &str,
    provider_id: &str,
    state: &AppState,
) -> Result<(provider::Model, Option<provider_credential::Model>), ApiError> {
    let row = registry::provider_by_id(&state.db, owner, provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {provider_id}")))?;
    let credential =
        provider_credential::Entity::find_by_id((owner.to_owned(), provider_id.to_owned()))
            .one(&state.db)
            .await
            .map_err(db::map_db_err)?;
    Ok((row, credential))
}
