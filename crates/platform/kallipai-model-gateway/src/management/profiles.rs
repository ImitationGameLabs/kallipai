//! The profile face: rows live inside their collection. An unassigned
//! profile is a row with `parked = true` -- the park flag is a filter, not a
//! resource; promotion is a plain PUT and joining a set flips the flag in
//! the membership rewrite. The row identity is `(owner, collection_name,
//! profile_id)`, so every route names the collection it addresses and the
//! cross-collection move is the one operation that spans two of them.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::AdminToken;
use crate::management::providers;
use crate::management::sets::AdminProfile;
use crate::management::{
    Deleted, encode_modalities, resolve_profile, resolve_profile_for_update, set_state_json,
    validate_modalities, validate_profile_id,
};
use crate::registry::{self, collection, profile, provider, set_member};
use crate::secret::provider_credential;
use crate::state::AppState;
use kallipai_common::protocol::ApiError;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};

#[derive(Deserialize)]
pub struct ProfileCreate {
    pub profile_id: String,
    pub provider_id: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
    /// The state the row is born in; absent means unassigned (`true`).
    #[serde(default)]
    pub parked: Option<bool>,
}

/// PUT /admin/collections/{collection}/profiles/{id}: full-field
/// replacement (plain PUT semantics -- the body is the complete desired
/// state, `parked` included, so a promote to served and a demote back to
/// unassigned are the same endpoint). The one refusal is a demote the sets
/// forbid: a row a set carries is served, so `parked: true` on it is a 409
/// until its memberships are gone.
#[derive(Deserialize)]
pub struct ProfilePut {
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
    pub parked: bool,
}

/// GET /admin/profiles query: the collection to read (`None` = every
/// collection of the catalog space, the manage page's summary read) and
/// the park filter (`None` = both).
#[derive(Deserialize)]
pub struct ProfileFilter {
    pub collection_name: Option<String>,
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
        // Born unassigned unless the caller says otherwise: serving is an
        // explicit choice, and only a set membership serves the row.
        parked: Set(body.parked.unwrap_or(true)),
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
/// and park state in the query (never after it). The rows carry
/// `collection_name`, so a cross-collection read still names each row's
/// home.
pub async fn list_profiles(
    State(state): State<AppState>,
    _admin: AdminToken,
    Query(filter): Query<ProfileFilter>,
) -> Result<Json<Vec<AdminProfile>>, ApiError> {
    let rows = registry::profiles_in_collection(
        &state.db,
        registry::CATALOG_OWNER,
        filter.collection_name.as_deref(),
        filter.parked,
    )
    .await
    .map_err(db::map_db_err)?;
    let served = registry::served_many(&state.db, rows)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(served.into_iter().map(AdminProfile::from).collect()))
}

/// GET /admin/collections/{collection}/profiles/{profile_id}: the row
/// read. Unassigned and served rows are the same resource here -- the park
/// flag is an attribute, and the list's `parked` filter is how a caller
/// asks for the unassigned rows.
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
/// replacement of the row's content. The identity (space, collection, id)
/// is not payload: the body replaces the content and the row stays where
/// it is -- a collection change is the move endpoint's job.
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
    // The gate opens on a locking read: this write owns the flag, a set
    // rewrite flipping the same row owns the membership, and every writer
    // that touches both takes the profile row first, its member rows
    // after, never the reverse (see `resolve_profile_for_update`; the set
    // rewrite locks the members plus the ids leaving it, by id, ascending).
    // That is the module's one order and a new writer must keep it. The
    // check below therefore reads a committed membership. The one write
    // that can still slip past it is the user face's set edit: it rewrites
    // member rows without parking or locking a profile row, so a parked
    // member there stays parked. The faces then split on purpose: the
    // distribution face serves the selected collection without reading the
    // flag and `/parking` lists that member as unassigned, while the
    // forwarding face reads it and refuses the row (403,
    // `GateError::parking`).
    let current = resolve_profile_for_update(&txn, &owner, &collection, &profile_id).await?;
    // A row a set carries is served by definition: the flag follows the
    // memberships, so this write parks nothing a set still holds.
    if body.parked {
        let member = set_member::Entity::find()
            .filter(set_member::Column::Owner.eq(current.owner.clone()))
            .filter(set_member::Column::CollectionName.eq(current.collection_name.clone()))
            .filter(set_member::Column::ProfileId.eq(profile_id.clone()))
            .one(&txn)
            .await
            .map_err(db::map_db_err)?;
        if member.is_some() {
            return Err(ApiError::conflict(
                "a set member cannot be unassigned; remove it from its sets first",
            ));
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
        parked: Set(body.parked),
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
            actor,
            before: Some(before),
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(AdminProfile::from(served)))
}

