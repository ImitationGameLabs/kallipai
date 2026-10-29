//! The profile face: rows live inside their collection. A row's set is a
//! column (`set_name`): `NULL` is unassigned, a name is membership, and the
//! composite foreign key keeps the set inside the row's own collection.
//! Position follows the membership (`position`, ordered with `profile_id` as
//! the tie-break), so a set's failover order is one query over these rows.
//! The row identity is `(owner, collection_name, profile_id)`, so every route
//! names the collection it addresses and the cross-collection move is the one
//! operation that spans two of them.
//!
//! A row joins or leaves a set through this face's `set` field: the set face
//! carries no member list (its POST and PATCH take the description), and the
//! order inside a set is the set face's `/order` route.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::AdminToken;
use crate::management::providers;
use crate::management::sets::AdminProfile;
use crate::management::{
    Deleted, deserialize_explicit_option, encode_modalities, resolve_profile,
    resolve_profile_for_update, set_state_json, set_state_required, validate_modalities,
    validate_profile_id,
};
use crate::registry::{self, collection, profile, provider};
use crate::secret::provider_credential;
use crate::state::AppState;
use kallipai_common::protocol::ApiError;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Set, TransactionTrait,
};

#[derive(Deserialize)]
pub struct ProfileCreate {
    pub profile_id: String,
    pub provider_id: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
}

/// PUT /admin/collections/{collection}/profiles/{id}: full-field
/// replacement (plain PUT semantics -- the body is the complete desired
/// state). `set` is three-state on the wire: a name joins that set (last
/// place), `null` leaves the row unassigned, and a body without the key is a
/// 400 -- a full replacement has no reading for "unchanged", and this field
/// decides which set serves the row.
#[derive(Deserialize)]
pub struct ProfilePut {
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_explicit_option")]
    pub set: Option<Option<String>>,
}

/// GET /admin/profiles query: the collection to read (`None` = every
/// collection of the catalog space, the manage page's summary read) and the
/// unassigned filter (`None` = both states). `parked` is present only to
/// refuse: the state it asks about is the one `unassigned` asks about, so
/// a caller that sends it is told so instead of serde dropping the key and
/// the list answering every row.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileFilter {
    pub collection_name: Option<String>,
    pub unassigned: Option<bool>,
    #[serde(default)]
    pub parked: Option<bool>,
}

/// POST /admin/collections/{collection}/profiles: a row born in the named
/// collection. The provider it references lives in the same space (the
/// composite FK holds that), and the same id may already exist in another
/// collection -- only this collection's own row conflicts.
pub async fn create_profile(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(collection): Path<String>,
    Json(body): Json<ProfileCreate>,
) -> Result<Json<AdminProfile>, ApiError> {
    let actor = admin.operator.as_str();
    validate_profile_id(&body.profile_id)?;
    if body.model.is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    validate_modalities(&body.modalities)?;
    let owner = registry::CATALOG_OWNER.to_owned();
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
    registry::provider_by_id(&txn, registry::CATALOG_OWNER, &body.provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {}", body.provider_id)))?;
    if registry::profile_by_id(&txn, &owner, &collection, &body.profile_id)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict(
            "profile already exists in this collection",
        ));
    }
    let active = profile::ActiveModel {
        profile_id: Set(body.profile_id.clone()),
        collection_name: Set(collection.clone()),
        provider_id: Set(body.provider_id.clone()),
        model: Set(body.model.clone()),
        max_context_window: Set(body.max_context_window),
        effort: Set(body.effort.clone()),
        modalities: Set(encode_modalities(&body.modalities)),
        // Born unassigned: a set is a separate, explicit write, so the row
        // cannot arrive already serving a set nobody named.
        set_name: Set(None),
        position: Set(0),
        store: Set(None),
        owner: Set(owner),
    };
    let inserted = active.insert(&txn).await.map_err(db::map_db_err)?;
    let served = registry::served_profile(&txn, inserted)
        .await
        .map_err(db::map_db_err)?
        .expect("the referenced provider was checked above");
    let after = serde_json::to_value(AdminProfile::from(served.clone()))
        .expect("AdminProfile always serializes");
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "profile",
            entity_id: body.profile_id.clone(),
            actor,
            before: None,
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(AdminProfile::from(served)))
}

/// GET /admin/profiles: the catalog space's rows, filtered by collection
/// and membership state in the query (never after it). The rows carry
/// `collection_name`, so a cross-collection read still names each row's
/// home.
pub async fn list_profiles(
    State(state): State<AppState>,
    _admin: AdminToken,
    Query(filter): Query<ProfileFilter>,
) -> Result<Json<Vec<AdminProfile>>, ApiError> {
    // `parked` is refused rather than dropped: it asks the same state as
    // `unassigned`, and the refusal names both directions of that filter.
    if filter.parked.is_some() {
        return Err(ApiError::bad_request(
            "the parked filter is refused: `unassigned=true` selects the rows in no set, and `unassigned=false` selects the rows that belong to one",
        ));
    }
    let rows = registry::profiles_in_collection(
        &state.db,
        registry::CATALOG_OWNER,
        filter.collection_name.as_deref(),
        filter.unassigned,
    )
    .await
    .map_err(db::map_db_err)?;
    let served = registry::served_many(&state.db, rows)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(served.into_iter().map(AdminProfile::from).collect()))
}

