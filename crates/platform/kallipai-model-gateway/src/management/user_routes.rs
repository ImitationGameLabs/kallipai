//! The user domain: the /user face behind the session cookie
//! ([`UserSession`]). Ownership is the session's account id, so the
//! routes take no owner parameter -- the owner is always the caller,
//! and the composite foreign keys keep every reference inside that one
//! space. Writes audit into `management_events` with the account id as
//! the actor (one spelling with the admin face).

use axum::Json;
use axum::extract::{Path, State};
use axum::routing::{delete, get, patch, post, put};
use sea_orm::sea_query::{Expr, Query};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};

use super::deserialize_explicit_option;
use super::{
    SecretPut, mask_key, validate_family, validate_modalities, validate_profile_id,
    validate_set_name,
};
use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::distribution::visibility;
use crate::management::UserSession;
use crate::registry::{
    self, collection, collection_publication, group, group_member, profile, profile_set, provider,
    set_member,
};
use crate::secret::provider_credential;
use crate::state::AppState;
use kallipai_common::protocol::ApiError;

use crate::registry::EVERYONE_GROUP_ID;

// -- wire shapes -------------------------------------------------------------

/// The user view of one pool provider: identity and shape, the key only
/// as its mask. The owner is the caller and is not echoed.
#[derive(Clone, Debug, Serialize)]
pub struct UserProviderView {
    pub provider_id: String,
    pub family: String,
    pub base_url: Option<String>,
    pub api_key_masked: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserProviderViews {
    pub providers: Vec<UserProviderView>,
}

/// POST /user/providers body: the pool row's identity and shape, plus
/// the optional seed credential (stored, never echoed back).
#[derive(Deserialize)]
pub struct UserProviderCreate {
    pub provider_id: String,
    pub family: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

/// PATCH /user/providers/{provider_id} body. A missing field leaves the
/// field unchanged; `base_url: null` clears the URL (the explicit
/// option), a present `api_key` writes (or rewrites) the credential.
#[derive(Deserialize)]
pub struct UserProviderUpdate {
    pub family: Option<String>,
    #[serde(default, deserialize_with = "deserialize_explicit_option")]
    pub base_url: Option<Option<String>>,
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserProfileView {
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
pub struct UserProfileViews {
    pub profiles: Vec<UserProfileView>,
}

/// POST /user/profiles body: a configuration referencing a provider in
/// the caller's own space. New profiles are parked; the user domain has
/// no promote step (serving reads stay a platform concern).
#[derive(Deserialize)]
pub struct UserProfileCreate {
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
pub struct UserProfilePut {
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserSetView {
    pub name: String,
    pub description: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserSetViews {
    pub sets: Vec<UserSetView>,
}

/// PATCH /user/sets/{name}: `description` and `members` are independent
/// (absent means unchanged; a present member list replaces the whole
/// membership).
#[derive(Deserialize)]
pub struct UserSetUpdate {
    pub description: Option<String>,
    pub members: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserCollectionView {
    pub name: String,
    pub description: String,
    pub sets: Vec<String>,
    /// The collection's default-set anchor (null = none chosen yet).
    pub default_set: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserCollectionViews {
    pub collections: Vec<UserCollectionView>,
}

/// POST /user/collections body: a fresh collection starts empty (sets
/// join only through the nested creation door).
#[derive(Deserialize)]
pub struct UserCollectionCreate {
    pub name: String,
    pub description: String,
}

/// PATCH /user/collections/{name}: `description` only (absent means
/// unchanged); the set membership is not a collection attribute.
/// Set membership changes only by creating or deleting a set.
#[derive(Deserialize)]
pub struct UserCollectionUpdate {
    pub description: Option<String>,
}

/// POST /user/collections/{name}/sets body: a set created directly
/// inside the named collection (the collection anchors the set).
#[derive(Deserialize)]
pub struct UserCollectionSetCreate {
    pub name: String,
    pub description: String,
    pub members: Option<Vec<String>>,
}

/// PATCH /user/collections/{name}/default body: the member set that
/// takes over as the collection's default.
#[derive(Deserialize)]
pub struct UserCollectionDefaultBody {
    pub set_name: String,
}

/// POST /user/collections/{name}/publications body: the audience group
/// the collection becomes visible to.
#[derive(Deserialize)]
pub struct UserPublicationBody {
    pub audience: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserGroupView {
    pub group_id: String,
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserGroupViews {
    pub groups: Vec<UserGroupView>,
}

/// POST /user/groups body: a member-managed group in the caller's space.
#[derive(Deserialize)]
pub struct UserGroupCreate {
    pub name: String,
    pub members: Option<Vec<String>>,
}

/// PATCH /user/groups/{group_id}: `name` and `members` are independent
/// (absent means unchanged; a present member list replaces the whole
/// membership).
#[derive(Deserialize)]
pub struct UserGroupUpdate {
    pub name: Option<String>,
    pub members: Option<Vec<String>>,
}

/// GET /user/platform-collections: one published platform collection
/// the caller's platform reach carries.
#[derive(Clone, Debug, Serialize)]
pub struct UserPlatformCollectionView {
    pub owner: String,
    pub name: String,
    pub description: String,
    pub sets: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserPlatformCollectionViews {
    pub collections: Vec<UserPlatformCollectionView>,
}

// -- helpers -----------------------------------------------------------------

/// The caller's ownership key: the verified session's account id, the
/// same spelling the audit rows carry for the consuming account.
fn owner_of(usr: &UserSession) -> String {
    usr.session.user_id.to_string()
}

fn decode_modalities(raw: &Option<String>) -> Option<Vec<String>> {
    raw.as_ref()
        // the column is written by this face's encode; degrade rather than
        // panic the serving path if anything unexpected is stored
        .and_then(|s| serde_json::from_str(s).ok())
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

// -- providers ---------------------------------------------------------------

/// GET /user/providers: the caller's pool rows, registration order.
pub async fn list_providers(
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
pub async fn create_provider(
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
pub async fn update_provider(
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
pub async fn delete_provider(
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
pub async fn put_provider_credential(
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
pub async fn delete_provider_credential(
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

// -- profiles ----------------------------------------------------------------

/// GET /user/profiles: the caller's configurations.
pub async fn list_profiles(
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
pub async fn create_profile(
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
pub async fn update_profile(
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
pub async fn delete_profile(
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

/// The user view of one profile: the provider's wire family rides along
/// (the family left the profile row when it moved to the pool).
async fn user_profile_view<C: sea_orm::ConnectionTrait>(
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

// -- sets --------------------------------------------------------------------

/// GET /user/sets: the caller's configuration sets with their ordered
/// members.
pub async fn list_sets(
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
pub async fn get_set(
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
pub async fn update_set(
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
pub async fn delete_set(
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
async fn replace_set_members<C: sea_orm::ConnectionTrait>(
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

async fn user_set_view<C: sea_orm::ConnectionTrait>(
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

// -- collections -------------------------------------------------------------

/// GET /user/collections: the caller's collections with their (unordered)
/// set members.
pub async fn list_collections(
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
pub async fn get_collection(
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
pub async fn create_collection(
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
pub async fn update_collection(
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
pub async fn delete_collection(
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
pub async fn publish_collection(
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
pub async fn unpublish_collection(
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

/// POST /user/collections/{name}/sets: a set created directly inside the
/// collection -- the row carries the collection in `collection_name`,
/// the membership authority. The first set in a default-less
/// collection takes the default anchor.
pub async fn create_collection_set(
    State(state): State<AppState>,
    usr: UserSession,
    Path(name): Path<String>,
    Json(body): Json<UserCollectionSetCreate>,
) -> Result<Json<UserSetView>, ApiError> {
    let owner = owner_of(&usr);
    let actor = std::borrow::Cow::Owned(owner.clone());
    validate_set_name(&body.name)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    if registry::set_by_id(&txn, &owner, &body.name)
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
        collection_name: Set(name.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    anchor_collection_default(&txn, &owner, &name, &body.name).await?;
    if let Some(members) = &body.members {
        replace_set_members(&txn, &owner, &body.name, members).await?;
    }
    let set = registry::set_by_id(&txn, &owner, &body.name)
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
pub async fn set_collection_default(
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
    let set = registry::set_by_id(&txn, &owner, &body.set_name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such set: {}", body.set_name)))?;
    if set.collection_name != name {
        return Err(ApiError::not_found(format!(
            "set {} is not a member of collection {name}",
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

// -- groups ------------------------------------------------------------------

/// GET /user/groups: the caller's groups with their members.
pub async fn list_groups(
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
pub async fn get_group(
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
pub async fn create_group(
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
pub async fn update_group(
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
pub async fn delete_group(
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
pub async fn list_platform_collections(
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
// -- the route table ---------------------------------------------------------

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        // -- providers ------------------------------------------------------
        .route("/providers", get(list_providers).post(create_provider))
        .route(
            "/providers/{provider_id}",
            patch(update_provider).delete(delete_provider),
        )
        .route(
            "/providers/{provider_id}/credential",
            put(put_provider_credential).delete(delete_provider_credential),
        )
        // -- profiles -------------------------------------------------------
        .route("/profiles", get(list_profiles).post(create_profile))
        .route(
            "/profiles/{profile_id}",
            put(update_profile).delete(delete_profile),
        )
        // -- sets -----------------------------------------------------------
        .route("/sets", get(list_sets))
        .route(
            "/sets/{name}",
            get(get_set).patch(update_set).delete(delete_set),
        )
        // -- collections ----------------------------------------------------
        .route(
            "/collections",
            get(list_collections).post(create_collection),
        )
        .route(
            "/collections/{name}",
            get(get_collection)
                .patch(update_collection)
                .delete(delete_collection),
        )
        .route("/collections/{name}/publications", post(publish_collection))
        .route(
            "/collections/{name}/publications/{group}",
            delete(unpublish_collection),
        )
        .route("/collections/{name}/sets", post(create_collection_set))
        .route("/collections/{name}/default", patch(set_collection_default))
        // -- groups ---------------------------------------------------------
        .route("/groups", get(list_groups).post(create_group))
        .route(
            "/groups/{group_id}",
            get(get_group).patch(update_group).delete(delete_group),
        )
        // -- the platform catalog --------------------------------------------
        .route("/platform-collections", get(list_platform_collections))
}

pub(crate) async fn user_collection_view<C: sea_orm::ConnectionTrait>(
    db: &C,
    owner: String,
    row: collection::Model,
) -> Result<UserCollectionView, ApiError> {
    let sets = profile_set::Entity::find()
        .filter(profile_set::Column::Owner.eq(owner))
        .filter(profile_set::Column::CollectionName.eq(row.name.clone()))
        .order_by_asc(profile_set::Column::Name)
        .all(db)
        .await
        .map_err(db::map_db_err)?;
    Ok(UserCollectionView {
        name: row.name,
        description: row.description,
        sets: sets.into_iter().map(|s| s.name).collect(),
        default_set: row.default_set_name,
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
/// resolution. `face` names the transfer path in the message.
pub(crate) async fn collection_default_guard<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    set_name: &str,
    face: &str,
) -> Result<(), ApiError> {
    let anchored = collection::Entity::find()
        .filter(collection::Column::Owner.eq(owner))
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

/// Delete a collection's anchored sets and the profiles the deletion
/// leaves without any set in this space (profiles a set another
/// collection still references survive). The member rows cascade with
/// the set rows; each deletion lands its
/// own audit event inside the caller's transaction. The collection
/// row itself is the caller's to delete (its audit shape is
/// face-specific).
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
    // Before-views while the rows exist: the member lists go with the
    // set rows (the FK cascade), so they are read first. The union of
    // member ids feeds the profile sweep below.
    let mut before_sets = Vec::with_capacity(sets.len());
    let mut member_union: Vec<String> = Vec::new();
    for set in &sets {
        let members: Vec<String> = set_member::Entity::find()
            .filter(set_member::Column::Owner.eq(owner))
            .filter(set_member::Column::SetName.eq(&set.name))
            .order_by_asc(set_member::Column::Position)
            .all(txn)
            .await
            .map_err(db::map_db_err)?
            .into_iter()
            .map(|m| m.profile_id)
            .collect();
        for id in &members {
            if !member_union.contains(id) {
                member_union.push(id.clone());
            }
        }
        let view = match face {
            CascadeFace::User => serde_json::to_value(UserSetView {
                name: set.name.clone(),
                description: set.description.clone(),
                members,
            })
            .expect("view serializes"),
            CascadeFace::Admin => serde_json::json!({
                "name": set.name.clone(),
                "description": set.description.clone(),
                "owner": set.owner.clone(),
                "members": members,
            }),
        };
        before_sets.push(view);
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
    // The sweep: a profile whose last set in this space went with the
    // collection has nothing left to serve; one still referenced
    // elsewhere stays.
    for profile_id in member_union {
        let remaining = set_member::Entity::find()
            .filter(set_member::Column::Owner.eq(owner))
            .filter(set_member::Column::ProfileId.eq(&profile_id))
            .count(txn)
            .await
            .map_err(db::map_db_err)?;
        if remaining > 0 {
            continue;
        }
        let Some(row) = registry::profile_by_id(txn, owner, &profile_id)
            .await
            .map_err(db::map_db_err)?
        else {
            continue;
        };
        let before = match face {
            CascadeFace::User => {
                serde_json::to_value(user_profile_view(txn, row).await?).expect("view serializes")
            }
            CascadeFace::Admin => {
                let served = registry::served_profile(txn, row)
                    .await
                    .map_err(db::map_db_err)?
                    .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
                serde_json::to_value(super::sets::AdminProfile::from(served))
                    .expect("view serializes")
            }
        };
        profile::Entity::delete_by_id((owner.to_owned(), profile_id.clone()))
            .exec(txn)
            .await
            .map_err(db::map_db_err)?;
        management_event::record(
            txn,
            ManagementEventRow {
                action: "delete",
                entity: "profile",
                entity_id: profile_id.clone(),
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

#[cfg(test)]
mod tests {
    use crate::management::testkit::*;
    use crate::test_support::TEST_USER_BEARER;
    use axum::http::StatusCode;

    /// The user domain's full arc on one seeded state: a provider with
    /// its credential, a profile referencing it, an ordered set, and a
    /// collection published to a group and unpublished again.
    #[tokio::test]
    async fn user_domain_crud_round_trip() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        // Provider with a seed credential.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/providers",
                user,
                true,
                Some(serde_json::json!({
                    "provider_id": "up1",
                    "family": "deepseek",
                    "base_url": "https://api.user.test",
                    "api_key": "sk-user-secret"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["provider_id"], "up1");
        // The plaintext is stored; the face only ever shows the mask.
        assert!(body.to_string().contains("***"));
        assert!(!body.to_string().contains("sk-user-secret"));
        // Profile referencing the provider (the family rides along).
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/profiles",
                user,
                true,
                Some(serde_json::json!({
                    "profile_id": "p-user",
                    "provider_id": "up1",
                    "model": "deepseek-chat"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["family"], "deepseek");
        // A profile pointing at a provider outside the space is 404.
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/profiles",
                user,
                true,
                Some(serde_json::json!({
                    "profile_id": "p-x",
                    "provider_id": "p1",
                    "model": "m"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // A scratch collection carries the set until the bundle takes it;
        // s0 holds its anchor so "mine" can travel.
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                user,
                true,
                Some(serde_json::json!({"name": "scratch", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections/scratch/sets",
                user,
                true,
                Some(serde_json::json!({"name": "s0", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Set with the profile as a member.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections/scratch/sets",
                user,
                true,
                Some(serde_json::json!({
                    "name": "mine",
                    "description": "d",
                    "members": ["p-user"]
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["members"], serde_json::json!(["p-user"]));
        // Group, collection, publish.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/groups",
                user,
                true,
                Some(serde_json::json!({"name": "team", "members": ["user-plain"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let group_id = body["group_id"].as_str().expect("group id").to_owned();
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                user,
                true,
                Some(serde_json::json!({
                    "name": "bundle",
                    "description": "d"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections/bundle/publications",
                user,
                true,
                Some(serde_json::json!({"audience": group_id})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // A duplicate publication conflicts.
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections/bundle/publications",
                user,
                true,
                Some(serde_json::json!({"audience": group_id})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // Unpublish withdraws the row.
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "DELETE",
                &format!("/user/collections/bundle/publications/{group_id}"),
                user,
                true,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Audit rows carry the account id as the actor.
        let rows = audit_rows(&state.db).await;
        let row = rows
            .iter()
            .find(|r| r.entity == "provider" && r.entity_id == "up1")
            .expect("provider audit row");
        assert_eq!(row.actor, "user-plain");
    }

    /// Ownership is the session: the user face shows only the caller's
    /// rows, and the admin face's catalog space shows none of them.
    #[tokio::test]
    async fn user_domain_owner_isolation() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/providers",
                user,
                true,
                Some(serde_json::json!({"provider_id": "up1", "family": "deepseek"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The user face sees exactly one row.
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/user/providers", user, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["providers"].as_array().expect("list").len(), 1);
        // The catalog space is unaffected (the seed rows are not the
        // user's): the admin provider list is the catalog-side witness.
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/admin/providers", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.to_string().contains("up1"));
        // Another owner's group id does not resolve on this face.
        let (status, _) = send(
            app(state.clone()),
            cookie_req("GET", "/user/groups/no-such-group", user, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The bearer channel is not a user credential: the families stay
    /// apart.
    #[tokio::test]
    async fn user_face_refuses_the_bearer_channel() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state.clone()),
            req("GET", "/user/providers", Some(TEST_USER_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// The user face fails closed with 503 when the auth backend is
    /// unreachable.
    #[tokio::test]
    async fn user_face_unreachable_backend_fails_closed_503() {
        let mut state = seeded_state().await;
        state.management = crate::management::AdminAuth::Platform(std::sync::Arc::new(
            crate::test_support::MockVerifier::unreachable(),
        ));
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "GET",
                "/user/providers",
                Some(TEST_USER_COOKIE),
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "auth_backend_unavailable");
    }
}