/// The before-state of every set this row belongs to, captured while its
/// membership rows still stand: the audit's before half for a write that
/// removes them (a delete, a move).
async fn source_set_befores<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    collection_name: &str,
    profile_id: &str,
) -> Result<Vec<(String, serde_json::Value)>, ApiError> {
    let members = set_member::Entity::find()
        .filter(set_member::Column::Owner.eq(owner))
        .filter(set_member::Column::CollectionName.eq(collection_name))
        .filter(set_member::Column::ProfileId.eq(profile_id))
        .order_by_asc(set_member::Column::SetName)
        .all(txn)
        .await
        .map_err(db::map_db_err)?;
    let mut befores = Vec::with_capacity(members.len());
    for member in &members {
        let state = set_state_json(txn, owner, collection_name, &member.set_name)
            .await?
            .ok_or_else(|| {
                ApiError::internal("a member row outlived its set within this transaction")
            })?;
        befores.push((member.set_name.clone(), state));
    }
    Ok(befores)
}

/// One audit row per set whose member order the write changed: the order
/// the set held before, and the order left behind. Failover position is
/// exactly the fact nothing else records.
async fn record_set_state_updates<C: sea_orm::ConnectionTrait>(
    txn: &C,
    owner: &str,
    collection_name: &str,
    actor: std::borrow::Cow<'static, str>,
    befores: Vec<(String, serde_json::Value)>,
) -> Result<(), ApiError> {
    for (set_name, before) in befores {
        let after = set_state_json(txn, owner, collection_name, &set_name)
            .await?
            .expect("the set outlives its members within this transaction");
        management_event::record(
            txn,
            ManagementEventRow {
                action: "update",
                entity: "set_member",
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
/// its set memberships (FK cascade), and -- when it was the provider's
/// last rider in the space -- the provider with its credential. Every
/// cascade lands its own audit row: the profile as the primary entry, the
/// credential as its masked row, one row per affected set with the member
/// order before and after.
pub async fn delete_profile(
    State(state): State<AppState>,
    admin: AdminToken,
    Path((collection, profile_id)): Path<(String, String)>,
) -> Result<Json<Deleted>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    // Locking read: the delete takes the profile row before the member
    // rows it cascades -- the module's order.
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

    // The membership rows die with the profile (FK cascade): capture each
    // affected set's before-state first, while the members are still there.
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
    // The source row is taken locked, before the member rows this move
    // rewrites: the module's order.
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
    // The target set's state before the row arrives (the audit's before
    // half) and the position the row takes: one past the last member.
    let target_set_befores = match &body.set {
        Some(set_name) => vec![(
            set_name.clone(),
            set_state_json(&txn, &owner, &collection, set_name)
                .await?
                .expect("the target set was checked above"),
        )],
        None => Vec::new(),
    };
    let position = match &body.set {
        Some(set_name) => set_member::Entity::find()
            .filter(set_member::Column::Owner.eq(&owner))
            .filter(set_member::Column::CollectionName.eq(&collection))
            .filter(set_member::Column::SetName.eq(set_name))
            .order_by_desc(set_member::Column::Position)
            .one(&txn)
            .await
            .map_err(db::map_db_err)?
            .map(|m| m.position + 1)
            .unwrap_or(0),
        None => 0,
    };
    // The copy: content verbatim, the new container, and unassigned when it
    // arrives without a set (a member is served by definition).
    let copy = profile::ActiveModel {
        profile_id: Set(current.profile_id.clone()),
        collection_name: Set(collection.clone()),
        provider_id: Set(current.provider_id.clone()),
        model: Set(current.model.clone()),
        max_context_window: Set(current.max_context_window),
        effort: Set(current.effort.clone()),
        modalities: Set(current.modalities.clone()),
        parked: Set(body.set.is_none()),
        store: Set(current.store),
        owner: Set(current.owner.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    if let Some(set_name) = &body.set {
        set_member::ActiveModel {
            owner: Set(current.owner.clone()),
            collection_name: Set(collection.clone()),
            set_name: Set(set_name.clone()),
            profile_id: Set(profile_id.clone()),
            position: Set(position),
        }
        .insert(&txn)
        .await
        .map_err(db::map_db_err)?;
    }
    let before_served = registry::served_profile(&txn, current.clone())
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    let source_before = serde_json::to_value(AdminProfile::from(before_served))
        .expect("AdminProfile always serializes");
    // The source row's memberships cascade with it, so each source set
    // loses a member exactly as it does on a delete: capture every set's
    // order first, land one row per set below.
    let source_befores =
        source_set_befores(&txn, &current.owner, &current.collection_name, &profile_id).await?;
    // The source row leaves; its membership rows cascade with it.
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
    async fn unassigned_crud_promotes_and_audits() {
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
        assert_eq!(body["parked"], true);
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
        // Promote: parked=false through the same PUT.
        let promote = serde_json::json!({
            "profile_id": "p9",
            "model": "m9",
            "modalities": ["text"],
            "max_context_window": 8192,
            "parked": false
        });
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/p9",
                Some(TEST_ADMIN_BEARER),
                Some(promote),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["parked"], false);
        // The promote flipped the flag, not the resource: the row still
        // reads, as a served row.
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
        assert_eq!(body["parked"], false);
        // The unassigned listing is the park filter: p9 left it.
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
        assert_eq!(status, StatusCode::OK, "{body}");
        let unassigned: Vec<&str> = body
            .as_array()
            .expect("unassigned array")
            .iter()
            .map(|p| p["profile_id"].as_str().expect("profile id"))
            .collect();
        assert!(
            !unassigned.contains(&"p9"),
            "a promoted row is not unassigned"
        );
        // The distribution unassigned list follows the selection pointer:
        // anchored to the catalog baseline, p4 alone is parked there.
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
        // after parked) and one promote (parked true -> false), per field.
        let rows = audit_rows(&state.db).await;
        let p9_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "profile" && r.entity_id == "p9")
            .collect();
        assert_eq!(p9_rows.len(), 2);
        assert_eq!(p9_rows[0].action, "create");
        assert!(p9_rows[0].before.is_none());
        assert_eq!(
            p9_rows[0].after.as_ref().expect("create after")["parked"],
            true
        );
        assert_eq!(p9_rows[1].action, "update");
        assert_eq!(
            p9_rows[1].before.as_ref().expect("promote before")["parked"],
            true
        );
        assert_eq!(
            p9_rows[1].after.as_ref().expect("promote after")["parked"],
            false
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
    /// the admin space owns outright, planted with its own membership
    /// edge) additionally lands the cascading credential's own masked
    /// delete row and one row per affected set with its before/after
    /// member order (the membership edge leaves via the FK cascade, but
    /// the audit trail keeps it).
    #[tokio::test]
    async fn delete_unassigned_audits_the_cascade() {
        let state = seeded_state().await;
        // The whole walk stays in the catalog space: the unassigned rows, the
        // served row, its credential, and a set holding the membership
        // edge.
        catalog_profile(&state, "a4", "unassigned-x", true).await;
        catalog_profile(&state, "a1", "m1", false).await;
        catalog_profile(&state, "a2", "m2", false).await;
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
        // The set lost the member... the catalog set face cannot see the
        // admin space, so the membership is asserted at the row level.
        let member_rows_left = state
            .db
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT profile_id FROM set_members \
                 WHERE collection_name = 'baseline' AND set_name = 'alpha2' ORDER BY position"
                    .to_owned(),
            ))
            .await
            .expect("member rows read");
        let ids_left: Vec<String> = member_rows_left
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

        // The membership edge is audited per set: alpha2's order with
        // a1 at the head before, a2 alone after.
        let member_rows: Vec<_> = rows.iter().filter(|r| r.entity == "set_member").collect();
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

        // Join the seeded set through the membership rewrite: the
        // promote moves the unassigned row into the catalog in the same
        // transaction (the catalog read serves members), and the
        // fixture's members keep their places ahead of it.
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let mut members: Vec<String> = body["profiles"]
            .as_array()
            .expect("profiles array")
            .iter()
            .map(|p| p["profile_id"].as_str().expect("id").to_owned())
            .collect();
        members.push("sx1".to_owned());
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({ "members": members })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

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
        // profile PUT (parked included -- the promote is a side effect of
        // this call, not the point). The moved sx1 now lives in the catalog
        // space with its provider (the promote move carried both), so a
        // second provider hosts the new unassigned row.
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
                    "parked": false
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

    /// The unassigned listing is the park filter over the catalog space: a
    /// row created on this face and the seeded catalog row answer
    /// together, and a served row never joins them.
    #[tokio::test]
    async fn the_unassigned_list_is_the_parked_filter() {
        let state = seeded_state().await;
        // The fixture's p4 is the catalog's seeded parked row.
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
                "/admin/profiles?collection_name=baseline&parked=true",
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
                    "parked": false
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
        assert_eq!(body["parked"], true);
    }

    /// Membership drives the parked flag, and the flip is scoped to the
    /// set's own collection: a same-id row elsewhere stays unassigned, and
    /// the audit row the flip lands names the collection it touched.
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
            assert_eq!(body["parked"], true);
        }
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["twin"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["profile_id"], "twin");
        assert_eq!(body["profiles"][0]["parked"], false);
        assert!(!parked_flag(&state, "baseline", "twin").await);
        assert!(parked_flag(&state, "extra", "twin").await);
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
        assert_eq!(after["parked"], false);
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
                    "parked": false
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The sets the row leaves: the movement has to record the order
        // each of them held, exactly as a delete does.
        catalog_set(&state, "origin").await;
        catalog_profile(&state, "mate", "m", false).await;
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
        assert_eq!(body["parked"], true);
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
        assert_eq!(landed.after.as_ref().expect("create after")["parked"], true);
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
            .find(|r| r.entity == "set_member" && r.entity_id == "origin" && r.action == "update")
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

    /// A row a set carries is served by definition: parking it is a
    /// refused write, and the refusal leaves the row untouched.
    #[tokio::test]
    async fn a_set_member_cannot_be_parked() {
        let state = seeded_state().await;
        catalog_profile(&state, "pm1", "m", false).await;
        catalog_set(&state, "hold").await;
        catalog_member(&state, "hold", "pm1", 0).await;

        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/pm1",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "m2", "parked": true})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("cannot be unassigned"),
            "{body}"
        );
        assert!(
            !parked_flag(&state, "baseline", "pm1").await,
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
        catalog_profile_in(&state, "extra", "seat", "m", false).await;
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
        assert_eq!(body["parked"], false);
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
                r.entity_id == "mv2" || (r.entity == "set_member" && r.entity_id == "landing")
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
                ("update", "set_member"),
            ]
        );
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