/// GET /admin/collections/{collection}/profiles/{profile_id}: the row
/// read. Unassigned and served rows are the same resource here -- the set
/// membership is an attribute, and the list's `unassigned` filter is how a
/// caller asks for the rows that live in no set.
pub async fn get_profile(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path((collection, profile_id)): Path<(String, String)>,
) -> Result<Json<AdminProfile>, ApiError> {
    let owner = registry::CATALOG_OWNER.to_owned();
    let row = resolve_profile(&state.db, &owner, &collection, &profile_id).await?;
    let served = registry::served_profile(&state.db, row)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    Ok(Json(AdminProfile::from(served)))
}

/// PUT /admin/collections/{collection}/profiles/{profile_id}: full-field
/// replacement of the row's content, membership included. The identity
/// (space, collection, id) is not payload: the body replaces the content and
/// the row stays where it is -- a collection change is the move endpoint's
/// job.
pub async fn update_profile(
    State(state): State<AppState>,
    admin: AdminToken,
    Path((collection, profile_id)): Path<(String, String)>,
    Json(body): Json<ProfilePut>,
) -> Result<Json<AdminProfile>, ApiError> {
    let actor = admin.operator.as_str();
    if body.model.is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    validate_modalities(&body.modalities)?;
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    // The row lock is this face's gate: two writes to one profile queue here.
    // It does not serialize appends to one set (each profile has its own row),
    // so `position` claims no uniqueness and `profile_id` breaks a tie in reads.
    let current = resolve_profile_for_update(&txn, &owner, &collection, &profile_id).await?;
    // A missing `set` key is not "leave it alone": a full replacement that
    // declines to say where the row lives has no answer, and both legal forms
    // are explicit (a set name, or null).
    let Some(set) = body.set else {
        return Err(ApiError::bad_request(
            "the set field is required: name the set this profile joins, or pass null to leave it unassigned",
        ));
    };
    // The position is computed under the row lock: joining a set appends to
    // its end, staying put keeps the value it had. `position` carries no
    // uniqueness claim (two concurrent joins may land on the same number and
    // the read order breaks the tie by `profile_id`).
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

    // The management payloads do not carry the store tri-state: updates
    // preserve whatever the profile holds.
    let before_served = registry::served_profile(&txn, current.clone())
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    let prev_store = current.store;
    let before = serde_json::to_value(AdminProfile::from(before_served))
        .expect("AdminProfile always serializes");
    let updated = profile::ActiveModel {
        profile_id: Set(profile_id.clone()),
        collection_name: Set(current.collection_name.clone()),
        provider_id: Set(current.provider_id.clone()),
        model: Set(body.model.clone()),
        max_context_window: Set(body.max_context_window),
        effort: Set(body.effort.clone()),
        modalities: Set(encode_modalities(&body.modalities)),
        set_name: Set(set.clone()),
        position: Set(position),
        store: Set(prev_store),
        owner: Set(current.owner.clone()),
    }
    .update(&txn)
    .await
    .map_err(db::map_db_err)?;
    let served = registry::served_profile(&txn, updated)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    let after = serde_json::to_value(AdminProfile::from(served.clone()))
        .expect("AdminProfile always serializes");
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "profile",
            entity_id: profile_id.clone(),
            actor: actor.clone(),
            before: Some(before),
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    // One row per set whose member order this write moved.
    record_set_state_updates(&txn, &owner, &collection, actor, set_befores).await?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(AdminProfile::from(served)))
}

/// The before-state of the set this row belongs to, captured while the
/// membership column still names it: the audit's before half for a write that
/// takes the row out of its set (a delete, a move).
async fn source_set_befores<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    collection_name: &str,
    profile_id: &str,
) -> Result<Vec<(String, serde_json::Value)>, ApiError> {
    let set_name = registry::profile_by_id(txn, owner, collection_name, profile_id)
        .await
        .map_err(db::map_db_err)?
        .and_then(|row| row.set_name);
    let Some(set_name) = set_name else {
        return Ok(Vec::new());
    };
    let state = set_state_json(txn, owner, collection_name, &set_name)
        .await?
        .ok_or_else(|| {
            ApiError::internal("the profile names a set that is gone within this transaction")
        })?;
    Ok(vec![(set_name, state)])
}

/// One audit row per set whose member order the write changed: the order
/// the set held before, and the order left behind. Failover position is
/// exactly the fact nothing else records.
pub(super) async fn record_set_state_updates<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    collection_name: &str,
    actor: std::borrow::Cow<'static, str>,
    befores: Vec<(String, serde_json::Value)>,
) -> Result<(), ApiError> {
    for (set_name, before) in befores {
        let after = set_state_required(txn, owner, collection_name, &set_name).await?;
        management_event::record(
            txn,
            ManagementEventRow {
                action: "update",
                entity: "profile_set",
                entity_id: set_name,
                actor: actor.clone(),
                before: Some(before),
                after: Some(after),
            },
        )
        .await
        .map_err(db::map_db_err)?;
    }
    Ok(())
}

