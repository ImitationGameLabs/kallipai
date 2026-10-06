//! The parking domain: profiles enter the registry here as catalog-space
//! drafts (`parked = true`), invisible on the distribution face until a
//! set joins or a plain PUT promotes them.

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::AdminToken;
use crate::management::providers;
use crate::management::sets::AdminProfile;
use crate::management::{
    Deleted, encode_modalities, resolve_profile, set_state_json, validate_modalities,
    validate_profile_id,
};
use crate::registry::{self, profile, provider, set_member};
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
}

/// PUT /admin/parking/{id}: full-field replacement (plain PUT semantics --
/// the body is the complete desired state, `parked` included, so a promote
/// to served and a demote back to draft are the same endpoint).
#[derive(Deserialize)]
pub struct ProfilePut {
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
    pub parked: bool,
}
/// New profiles enter the registry as parking drafts (`parked = true`, not
/// client-visible on the distribution face until promoted). The draft
/// references a provider in the catalog space (the family rides the
/// provider row); promotion is a plain PUT with `parked: false`.
/// Joining a set through `PATCH /admin/sets/{name}` promotes too: the
/// membership rewrite flips `parked` in the same transaction.
pub async fn create_parked(
    State(state): State<AppState>,
    admin: AdminToken,
    Json(body): Json<ProfileCreate>,
) -> Result<Json<AdminProfile>, ApiError> {
    let actor = admin.operator.as_str();
    validate_profile_id(&body.profile_id)?;
    if body.model.is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    validate_modalities(&body.modalities)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    registry::provider_by_id(&txn, registry::CATALOG_OWNER, &body.provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {}", body.provider_id)))?;
    if !registry::profiles_by_id(&txn, &body.profile_id)
        .await
        .map_err(db::map_db_err)?
        .is_empty()
    {
        return Err(ApiError::conflict("profile already exists"));
    }
    let active = profile::ActiveModel {
        profile_id: Set(body.profile_id.clone()),
        provider_id: Set(body.provider_id.clone()),
        model: Set(body.model.clone()),
        max_context_window: Set(body.max_context_window),
        effort: Set(body.effort.clone()),
        modalities: Set(encode_modalities(&body.modalities)),
        // New profiles park: serving is an explicit promotion (the PUT).
        parked: Set(true),
        store: Set(None),
        // A new draft is born in the catalog space: it is a member
        // candidate the moment it exists (joining a set promotes it).
        owner: Set(registry::CATALOG_OWNER.to_owned()),
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

/// GET /admin/parking: the catalog space's parked drafts. The domain is
/// fixed to the catalog (system) space -- the one space this face
/// manages.
pub async fn list_parked(
    State(state): State<AppState>,
    _admin: AdminToken,
) -> Result<Json<Vec<AdminProfile>>, ApiError> {
    let parked = profile::Entity::find()
        .filter(profile::Column::Parked.eq(true))
        .filter(profile::Column::Owner.eq(registry::CATALOG_OWNER))
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let served = registry::served_many(&state.db, parked)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(served.into_iter().map(AdminProfile::from).collect()))
}

/// The parking detail route serves parked drafts only: an unparked profile
/// is not in this resource (404). PUT and DELETE still operate on it --
/// they are how a served profile gets edited, re-parked, or removed.
pub async fn get_parked(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(profile_id): Path<String>,
) -> Result<Json<AdminProfile>, ApiError> {
    let owner = registry::CATALOG_OWNER.to_owned();
    let p = resolve_profile(&state.db, &owner, &profile_id).await?;
    if !p.parked {
        return Err(ApiError::not_found("profile is not parked"));
    }
    let served = registry::served_profile(&state.db, p)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    Ok(Json(AdminProfile::from(served)))
}

pub async fn update_parked(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(profile_id): Path<String>,
    Json(body): Json<ProfilePut>,
) -> Result<Json<AdminProfile>, ApiError> {
    let actor = admin.operator.as_str();
    if body.model.is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    validate_modalities(&body.modalities)?;
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let current = resolve_profile(&txn, &owner, &profile_id).await?;
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
        provider_id: Set(current.provider_id.clone()),
        model: Set(body.model.clone()),
        max_context_window: Set(body.max_context_window),
        effort: Set(body.effort.clone()),
        modalities: Set(encode_modalities(&body.modalities)),
        parked: Set(body.parked),
        store: Set(prev_store),
        // Attribution is not payload: the PUT replaces content, the
        // owner stays.
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

/// Deleting a profile cascades three facts: the profile row, its upstream
/// credential (via the FK cascade), and its set memberships (also via the
/// cascade). All three are audited: the profile as the primary entry, the
/// credential as its own masked row when one existed, and one row per
/// affected set with the set's before/after member order (entity
/// `set_member`) -- the membership rows vanish, so this is their trace.
pub async fn delete_parked(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(profile_id): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let current = resolve_profile(&txn, &owner, &profile_id).await?;
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
    let affected_sets = set_member::Entity::find()
        .filter(set_member::Column::ProfileId.eq(profile_id.clone()))
        .filter(set_member::Column::Owner.eq(current.owner.clone()))
        .order_by_asc(set_member::Column::SetName)
        .all(&txn)
        .await
        .map_err(db::map_db_err)?;
    let mut member_befores = Vec::with_capacity(affected_sets.len());
    for m in &affected_sets {
        if let Some(state) = set_state_json(&txn, &current.owner, &m.set_name).await? {
            member_befores.push((m.set_name.clone(), state));
        }
    }
    profile::Entity::delete_by_id((current.owner.clone(), profile_id.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    // The provider projection dies only with its last profile: the fold
    // shares one provider row across profiles, so the credential (which
    // cascades with the provider row) outlives any single rider.
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
    // the cascade, and the order left behind. Failover position is exactly
    // the fact nothing else records.
    for (set_name, before) in member_befores {
        let after = set_state_json(&txn, &current.owner, &set_name)
            .await?
            .expect("the set outlives its members within this transaction");
        management_event::record(
            &txn,
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
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(Deleted {
        deleted: true,
        warning: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::testkit::*;
    use axum::http::StatusCode;
    use sea_orm::{ConnectionTrait, EntityTrait, Statement};

    #[tokio::test]
    async fn parking_crud_promotes_and_audits() {
        let state = seeded_state().await;
        catalog_provider(&state, "up1", "openai-compatible").await;
        let draft = serde_json::json!({
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
                "/admin/parking",
                Some(TEST_ADMIN_BEARER),
                Some(draft),
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
                    "/admin/parking",
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
                "/admin/parking/p9",
                Some(TEST_ADMIN_BEARER),
                Some(promote),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["parked"], false);
        // The promoted profile left the parking resource on both faces.
        let (status, _) = send(
            app(state.clone()),
            req("GET", "/admin/parking/p9", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/parking", Some(TEST_TAGMA_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let models = body["profiles"].as_array().expect("profiles array");
        assert_eq!(models.len(), 1); // only the seeded p4 draft remains
        // The distribution payload is the sanitized shape (no profile_id;
        // the model name identifies the draft).
        assert_eq!(models[0]["model"], "draft-x");
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

    /// A draft references a provider: the family derives from the
    /// referenced row and the provider table gains nothing.
    #[tokio::test]
    async fn create_parked_references_a_provider_and_derives_its_family() {
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
                "/admin/parking",
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
    // and the referencing draft must land in one space (the catalog
    // space this face manages).
    #[tokio::test]
    async fn provider_then_parked_draft_share_the_catalog_space_without_owner_params() {
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
                "/admin/parking",
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

    /// Deleting a parked draft removes it from the domain's faces and
    /// lands one profile delete row; deleting a served profile (a row
    /// the admin space owns outright, planted with its own membership
    /// edge) additionally lands the cascading credential's own masked
    /// delete row and one row per affected set with its before/after
    /// member order (the membership edge leaves via the FK cascade, but
    /// the audit trail keeps it).
    #[tokio::test]
    async fn delete_parked_audits_the_cascade() {
        let state = seeded_state().await;
        // The whole walk stays in the catalog space: the drafts, the
        // served row, its credential, and a set holding the membership
        // edge.
        catalog_profile(&state, "a4", "draft-x", true).await;
        catalog_profile(&state, "a1", "m1", false).await;
        catalog_profile(&state, "a2", "m2", false).await;
        catalog_credential(&state, "a1", "sk-served-under-test").await;
        catalog_set(&state, "alpha2").await;
        catalog_member(&state, "alpha2", "a1", 0).await;
        catalog_member(&state, "alpha2", "a2", 1).await;
        // The planted draft a4 has no credential: one audit row only.
        let (status, body) = send(
            app(state.clone()),
            req("DELETE", "/admin/parking/a4", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = send(
            app(state.clone()),
            req("GET", "/admin/parking/a4", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // a1 is served, member of alpha2, and holds a credential: the
        // delete cascades all three facts.
        let (status, _) = send(
            app(state.clone()),
            req("DELETE", "/admin/parking/a1", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The set lost the member... the catalog set face cannot see the
        // admin space, so the membership is asserted at the row level.
        let member_rows_left = state
            .db
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT profile_id FROM set_members WHERE set_name = 'alpha2' ORDER BY position"
                    .to_owned(),
            ))
            .await
            .expect("member rows read");
        let ids_left: Vec<String> = member_rows_left
            .iter()
            .map(|r| r.try_get::<String>("", "profile_id").expect("profile id"))
            .collect();
        assert_eq!(ids_left, vec!["a2".to_owned()]); // only a2 remains
        // ...and the parking face answers 404 for the deleted id (the
        // distribution face never saw this private row).
        let (status, _) = send(
            app(state.clone()),
            req("GET", "/admin/parking/a1", Some(TEST_ADMIN_BEARER), None),
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
    /// no stored value (the distribution face answers null -- unknown,
    /// not false), and a full-field PUT on top of a true column keeps it
    /// true. The null end is asserted on the distribution face (that is
    /// the face the responses wire reads); the preserved end lives on a
    /// private draft, so the row itself is the witness.
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
                "/admin/parking",
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
        // promote moves the draft into the catalog in the same
        // transaction (the catalog read serves members), and the
        // fixture's members keep their places ahead of it.
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/admin/sets/alpha", Some(TEST_ADMIN_BEARER), None),
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
                "/admin/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({ "members": members })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) = send(
            app(state.clone()),
            req("GET", "/profiles/sx1", Some(TEST_TAGMA_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["store"], serde_json::Value::Null);

        // A fresh parked draft carries the same guarantee: set the
        // column directly, then replace the full desired state through
        // the parking PUT (parked included -- the promote is a side
        // effect of this call, not the point). The moved sx1 now
        // lives in the catalog space with its provider (the promote
        // move carried both), so a second provider hosts the new draft.
        catalog_provider(&state, "up2", "openai-compatible").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/parking",
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
                "UPDATE profiles SET store = TRUE WHERE profile_id = 'sx2'".to_owned(),
            ))
            .await
            .expect("flip store");
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/parking/sx2",
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
        // ProfilePut, so the replacement must not zero it. The draft
        // never joins a set, so the witness is the row itself.
        let stored = state
            .db
            .query_one(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT store FROM profiles WHERE profile_id = 'sx2'".to_owned(),
            ))
            .await
            .expect("store read")
            .expect("sx2 row");
        assert!(stored.try_get::<bool>("", "store").expect("store value"));
    }

    /// The parking list is the catalog space's list: drafts created on
    /// this face and the seeded catalog drafts answer together.
    #[tokio::test]
    async fn parking_lists_the_catalog_space() {
        let state = seeded_state().await;
        // The fixture's p4 is the catalog's seeded parked draft.
        catalog_provider(&state, "up1", "deepseek").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/parking",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "draft-2",
                    "provider_id": "up1",
                    "model": "draft-model"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/admin/parking", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ids: Vec<&str> = body
            .as_array()
            .expect("parking array")
            .iter()
            .map(|p| p["profile_id"].as_str().expect("profile id"))
            .collect();
        assert!(ids.contains(&"p4"), "the seeded catalog draft lists");
        assert!(ids.contains(&"draft-2"), "the created draft lists");
    }
}