/// DELETE /admin/collections/{collection}/profiles/{profile_id}: the row,
/// its set membership (the column goes with the row), and -- when it was the
/// provider's last rider in the space -- the provider with its credential.
/// Every cascade lands its own audit row: the profile as the primary entry,
/// the credential as its masked row, the set the row left with its order
/// before and after.
pub async fn delete_profile(
    State(state): State<AppState>,
    admin: AdminToken,
    Path((collection, profile_id)): Path<(String, String)>,
) -> Result<Json<Deleted>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    // Locking read: the set's order and the row's membership are read under
    // one lock, so the audit's before half cannot disagree with the delete.
    let current = resolve_profile_for_update(&txn, &owner, &collection, &profile_id).await?;
    let before_served = registry::served_profile(&txn, current.clone())
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    let before = serde_json::to_value(AdminProfile::from(before_served))
        .expect("AdminProfile always serializes");
    let provider_row = registry::provider_by_id(&txn, &current.owner, &current.provider_id)
        .await
        .map_err(db::map_db_err)?;
    let credential = provider_credential::Entity::find_by_id((
        current.owner.clone(),
        current.provider_id.clone(),
    ))
    .one(&txn)
    .await
    .map_err(db::map_db_err)?;

    // Deleting the row takes a member out of its set: capture that set's
    // before-state first, while the member is still there.
    let member_befores =
        source_set_befores(&txn, &current.owner, &current.collection_name, &profile_id).await?;
    profile::Entity::delete_by_id((
        current.owner.clone(),
        current.collection_name.clone(),
        profile_id.clone(),
    ))
    .exec(&txn)
    .await
    .map_err(db::map_db_err)?;
    // The provider projection dies only with its last profile: the fold
    // shares one provider row across profiles, in any collection, so the
    // credential (which cascades with the provider row) outlives any
    // single rider.
    let co_riders = profile::Entity::find()
        .filter(profile::Column::Owner.eq(current.owner.clone()))
        .filter(profile::Column::ProviderId.eq(current.provider_id.clone()))
        .count(&txn)
        .await
        .map_err(db::map_db_err)?;
    let provider_gone = co_riders == 0;
    if provider_gone {
        provider::Entity::delete_by_id((current.owner.clone(), current.provider_id.clone()))
            .exec(&txn)
            .await
            .map_err(db::map_db_err)?;
    }
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "profile",
            entity_id: profile_id.clone(),
            actor: actor.clone(),
            before: Some(before),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    if provider_gone && let Some(cred) = credential {
        let base_url = provider_row
            .as_ref()
            .and_then(|p| p.base_url.clone())
            .unwrap_or_default();
        let view = providers::provider_credential_view(
            &current.owner,
            &current.provider_id,
            &base_url,
            &cred.api_key,
        );
        management_event::record(
            &txn,
            ManagementEventRow {
                action: "delete",
                entity: "provider_credential",
                entity_id: profile_id.clone(),
                actor: actor.clone(),
                before: Some(providers::provider_credential_audit_json(&view)),
                after: None,
            },
        )
        .await
        .map_err(db::map_db_err)?;
    }

    // One row per affected set: the member order the profile held before
    // the cascade, and the order left behind.
    record_set_state_updates(
        &txn,
        &current.owner,
        &current.collection_name,
        actor.clone(),
        member_befores,
    )
    .await?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(Deleted {
        deleted: true,
        warning: None,
    }))
}

/// POST /admin/collections/{collection}/profiles/{profile_id}/move body:
/// the source collection the row leaves and the set it joins in the
/// target collection (`None` = the copy lands there unassigned).
#[derive(Deserialize)]
pub struct MoveProfile {
    pub from_collection_name: String,
    pub set: Option<String>,
}

/// POST /admin/collections/{collection}/profiles/{profile_id}/move: move
/// one row into this collection. `{collection}` is the target; the body
/// names the source and, when the row should join a set on arrival, that
/// set (which must belong to the target). The copy keeps the row's id and
/// content, so a same-id row already sitting in the target is a 409.
pub async fn move_profile(
    State(state): State<AppState>,
    admin: AdminToken,
    Path((collection, profile_id)): Path<(String, String)>,
    Json(body): Json<MoveProfile>,
) -> Result<Json<AdminProfile>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    if body.from_collection_name == collection {
        return Err(ApiError::bad_request(
            "the row is already in the target collection; set membership is the set endpoint's job",
        ));
    }
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
    // The target set (when named) must live in the target collection: a
    // same-name set elsewhere is not a destination.
    if let Some(set_name) = &body.set
        && registry::set_by_id(&txn, &owner, &collection, set_name)
            .await
            .map_err(db::map_db_err)?
            .is_none()
    {
        return Err(ApiError::not_found(format!(
            "no such set: {set_name} in collection {collection}"
        )));
    }
    // The source row is taken locked: the position the copy takes is computed
    // and written inside this lock, and the row it reads is the one this move
    // removes.
    let current =
        resolve_profile_for_update(&txn, &owner, &body.from_collection_name, &profile_id).await?;
    if registry::profile_by_id(&txn, &owner, &collection, &profile_id)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict(
            "the target collection already holds a profile with this id",
        ));
    }
    // The target set's state before the row arrives (the audit's before half)
    // and the position the row takes: one past that set's last member.
    let target_set_befores = match &body.set {
        Some(set_name) => vec![(
            set_name.clone(),
            set_state_required(&txn, &owner, &collection, set_name).await?,
        )],
        None => Vec::new(),
    };
    let position = match &body.set {
        Some(set_name) => registry::set_next_position(&txn, &owner, &collection, set_name)
            .await
            .map_err(db::map_db_err)?,
        None => 0,
    };
    // The copy: content verbatim, the new container, and the arrival set
    // (null when it arrives unassigned).
    let copy = profile::ActiveModel {
        profile_id: Set(current.profile_id.clone()),
        collection_name: Set(collection.clone()),
        provider_id: Set(current.provider_id.clone()),
        model: Set(current.model.clone()),
        max_context_window: Set(current.max_context_window),
        effort: Set(current.effort.clone()),
        modalities: Set(current.modalities.clone()),
        set_name: Set(body.set.clone()),
        position: Set(position),
        store: Set(current.store),
        owner: Set(current.owner.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    let before_served = registry::served_profile(&txn, current.clone())
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    let source_before = serde_json::to_value(AdminProfile::from(before_served))
        .expect("AdminProfile always serializes");
    // The source row leaves its set, which loses a member exactly as it does
    // on a delete: capture that set's order first, land the audit row below.
    let source_befores =
        source_set_befores(&txn, &current.owner, &current.collection_name, &profile_id).await?;
    // The row itself goes; the set's other members keep their order.
    profile::Entity::delete_by_id((
        current.owner.clone(),
        current.collection_name.clone(),
        profile_id.clone(),
    ))
    .exec(&txn)
    .await
    .map_err(db::map_db_err)?;
    let served = registry::served_profile(&txn, copy)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    let target_after = serde_json::to_value(AdminProfile::from(served.clone()))
        .expect("AdminProfile always serializes");
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "profile",
            entity_id: profile_id.clone(),
            actor: actor.clone(),
            before: Some(source_before),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "profile",
            entity_id: profile_id.clone(),
            actor: actor.clone(),
            before: None,
            after: Some(target_after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    record_set_state_updates(
        &txn,
        &current.owner,
        &current.collection_name,
        actor.clone(),
        source_befores,
    )
    .await?;
    record_set_state_updates(&txn, &owner, &collection, actor, target_set_befores).await?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(AdminProfile::from(served)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::testkit::*;
    use axum::http::StatusCode;
    use sea_orm::{ConnectionTrait, EntityTrait, Statement};

    #[tokio::test]
    async fn unassigned_crud_joins_a_set_and_audits() {
        let state = seeded_state().await;
        catalog_provider(&state, "up1", "openai-compatible").await;
        let unassigned_body = serde_json::json!({
            "profile_id": "p9",
            "provider_id": "up1",
            "model": "m9",
            "modalities": ["text"],
            "max_context_window": 8192
        });
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(unassigned_body),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["set"], serde_json::Value::Null);
        assert_eq!(body["position"], 0);
        // The family rides the referenced provider row (the derived view).
        assert_eq!(body["family"], "openai-compatible");
        // Duplicate ids conflict; path-unsafe ids, unknown providers, and
        // non-wire modality entries all reject.
        for (payload, expected) in [
            (
                serde_json::json!({"profile_id": "p9", "provider_id": "up1", "model": "x"}),
                StatusCode::CONFLICT,
            ),
            (
                serde_json::json!({"profile_id": "p10", "provider_id": "missing", "model": "x"}),
                StatusCode::NOT_FOUND,
            ),
            (
                serde_json::json!({"profile_id": "p10", "provider_id": "up1", "model": "x", "modalities": ["smell"]}),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!({"profile_id": "a/b", "provider_id": "up1", "model": "x"}),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let (status, _) = send(
                app(state.clone()),
                req(
                    "POST",
                    "/admin/collections/baseline/profiles",
                    Some(TEST_ADMIN_BEARER),
                    Some(payload),
                ),
            )
            .await;
            assert_eq!(status, expected);
        }
        // Joining a set is the same PUT: the row keeps its identity and only
        // the membership moves, appending to the end of alpha's order.
        let join = serde_json::json!({
            "model": "m9",
            "modalities": ["text"],
            "max_context_window": 8192,
            "set": "alpha"
        });
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/p9",
                Some(TEST_ADMIN_BEARER),
                Some(join),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["set"], "alpha");
        assert_eq!(body["position"], 3);
        // The join moved the membership column, not the resource: the row
        // still reads, now as a member of alpha.
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/profiles/p9",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["set"], "alpha");
        // The unassigned listing is the set filter: p9 left it.
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/profiles?collection_name=baseline&unassigned=true",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let unassigned: Vec<&str> = body
            .as_array()
            .expect("unassigned array")
            .iter()
            .map(|p| p["profile_id"].as_str().expect("profile id"))
            .collect();
        assert!(
            !unassigned.contains(&"p9"),
            "a row in a set is not unassigned"
        );
        // The distribution unassigned list follows the selection pointer:
        // anchored to the catalog baseline, p4 alone sits in no set.
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/selection",
                Some(TEST_TAGMA_BEARER),
                Some(serde_json::json!({"owner": "system", "collection_name": "baseline"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/parking", Some(TEST_TAGMA_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let models = body["profiles"].as_array().expect("profiles array");
        // Only the seeded p4 unassigned row remains.
        assert_eq!(models.len(), 1);
        // The distribution payload is the sanitized shape (no profile_id;
        // the model name identifies the unassigned row).
        assert_eq!(models[0]["model"], "unassigned-x");
        // The audit trail now matches the name: one create (before=None,
        // after unassigned) and one join (set null -> alpha), per field.
        let rows = audit_rows(&state.db).await;
        let p9_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "profile" && r.entity_id == "p9")
            .collect();
        assert_eq!(p9_rows.len(), 2);
        assert_eq!(p9_rows[0].action, "create");
        assert!(p9_rows[0].before.is_none());
        assert_eq!(
            p9_rows[0].after.as_ref().expect("create after")["set"],
            serde_json::Value::Null
        );
        assert_eq!(p9_rows[1].action, "update");
        assert_eq!(
            p9_rows[1].before.as_ref().expect("join before")["set"],
            serde_json::Value::Null
        );
        assert_eq!(
            p9_rows[1].after.as_ref().expect("join after")["set"],
            "alpha"
        );
        assert_eq!(
            p9_rows[1].after.as_ref().expect("join after")["position"],
            3
        );
    }

    /// An unassigned row references a provider: the family derives from the
    /// referenced row and the provider table gains nothing.
    #[tokio::test]
    async fn create_unassigned_references_a_provider_and_derives_its_family() {
        let state = seeded_state().await;
        catalog_provider(&state, "up1", "deepseek").await;
        let before = provider::Entity::find()
            .all(&state.db)
            .await
            .expect("providers read")
            .len();
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "d1",
                    "provider_id": "up1",
                    "model": "m"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["provider_id"], "up1");
        assert_eq!(body["family"], "deepseek");
        let after = provider::Entity::find()
            .all(&state.db)
            .await
            .expect("providers read")
            .len();
        assert_eq!(after, before);
    }

    // The management UI's flow, regression-guarded: the provider dialog
    // and the profile dialog both send no `?owner=`, so the pool row
    // and the referencing unassigned row must land in one space (the catalog
    // space this face manages).
    #[tokio::test]
    async fn provider_and_unassigned_land_in_one_space_without_owner_params() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/providers",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "provider_id": "test1",
                    "family": "openai-compatible"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The pool row is in the catalog space, and the catalog-space
        // list sees it -- the dropdown and the save check read one domain.
        assert_eq!(
            registry::provider_by_id(&state.db, registry::CATALOG_OWNER, "test1")
                .await
                .expect("provider read")
                .expect("catalog-space provider row")
                .provider_id,
            "test1"
        );
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "d1",
                    "provider_id": "test1",
                    "model": "m"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["family"], "openai-compatible");
    }

    /// Deleting an unassigned row removes it from the domain's faces and
    /// lands one profile delete row; deleting a served profile (a row
    /// the admin space owns outright, planted with its own set membership)
    /// additionally lands the cascading credential's own masked delete row.
    /// The membership column goes with the row, but the set keeps its own
    /// audit row with the order it held before and after.
    #[tokio::test]
    async fn delete_unassigned_audits_the_cascade() {
        let state = seeded_state().await;
        // The whole walk stays in the catalog space: the unassigned rows, the
        // served row, its credential, and two members of one set.
        catalog_profile(&state, "a4", "unassigned-x", None).await;
        catalog_profile(&state, "a1", "m1", None).await;
        catalog_profile(&state, "a2", "m2", None).await;
        catalog_credential(&state, "a1", "sk-served-under-test").await;
        catalog_set(&state, "alpha2").await;
        catalog_member(&state, "alpha2", "a1", 0).await;
        catalog_member(&state, "alpha2", "a2", 1).await;
        // The planted unassigned row a4 has no credential: one audit row only.
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/collections/baseline/profiles/a4",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/profiles/a4",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // a1 is served, member of alpha2, and holds a credential: the
        // delete cascades all three facts.
        let (status, _) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/collections/baseline/profiles/a1",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The deleted row left the set... the catalog set face cannot see the
        // admin space, so the membership is asserted at the row level.
        let rows_left = state
            .db
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT profile_id FROM profiles \
                 WHERE collection_name = 'baseline' AND set_name = 'alpha2' ORDER BY position"
                    .to_owned(),
            ))
            .await
            .expect("profile rows read");
        let ids_left: Vec<String> = rows_left
            .iter()
            .map(|r| r.try_get::<String>("", "profile_id").expect("profile id"))
            .collect();
        assert_eq!(ids_left, vec!["a2".to_owned()]); // only a2 remains
        // ...and the profile item route answers 404 for the deleted id (the
        // distribution face never saw this private row).
        let (status, _) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/profiles/a1",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Audit: exactly two delete rows for a1 -- the profile and its
        // cascading credential (masked); the plaintext is in neither.
        let rows = audit_rows(&state.db).await;
        let p1_deletes: Vec<_> = rows
            .iter()
            .filter(|r| r.entity_id == "a1" && r.action == "delete")
            .collect();
        assert_eq!(p1_deletes.len(), 2);
        let profile_row = p1_deletes
            .iter()
            .find(|r| r.entity == "profile")
            .expect("profile delete audited");
        assert!(profile_row.before.is_some());
        assert!(profile_row.after.is_none());
        let cred_row = p1_deletes
            .iter()
            .find(|r| r.entity == "provider_credential")
            .expect("credential delete audited");
        let before = cred_row.before.as_ref().expect("before captured");
        assert!(
            before["api_key_masked"]
                .as_str()
                .expect("mask")
                .contains("***")
        );
        assert_eq!(before["owner"], registry::CATALOG_OWNER);
        assert_eq!(before["provider_id"], "a1");
        let raw = format!("{:?}", p1_deletes);
        assert!(!raw.contains("sk-served-under-test"));

        // The set keeps its own audit row: alpha2's order with
        // a1 at the head before, a2 alone after.
        let member_rows: Vec<_> = rows.iter().filter(|r| r.entity == "profile_set").collect();
        assert_eq!(member_rows.len(), 1);
        assert_eq!(member_rows[0].entity_id, "alpha2");
        assert_eq!(member_rows[0].action, "update");
        assert_eq!(
            member_rows[0].before.as_ref().expect("before")["members"],
            serde_json::json!(["a1", "a2"])
        );
        assert_eq!(
            member_rows[0].after.as_ref().expect("after")["members"],
            serde_json::json!(["a2"])
        );
    }

    /// The tri-state walk: a profile created without the store field has
    /// no stored value (unknown, not false), and a full-field PUT on top
    /// of a true column keeps it true. Both ends read the row itself: the
    /// column is not part of either payload, so the row is the witness.
    #[tokio::test]
    async fn store_tristate_carries_through_create_update_and_distribution() {
        let state = seeded_state().await;
        catalog_provider(&state, "up1", "openai-compatible").await;

        // Create with no store field in the payload: the column must
        // stay NULL, not be coerced to a boolean.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "sx1",
                    "provider_id": "up1",
                    "model": "m",
                    "modalities": ["text"],
                    "max_context_window": 8192
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // Join the seeded set through the same profile PUT: the row keeps
        // its identity, the fixture's members keep their places, and the row
        // appends to the end of the order.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/sx1",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "model": "m",
                    "max_context_window": 8192,
                    "modalities": ["text"],
                    "set": "alpha"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["set"], "alpha");

        let stored = state
            .db
            .query_one(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT store FROM profiles \
                 WHERE collection_name = 'baseline' AND profile_id = 'sx1'"
                    .to_owned(),
            ))
            .await
            .expect("store read")
            .expect("sx1 row");
        assert_eq!(
            stored
                .try_get::<Option<bool>>("", "store")
                .expect("store value"),
            None,
            "absent in the payload means unknown, never false"
        );

        // A fresh unassigned row carries the same guarantee: set the
        // column directly, then replace the full desired state through the
        // profile PUT (the `set` key included -- the body is the complete
        // desired state, and this row stays in no set). That moved row now
        // lives in the catalog space with its provider, so a second provider
        // hosts the new unassigned row.
        catalog_provider(&state, "up2", "openai-compatible").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "sx2",
                    "provider_id": "up2",
                    "model": "m",
                    "modalities": ["text"],
                    "max_context_window": 8192
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        state
            .db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE profiles SET store = TRUE \
                 WHERE collection_name = 'baseline' AND profile_id = 'sx2'"
                    .to_owned(),
            ))
            .await
            .expect("flip store");
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/sx2",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "model": "m",
                    "max_context_window": 8192,
                    "effort": null,
                    "modalities": ["text"],
                    "set": null
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // The update preserved store=true: the column is not part of
        // ProfilePut, so the replacement must not zero it. The unassigned row
        // never joins a set, so the witness is the row itself.
        let stored = state
            .db
            .query_one(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT store FROM profiles \
                 WHERE collection_name = 'baseline' AND profile_id = 'sx2'"
                    .to_owned(),
            ))
            .await
            .expect("store read")
            .expect("sx2 row");
        assert!(stored.try_get::<bool>("", "store").expect("store value"));
    }

    /// The unassigned listing is the set filter over the catalog space: a
    /// row created on this face and the seeded catalog row answer
    /// together, and a row in a set never joins them.
    #[tokio::test]
    async fn unassigned_filter_lists_the_rows_in_no_set() {
        let state = seeded_state().await;
        // The fixture's p4 is the catalog's seeded unassigned row.
        catalog_provider(&state, "up1", "deepseek").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "unassigned-2",
                    "provider_id": "up1",
                    "model": "unassigned-model"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/profiles?collection_name=baseline&unassigned=true",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ids: Vec<&str> = body
            .as_array()
            .expect("profiles array")
            .iter()
            .map(|p| p["profile_id"].as_str().expect("profile id"))
            .collect();
        assert!(
            ids.contains(&"p4"),
            "the seeded catalog unassigned row lists"
        );
        assert!(
            ids.contains(&"unassigned-2"),
            "the created unassigned row lists"
        );
    }
    /// This route takes no `parked` filter: a request that names it is
    /// refused rather than silently ignored, and the refusal points at the
    /// filter that answers the same question.
    #[tokio::test]
    async fn the_parked_filter_is_refused() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/profiles?collection_name=baseline&parked=true",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let message = body["error"]["message"].as_str().expect("message");
        for side in ["unassigned=true", "unassigned=false"] {
            assert!(
                message.contains(side),
                "the refusal names both directions of `unassigned`: {body}"
            );
        }
        // Each half keeps its direction: swapping them would leave both
        // names in place and both filters reading the wrong way.
        let assigned_half = message
            .split("`unassigned=false`")
            .nth(1)
            .expect("the refusal names the assigned half");
        // Cut at the next clause, so a swapped pair cannot leave the other
        // half's verb inside this slice.
        let assigned_clause = assigned_half
            .split('`')
            .next()
            .expect("a split yields one piece at least");
        assert!(
            assigned_clause.contains("belong"),
            "`unassigned=false` names the rows in a set: {body}"
        );
    }

    /// The unassigned filter answers both directions: `true` names the rows
    /// in no set, `false` names the rows that belong to one, and the two
    /// reads are disjoint halves of the unfiltered list.
    #[tokio::test]
    async fn the_unassigned_filter_answers_both_directions() {
        async fn listed(state: AppState, query: &str) -> Vec<String> {
            let (status, body) =
                send(app(state), req("GET", query, Some(TEST_ADMIN_BEARER), None)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body.as_array()
                .expect("profiles array")
                .iter()
                .map(|p| p["profile_id"].as_str().expect("profile id").to_string())
                .collect()
        }

        let state = seeded_state().await;
        catalog_provider(&state, "up1", "deepseek").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "member",
                    "provider_id": "up1",
                    "model": "member-model"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/member",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "model": "member-model",
                    "set": "alpha"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let whole = listed(state.clone(), "/admin/profiles?collection_name=baseline").await;
        let unassigned = listed(
            state.clone(),
            "/admin/profiles?collection_name=baseline&unassigned=true",
        )
        .await;
        let assigned = listed(
            state,
            "/admin/profiles?collection_name=baseline&unassigned=false",
        )
        .await;

        assert!(unassigned.contains(&"p4".to_string()), "{unassigned:?}");
        assert!(assigned.contains(&"member".to_string()), "{assigned:?}");
        assert!(
            !assigned.iter().any(|id| unassigned.contains(id)),
            "the two reads split the rows: {assigned:?} / {unassigned:?}"
        );
        let mut halves = assigned.clone();
        halves.extend(unassigned);
        halves.sort();
        let mut all = whole;
        all.sort();
        assert_eq!(halves, all, "the two halves cover the unfiltered list");
    }

    /// The same id in another collection is a different row: creation is
    /// scoped to its collection (only a duplicate there conflicts), and a
    /// write lands on the row its path names.
    #[tokio::test]
    async fn a_same_id_in_another_collection_is_a_different_row() {
        let state = seeded_state().await;
        seed_collection(&state, "system", "extra").await;
        catalog_provider(&state, "up1", "deepseek").await;
        let unassigned_body = serde_json::json!({
            "profile_id": "twin",
            "provider_id": "up1",
            "model": "m-a"
        });
        for collection in ["baseline", "extra"] {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "POST",
                    &format!("/admin/collections/{collection}/profiles"),
                    Some(TEST_ADMIN_BEARER),
                    Some(unassigned_body.clone()),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["collection_name"], collection);
        }
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(unassigned_body),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        // The full-field PUT addresses the row its path names: the twin in
        // the other collection keeps its content.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/twin",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "model": "m-b",
                    "modalities": ["text"],
                    "max_context_window": 8192,
                    "set": null
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["model"], "m-b");
        assert_eq!(body["collection_name"], "baseline");
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/extra/profiles/twin",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["model"], "m-a");
        assert_eq!(body["set"], serde_json::Value::Null);
    }

    /// Set membership is scoped to the row's own collection: a same-id
    /// row elsewhere stays in no set, and the audit row the join lands
    /// names the collection it touched.
    #[tokio::test]
    async fn a_membership_flip_touches_only_this_collections_row() {
        let state = seeded_state().await;
        seed_collection(&state, "system", "extra").await;
        catalog_provider(&state, "up1", "deepseek").await;
        let unassigned_body = serde_json::json!({
            "profile_id": "twin",
            "provider_id": "up1",
            "model": "m"
        });
        for collection in ["baseline", "extra"] {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "POST",
                    &format!("/admin/collections/{collection}/profiles"),
                    Some(TEST_ADMIN_BEARER),
                    Some(unassigned_body.clone()),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["set"], serde_json::Value::Null);
        }
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/twin",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "model": "m",
                    "modalities": ["text"],
                    "max_context_window": 8192,
                    "set": "alpha"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["set"], "alpha");
        assert_eq!(
            set_of(&state, "baseline", "twin").await.as_deref(),
            Some("alpha")
        );
        assert_eq!(set_of(&state, "extra", "twin").await, None);
        let rows = audit_rows(&state.db).await;
        let flips: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "profile" && r.entity_id == "twin" && r.action == "update")
            .collect();
        assert_eq!(flips.len(), 1, "one flip, for the set's own collection");
        let before = flips[0].before.as_ref().expect("flip before");
        let after = flips[0].after.as_ref().expect("flip after");
        assert_eq!(before["collection_name"], "baseline");
        assert_eq!(after["collection_name"], "baseline");
        assert_eq!(after["set"], "alpha");
    }

    /// The move endpoint re-anchors one row: the copy lands in the target
    /// collection unassigned (no set carries it), the source row leaves,
    /// and the trail records the pair.
    #[tokio::test]
    async fn move_lands_the_row_in_the_target_collection() {
        let state = seeded_state().await;
        seed_collection(&state, "system", "extra").await;
        catalog_provider(&state, "up1", "deepseek").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "mv1",
                    "provider_id": "up1",
                    "model": "m",
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The sets the row leaves: the movement has to record the order
        // each of them held, exactly as a delete does.
        catalog_set(&state, "origin").await;
        catalog_profile(&state, "mate", "m", None).await;
        catalog_member(&state, "origin", "mv1", 0).await;
        catalog_member(&state, "origin", "mate", 1).await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/extra/profiles/mv1/move",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"from_collection_name": "baseline"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["collection_name"], "extra");
        assert_eq!(body["set"], serde_json::Value::Null);
        // The source row left; the target row reads with its content.
        let (status, _) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/profiles/mv1",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/extra/profiles/mv1",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["model"], "m");
        // The trail: this row's create as unassigned, its delete, and the
        // landing create.
        let rows = audit_rows(&state.db).await;
        let mv1: Vec<_> = rows.iter().filter(|r| r.entity_id == "mv1").collect();
        assert_eq!(mv1.len(), 3);
        let landed = mv1
            .iter()
            .find(|r| {
                r.action == "create"
                    && r.after.as_ref().expect("create after")["collection_name"] == "extra"
            })
            .expect("the landing create");
        assert_eq!(
            landed.after.as_ref().expect("create after")["set"],
            serde_json::Value::Null
        );
        let left = mv1
            .iter()
            .find(|r| r.action == "delete")
            .expect("the source delete");
        assert_eq!(
            left.before.as_ref().expect("delete before")["collection_name"],
            "baseline"
        );
        // The cascade is the same fact as a delete, so the set it left
        // carries the audit pair: the order it held, and the order left.
        let order = rows
            .iter()
            .find(|r| r.entity == "profile_set" && r.entity_id == "origin" && r.action == "update")
            .expect("the source set's update");
        assert_eq!(
            order.before.as_ref().expect("update before")["members"],
            serde_json::json!(["mv1", "mate"]),
            "the order the set held before the move"
        );
        assert_eq!(
            order.after.as_ref().expect("update after")["members"],
            serde_json::json!(["mate"]),
            "the order the set is left with"
        );
    }

    /// A full replacement has to say where the row lives: a body without the
    /// `set` key is refused, and the refusal leaves the row untouched. Passing
    /// `null` instead takes the row out of its set (unassigned is a state, not
    /// a refusal).
    #[tokio::test]
    async fn a_put_without_the_set_key_is_refused() {
        let state = seeded_state().await;
        catalog_profile(&state, "pm1", "m", None).await;
        catalog_set(&state, "hold").await;
        catalog_member(&state, "hold", "pm1", 0).await;

        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/pm1",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "m2"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("the set field is required"),
            "{body}"
        );
        assert_eq!(
            set_of(&state, "baseline", "pm1").await.as_deref(),
            Some("hold"),
            "the refused write changed no column"
        );
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/profiles/pm1",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["model"], "m", "the model survived the refusal too");
        // An explicit null is the legal way out of the set.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/pm1",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "m2", "set": null})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["set"], serde_json::Value::Null);
        assert_eq!(set_of(&state, "baseline", "pm1").await, None);
    }

    /// A move into a set arrives served and takes the last position: the
    /// set's own face is what the distribution layer reads.
    #[tokio::test]
    async fn move_into_a_set_appends_the_member() {
        let state = seeded_state().await;
        seed_collection(&state, "system", "extra").await;
        catalog_provider(&state, "up1", "deepseek").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "mv2",
                    "provider_id": "up1",
                    "model": "m"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        catalog_set_in(&state, "extra", "landing").await;
        catalog_profile_in(&state, "extra", "seat", "m", None).await;
        catalog_member_in(&state, "extra", "landing", "seat", 0).await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/extra/profiles/mv2/move",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "from_collection_name": "baseline",
                    "set": "landing"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["collection_name"], "extra");
        assert_eq!(body["set"], "landing");
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/extra/sets/landing",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ids: Vec<&str> = body["profiles"]
            .as_array()
            .expect("profiles array")
            .iter()
            .map(|p| p["profile_id"].as_str().expect("profile id"))
            .collect();
        assert_eq!(ids, ["seat", "mv2"]);
        let rows = audit_rows(&state.db).await;
        let mut kinds: Vec<(&str, &str)> = rows
            .iter()
            .filter(|r| {
                r.entity_id == "mv2" || (r.entity == "profile_set" && r.entity_id == "landing")
            })
            .map(|r| (r.action.as_str(), r.entity.as_str()))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                ("create", "profile"),
                ("create", "profile"),
                ("delete", "profile"),
                ("update", "profile_set"),
            ]
        );
    }
    /// A membership move reorders two sets and lands one audit row per set:
    /// the join appends the row to the target's order, the leave drops it
    /// from the source's. A PUT that keeps the row where it is reorders
    /// nothing.
    #[tokio::test]
    async fn a_membership_move_audits_both_sets() {
        let state = seeded_state().await;
        // p4 is the seeded unassigned row; alpha holds p1, p2, p6.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/p4",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "unassigned-x", "set": "alpha"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // Same set: the order does not move, so no set row.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/p4",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "unassigned-x", "set": "alpha"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // Leaving: alpha loses p4 again.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/p4",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "unassigned-x", "set": null})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let rows = audit_rows(&state.db).await;
        let set_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "profile_set" && r.entity_id == "alpha")
            .collect();
        assert_eq!(set_rows.len(), 2);
        assert_eq!(
            set_rows[0].before.as_ref().expect("join before")["members"],
            serde_json::json!(["p1", "p2", "p6"])
        );
        assert_eq!(
            set_rows[0].after.as_ref().expect("join after")["members"],
            serde_json::json!(["p1", "p2", "p6", "p4"])
        );
        assert_eq!(
            set_rows[1].before.as_ref().expect("leave before")["members"],
            serde_json::json!(["p1", "p2", "p6", "p4"])
        );
        assert_eq!(
            set_rows[1].after.as_ref().expect("leave after")["members"],
            serde_json::json!(["p1", "p2", "p6"])
        );
        assert!(set_rows.iter().all(|r| r.actor == "admin"));
    }

    /// A move refuses a target it cannot honour: the same collection, a
    /// set that lives elsewhere, an occupied id, a missing source, and a
    /// missing target collection all leave every row where it was.
    #[tokio::test]
    async fn move_refuses_bad_targets() {
        let state = seeded_state().await;
        seed_collection(&state, "system", "extra").await;
        catalog_provider(&state, "up1", "deepseek").await;
        for (collection, id) in [("baseline", "mv3"), ("baseline", "twin"), ("extra", "twin")] {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "POST",
                    &format!("/admin/collections/{collection}/profiles"),
                    Some(TEST_ADMIN_BEARER),
                    Some(serde_json::json!({
                        "profile_id": id,
                        "provider_id": "up1",
                        "model": "m"
                    })),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        // The source is the target.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles/mv3/move",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"from_collection_name": "baseline"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        // A set of that name lives in the source collection only.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/extra/profiles/mv3/move",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "from_collection_name": "baseline",
                    "set": "alpha"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        // The target already holds the id.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/extra/profiles/twin/move",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"from_collection_name": "baseline"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        // No source row under the named id.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/extra/profiles/ghost/move",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"from_collection_name": "baseline"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        // No target collection.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/nope/profiles/mv3/move",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"from_collection_name": "baseline"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        // Nothing moved: the source rows still read where they were.
        for id in ["mv3", "twin"] {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "GET",
                    &format!("/admin/collections/baseline/profiles/{id}"),
                    Some(TEST_ADMIN_BEARER),
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }
}
