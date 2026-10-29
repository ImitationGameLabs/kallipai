//! The set face: collection-nested create and list plus the nested item
//! routes. Membership is a profile-row column (`set_name`, `position`), so a
//! set write touches the description and `/order` the member positions.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::AdminToken;
use crate::management::user_routes::{anchor_collection_default, collection_default_guard};
use crate::management::{
    Deleted, lock_set_order, refuse_set_members, resolve_set, set_state_json, set_state_required,
    validate_set_name, write_set_order,
};
use crate::registry::{self, collection, profile_set};
use crate::state::AppState;
use kallipai_common::protocol::ApiError;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
    sea_query::Expr,
};

/// The admin view of a profile: the full registry row joined with the
/// provider's family, no credential material (that lives in the secret
/// store, keyed by provider). `collection_name` echoes the container so a
/// caller reads the row's home instead of inferring it from the URL.
#[derive(Clone, Debug, Serialize)]
pub struct AdminProfile {
    pub profile_id: String,
    /// The collection this row belongs to (part of the row's identity).
    pub collection_name: String,
    /// The space-owned provider this deployment rides (the family's
    /// holder; N profiles may share one).
    pub provider_id: String,
    /// The ownership reservation: `system` is the catalog (the
    /// distribution face's only source); anything else is private
    /// to its owner.
    pub owner: String,
    pub family: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Vec<String>,
    /// The set this profile is a member of, if any (`NULL` = unassigned).
    pub set: Option<String>,
    /// Failover order inside `set`.
    pub position: i32,
}

impl From<registry::ServedProfile> for AdminProfile {
    fn from(s: registry::ServedProfile) -> Self {
        let p = &s.profile;
        let modalities = p
            .modalities
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .unwrap_or_default();
        Self {
            profile_id: p.profile_id.clone(),
            collection_name: p.collection_name.clone(),
            family: s.family,
            model: p.model.clone(),
            max_context_window: p.max_context_window,
            effort: p.effort.clone(),
            modalities,
            set: p.set_name.clone(),
            position: p.position,
            owner: p.owner.clone(),
            provider_id: p.provider_id.clone(),
        }
    }
}

/// The admin view of one set: identity plus the ordered members (the
/// failover order, verbatim -- every read path must preserve it).
#[derive(Clone, Debug, Serialize)]
pub struct AdminSet {
    pub name: String,
    pub collection_name: String,
    pub description: String,
    pub profiles: Vec<AdminProfile>,
}
#[derive(Clone, Debug, Serialize)]
pub struct AdminSetList {
    pub sets: Vec<AdminSet>,
}

/// POST /admin/collections/{collection}/sets body: a set created directly
/// inside the named collection (the collection anchors the set; the
/// catalog space is the only space on this face).
#[derive(Deserialize)]
pub struct SetCreate {
    pub name: String,
    pub description: String,
    /// Present only to refuse: membership is the profile's own `set` field
    /// (who belongs to a set), and order is the set's `/order` route.
    #[serde(default)]
    pub members: Option<serde_json::Value>,
}

/// PATCH /admin/collections/{collection}/sets/{name}: `description` only,
/// absent means unchanged. Membership is not a set property: a member's
/// `set` is written through that profile's own PUT, which also carries the
/// position it joins at.
#[derive(Deserialize)]
pub struct SetUpdate {
    pub description: Option<String>,
    /// Present only to refuse: membership is the profile's own `set` field
    /// (who belongs to a set), and order is the set's `/order` route.
    #[serde(default)]
    pub members: Option<serde_json::Value>,
}

/// POST /admin/collections/{collection}/sets: a set born inside the named
/// collection (the only creation door on this face; the anchor is for
/// life -- there is no move). A same-name set in another collection is a
/// different row, so the duplicate check is scoped to this collection.
pub async fn create_set(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(collection): Path<String>,
    Json(body): Json<SetCreate>,
) -> Result<Json<AdminSet>, ApiError> {
    let actor = admin.operator.as_str();
    refuse_set_members(&body.members)?;
    validate_set_name(&body.name)?;
    // The catalog space is the only space on this face: the path names
    // the collection the set is anchored to, so no space reaches the
    // write through the body.
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let description = body.description;
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
    // The name is unique per collection; this early check answers a
    // duplicate with a 409 instead of letting it die on the primary key
    // at insert.
    if registry::set_by_id(&txn, &owner, &collection, &body.name)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict("set already exists"));
    }
    profile_set::ActiveModel {
        name: Set(body.name.clone()),
        description: Set(description.clone()),
        owner: Set(owner.clone()),
        collection_name: Set(collection.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    // The set enters its collection at creation: the anchor column is
    // the membership authority. The first set in a default-less
    // collection takes the default anchor.
    anchor_collection_default(&txn, &owner, &collection, &body.name).await?;
    // Inserted by this transaction, with only the anchor write in between:
    // no other request can remove the row before this read can miss it.
    let after = set_state_json(&txn, &owner, &collection, &body.name)
        .await?
        .expect("the set was just inserted");
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "profile_set",
            entity_id: body.name.clone(),
            actor,
            before: None,
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(AdminSet {
        name: body.name,
        collection_name: collection,
        description,
        profiles: vec![],
    }))
}

/// GET /admin/collections/{collection}/sets: the collection's sets with
/// their ordered members (the family-nested list; the manage page reads a
/// collection at a time).
pub async fn list_collection_sets(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(collection): Path<String>,
) -> Result<Json<AdminSetList>, ApiError> {
    let owner = registry::CATALOG_OWNER.to_owned();
    let row = collection::Entity::find_by_id((owner.clone(), collection.clone()))
        .one(&state.db)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {collection}")))?;
    let set_rows = profile_set::Entity::find()
        .filter(profile_set::Column::Owner.eq(&owner))
        .filter(profile_set::Column::CollectionName.eq(row.name.clone()))
        .order_by_asc(profile_set::Column::Name)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut sets = Vec::with_capacity(set_rows.len());
    for set in set_rows {
        let profiles = registry::set_members_ordered(&state.db, &owner, &row.name, &set.name)
            .await
            .map_err(db::map_db_err)?;
        sets.push(AdminSet {
            name: set.name,
            collection_name: set.collection_name,
            description: set.description,
            profiles: profiles.into_iter().map(AdminProfile::from).collect(),
        });
    }
    Ok(Json(AdminSetList { sets }))
}

/// GET /admin/collections/{collection}/sets/{name}: item addressing inside
/// one collection (a same-name set in another collection is a 404 here),
/// the catalog space only.
pub async fn get_set(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path((collection, name)): Path<(String, String)>,
) -> Result<Json<AdminSet>, ApiError> {
    let owner = registry::CATALOG_OWNER.to_owned();
    get_set_inner(&state, &owner, &collection, &name).await
}

pub async fn update_set(
    State(state): State<AppState>,
    admin: AdminToken,
    Path((collection, name)): Path<(String, String)>,
    Json(body): Json<SetUpdate>,
) -> Result<Json<AdminSet>, ApiError> {
    let actor = admin.operator.as_str();
    refuse_set_members(&body.members)?;
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let set = resolve_set(&txn, &owner, &collection, &name).await?;
    let before = set_state_required(&txn, &set.owner, &set.collection_name, &name).await?;
    // Membership is not a set property any more: a profile's `set_name`
    // is written by that profile's own PUT, so this handler has one field.
    // The description write is conditional on the row still being there: a
    // set deleted after the resolve above answers 404 here, the same exit
    // every other lost-set read takes. This handler has no 409: the sibling
    // `/order` route keeps one as a guard, and its locks make that guard
    // stand for a lock that missed a row rather than a race a caller hits.
    if let Some(description) = &body.description {
        let written = profile_set::Entity::update_many()
            .col_expr(
                profile_set::Column::Description,
                Expr::value(description.clone()),
            )
            .filter(profile_set::Column::Owner.eq(set.owner.clone()))
            .filter(profile_set::Column::CollectionName.eq(set.collection_name.clone()))
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
    let after = set_state_required(&txn, &set.owner, &set.collection_name, &name).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "profile_set",
            entity_id: name.clone(),
            actor,
            before: Some(before),
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    get_set_inner(&state, &set.owner, &set.collection_name, &name).await
}

/// The post-commit read shared by the mutating set handlers (the response
/// is served from the committed state, not from in-transaction guesses).
async fn get_set_inner(
    state: &AppState,
    owner: &str,
    collection_name: &str,
    name: &str,
) -> Result<Json<AdminSet>, ApiError> {
    let set = registry::set_by_id(&state.db, owner, collection_name, name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such set"))?;
    let profiles = registry::set_members_ordered(&state.db, owner, collection_name, name)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(AdminSet {
        name: set.name,
        collection_name: set.collection_name,
        description: set.description,
        profiles: profiles.into_iter().map(AdminProfile::from).collect(),
    }))
}

pub async fn delete_set(
    State(state): State<AppState>,
    admin: AdminToken,
    Path((collection, name)): Path<(String, String)>,
) -> Result<Json<Deleted>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let set = resolve_set(&txn, &owner, &collection, &name).await?;
    let before = set_state_required(&txn, &set.owner, &set.collection_name, &name).await?;
    collection_default_guard(&txn, &set.owner, &set.collection_name, &name, "admin").await?;
    profile_set::Entity::delete_by_id((
        set.owner.clone(),
        set.collection_name.clone(),
        name.clone(),
    ))
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
            before: Some(before),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(Deleted {
        deleted: true,
        warning: Some(
            "the set's members became unassigned and its allowed-set grants \
             were removed with it; tagma-side agents still bound to it dangle \
             until rebound (the proxy does not track per-agent binds)"
                .to_owned(),
        ),
    }))
}
/// PUT /admin/collections/{collection}/sets/{name}/order body: the set's
/// members in the order they should fail over. A permutation, not a
/// membership edit: the ids must be exactly the set's current ones.
#[derive(Deserialize)]
pub struct SetOrder {
    pub members: Vec<String>,
}

/// PUT /admin/collections/{collection}/sets/{name}/order: rewrite the
/// failover order in place. `lock_set_order` holds the rows this write
/// could touch and checks the request is a permutation of the membership;
/// `write_set_order` writes the positions. Both faces of this route share
/// that one protocol; this handler resolves the set, audits, and commits.
/// An id outside the set, a missing id, or a repeat is a 400, a set that is
/// gone is a 404, and the 409 `write_set_order` can answer is a guard: every
/// row it renumbers is held, so that answer stands for a lock that missed a
/// row rather than a membership change a caller can race.
pub async fn reorder_set(
    State(state): State<AppState>,
    admin: AdminToken,
    Path((collection, name)): Path<(String, String)>,
    Json(body): Json<SetOrder>,
) -> Result<Json<AdminSet>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let set = resolve_set(&txn, &owner, &collection, &name).await?;
    lock_set_order(&txn, &set.owner, &set.collection_name, &name, &body.members).await?;
    let before = set_state_required(&txn, &set.owner, &set.collection_name, &name).await?;
    write_set_order(&txn, &set.owner, &set.collection_name, &name, &body.members).await?;
    let after = set_state_required(&txn, &set.owner, &set.collection_name, &name).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "profile_set",
            entity_id: name.clone(),
            actor,
            before: Some(before),
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    get_set_inner(&state, &set.owner, &set.collection_name, &name).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::testkit::*;
    use crate::registry::profile;
    use crate::registry::provider;
    use crate::secret::provider_credential;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn set_crud_round_trip_preserves_order_and_audits() {
        let state = seeded_state().await;
        // Create.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "gamma", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["name"], "gamma");
        // The set lands anchored: creation inside the baseline
        // collection is the platform catalog's entry door.
        let linked = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq("system"))
            .filter(profile_set::Column::Name.eq("gamma"))
            .filter(profile_set::Column::CollectionName.eq("baseline"))
            .one(&state.db)
            .await
            .expect("anchored set read");
        assert!(linked.is_some(), "a system set lands in baseline");
        // Duplicate name conflicts.
        let (status, _) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "gamma", "description": "x"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // Membership: two profiles join through the profile face, so the
        // set view reads them back in join order.
        for (id, model) in [("p4", "unassigned-x"), ("p1", "deepseek-chat")] {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "PUT",
                    &format!("/admin/collections/baseline/profiles/{id}"),
                    Some(TEST_ADMIN_BEARER),
                    Some(serde_json::json!({"model": model, "set": "gamma"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/sets/gamma",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["profile_id"], "p4");
        assert_eq!(body["profiles"][1]["profile_id"], "p1");
        // Reorder: the failover order is editable and read back verbatim.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/sets/gamma/order",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p1", "p4"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["profile_id"], "p1");
        // Description-only update leaves members alone.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/gamma",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"description": "new text"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["description"], "new text");
        assert_eq!(body["profiles"][0]["profile_id"], "p1");
        // Audit trail: one create + four updates -- the two joins, the
        // reorder, and the description patch -- entity profile_set.
        let rows = audit_rows(&state.db).await;
        let gamma: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "profile_set" && r.entity_id == "gamma")
            .collect();
        assert_eq!(gamma.len(), 5);
        assert_eq!(gamma[0].action, "create");
        assert!(gamma[0].before.is_none());
        assert_eq!(gamma.iter().filter(|r| r.action == "update").count(), 4);
        // The joins land their own order rows: gamma empty, then p4, then p1.
        assert_eq!(
            gamma[1].before.as_ref().expect("first join before")["members"],
            serde_json::json!([])
        );
        assert_eq!(
            gamma[1].after.as_ref().expect("first join after")["members"],
            serde_json::json!(["p4"])
        );
        assert_eq!(
            gamma[2].before.as_ref().expect("second join before")["members"],
            serde_json::json!(["p4"])
        );
        assert_eq!(
            gamma[2].after.as_ref().expect("second join after")["members"],
            serde_json::json!(["p4", "p1"])
        );
        assert!(rows.iter().all(|r| r.actor == "admin"));
    }

    /// Membership is a profile field: the profile PUT moves a row into a
    /// set (appending at the end) and the explicit null form takes it out
    /// again. A row sits in exactly one set at a time.
    #[tokio::test]
    async fn profile_put_moves_a_row_in_and_out_of_a_set() {
        let state = seeded_state().await;
        // p4 is the seeded unassigned row; alpha holds p1/p2/p6, beta p3.
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
        assert_eq!(body["set"], "alpha");
        assert_eq!(body["position"], 3, "an append lands after the last member");
        // Moving keeps the row in exactly one set: p3 leaves beta.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/p3",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "gpt-x", "set": "alpha"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["set"], "alpha");
        assert_eq!(
            set_of(&state, "baseline", "p3").await.as_deref(),
            Some("alpha")
        );
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/sets/beta",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["profiles"].as_array().expect("profiles").is_empty(),
            "the row left beta"
        );
        // The null form clears membership: p4 is unassigned again.
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
        assert!(body["set"].is_null());
        assert_eq!(set_of(&state, "baseline", "p4").await, None);
    }

    /// An id is unique inside its collection, not inside the store: the
    /// same id and collection name under another owner is a different
    /// row, and an admin write leaves it where it was.
    #[tokio::test]
    async fn a_same_id_row_in_another_space_survives_an_admin_write() {
        let state = seeded_state().await;
        seed_collection(&state, "user-other", "baseline").await;
        profile_set::ActiveModel {
            name: Set("held".to_owned()),
            description: Set("d".to_owned()),
            owner: Set("user-other".to_owned()),
            collection_name: Set("baseline".to_owned()),
        }
        .insert(&state.db)
        .await
        .expect("other-space set insert");
        provider::ActiveModel {
            owner: Set("user-other".to_owned()),
            provider_id: Set("pp".to_owned()),
            family: Set("deepseek".to_owned()),
            base_url: Set(None),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .expect("other-space provider insert");
        profile::ActiveModel {
            owner: Set("user-other".to_owned()),
            collection_name: Set("baseline".to_owned()),
            profile_id: Set("p1".to_owned()),
            provider_id: Set("pp".to_owned()),
            model: Set("m".to_owned()),
            set_name: Set(Some("held".to_owned())),
            position: Set(1),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .expect("other-space profile insert");

        // The admin write that clears system's p1 membership.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/p1",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "deepseek-chat", "set": null})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let set_of_row = |owner: &'static str| {
            let state = state.clone();
            async move {
                profile::Entity::find()
                    .filter(profile::Column::Owner.eq(owner))
                    .filter(profile::Column::CollectionName.eq("baseline"))
                    .filter(profile::Column::ProfileId.eq("p1"))
                    .one(&state.db)
                    .await
                    .expect("row read")
                    .expect("row exists")
                    .set_name
            }
        };
        assert!(
            set_of_row("system").await.is_none(),
            "the catalog row left its set"
        );
        assert_eq!(
            set_of_row("user-other").await.as_deref(),
            Some("held"),
            "the other space's row keeps its own membership"
        );
    }

    /// The admin owner opts out of the platform catalog: a pool-space
    /// set row (planted directly; the API door is catalog-only)
    /// stays off the platform face.
    #[tokio::test]
    async fn admin_owned_sets_stay_out_of_the_baseline() {
        let state = seeded_state().await;
        seed_collection(&state, "admin", "admin-col").await;
        // A pool-space set cannot arrive through the API (the nested
        // create door is catalog-only), so plant it directly: the read
        // faces must still keep it out of the platform baseline.
        profile_set::ActiveModel {
            name: Set("admin-only".to_owned()),
            description: Set("d".to_owned()),
            owner: Set("admin".to_owned()),
            collection_name: Set("admin-col".to_owned()),
        }
        .insert(&state.db)
        .await
        .expect("pool set row");
        let anchored = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq("admin"))
            .filter(profile_set::Column::Name.eq("admin-only"))
            .filter(profile_set::Column::CollectionName.eq("admin-col"))
            .one(&state.db)
            .await
            .expect("anchored set read");
        assert!(
            anchored.is_some(),
            "the admin set anchors its own collection"
        );
        let baseline = profile_set::Entity::find()
            .filter(profile_set::Column::CollectionName.eq("baseline"))
            .filter(profile_set::Column::Name.eq("admin-only"))
            .one(&state.db)
            .await
            .expect("baseline set read");
        assert!(baseline.is_none(), "an admin set never enters baseline");
    }

    /// The order route takes a permutation, never a membership edit: an
    /// unknown id, a repeat, and a missing id are all 400, and a profile
    /// PUT that names a set outside the collection is a 404.
    #[tokio::test]
    async fn order_and_assignment_reject_edits_that_do_not_fit() {
        let state = seeded_state().await;
        for members in [
            serde_json::json!(["p1", "ghost"]),
            serde_json::json!(["p1", "p1", "p2"]),
            serde_json::json!(["p1"]),
        ] {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "PUT",
                    "/admin/collections/baseline/sets/alpha/order",
                    Some(TEST_ADMIN_BEARER),
                    Some(serde_json::json!({ "members": members })),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/p4",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "unassigned-x", "set": "ghost"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        // The failed writes rolled back: alpha still has exactly p1, p2, p6.
        let (status, body) = send(
            app(state),
            req(
                "GET",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["profiles"].as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn set_delete_warns_then_audits() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], true);
        // The dangling-bind risk hint rides the response.
        assert!(
            body["warning"]
                .as_str()
                .expect("warning")
                .contains("unassigned")
        );
        // The set is gone from both faces.
        let (status, _) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Membership is a row column, so the delete leaves the rows behind
        // unassigned instead of deleting them.
        assert_eq!(set_of(&state, "baseline", "p1").await, None);
        // The selected-collection read is the surviving view: a pointer
        // at the catalog baseline lists the members it still has.
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
            req("GET", "/selected-collection", Some(TEST_TAGMA_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = body["sets"]
            .as_array()
            .expect("sets array")
            .iter()
            .map(|set| set["name"].as_str().expect("set name"))
            .collect();
        assert_eq!(names, vec!["beta"], "the deleted set left the view");
        // The delete landed an audit row with the full before-state.
        let rows = audit_rows(&state.db).await;
        let del = rows
            .iter()
            .find(|r| r.action == "delete" && r.entity_id == "alpha")
            .expect("delete audited");
        assert_eq!(del.entity, "profile_set");
        let before = del.before.as_ref().expect("before captured");
        assert_eq!(before["members"], serde_json::json!(["p1", "p2", "p6"]));
        assert!(del.after.is_none());
    }

    #[tokio::test]
    async fn update_on_folded_co_rider_addresses_the_shared_provider() {
        let state = seeded_state().await;
        catalog_provider(&state, "a1", "deepseek").await;
        insert_co_rider(&state.db, registry::CATALOG_OWNER, "baseline", "a1b", "a1").await;
        // The profile PUT carries no family: the wire shape moved it to
        // the provider face.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/a1b",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "model": "deepseek-chat",
                    "modalities": ["text"],
                    "max_context_window": 8192,
                    "set": null
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let shared =
            provider::Entity::find_by_id((registry::CATALOG_OWNER.to_owned(), "a1".to_owned()))
                .one(&state.db)
                .await
                .expect("provider read")
                .expect("shared provider row");
        assert_eq!(
            shared.family, "deepseek",
            "the profile PUT leaves the family"
        );
        // The family changes on the provider face, on the shared row.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/providers/a1",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({ "family": "openai-compatible" })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["family"], "openai-compatible");
        let shared =
            provider::Entity::find_by_id((registry::CATALOG_OWNER.to_owned(), "a1".to_owned()))
                .one(&state.db)
                .await
                .expect("provider read")
                .expect("shared provider row");
        assert_eq!(shared.family, "openai-compatible");
    }

    #[tokio::test]
    async fn delete_keeps_the_shared_provider_while_riders_remain() {
        let state = seeded_state().await;
        catalog_provider(&state, "a1", "deepseek").await;
        provider_credential::ActiveModel {
            owner: Set(registry::CATALOG_OWNER.to_owned()),
            provider_id: Set("a1".to_owned()),
            api_key: Set("sk-shared-under-test".to_owned()),
        }
        .insert(&state.db)
        .await
        .expect("seed shared credential");
        insert_co_rider(&state.db, registry::CATALOG_OWNER, "baseline", "a1", "a1").await;
        insert_co_rider(&state.db, registry::CATALOG_OWNER, "baseline", "a1b", "a1").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/collections/baseline/profiles/a1b",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The provider and its credential survive the co-rider's exit.
        let shared =
            provider::Entity::find_by_id((registry::CATALOG_OWNER.to_owned(), "a1".to_owned()))
                .one(&state.db)
                .await
                .expect("provider read")
                .expect("shared provider survives");
        let _ = shared;
        let cred = provider_credential::Entity::find_by_id((
            registry::CATALOG_OWNER.to_owned(),
            "a1".to_owned(),
        ))
        .one(&state.db)
        .await
        .expect("credential read")
        .expect("shared credential survives");
        let _ = cred;
        // The last rider's exit takes the provider with it.
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/collections/baseline/profiles/a1",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let gone =
            provider::Entity::find_by_id((registry::CATALOG_OWNER.to_owned(), "a1".to_owned()))
                .one(&state.db)
                .await
                .expect("provider read");
        assert!(gone.is_none(), "the last exit drops the provider");
    }

    /// A set born through the nested door lands in the catalog and the
    /// collection listing carries it.
    #[tokio::test]
    async fn create_set_lands_in_the_catalog_and_lists_nested() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "gamma", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state),
            req(
                "GET",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["sets"]
                .as_array()
                .expect("sets array")
                .iter()
                .any(|s| s["name"] == "gamma"),
            "the new set is listed"
        );
    }

    /// Set names are unique within their collection: an exact duplicate
    /// lands a conflict, and the same name elsewhere is a second row.
    #[tokio::test]
    async fn duplicate_set_name_still_conflicts() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "gamma", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(
            app(state),
            req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "gamma", "description": "again"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    /// A catalog row joins a set through its own PUT: the row appends
    /// to the set's order and the set view lists it. A create lands
    /// unassigned, so the profile PUT is the one path into the order.
    #[tokio::test]
    async fn profile_put_joins_a_catalog_unassigned_row_to_a_set() {
        let state = seeded_state().await;
        catalog_provider(&state, "up1", "deepseek").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/profiles",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "profile_id": "unassigned-1",
                    "provider_id": "up1",
                    "model": "unassigned-model"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["owner"], "system");
        assert!(body["set"].is_null(), "a create lands unassigned");
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/unassigned-1",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "unassigned-model", "set": "alpha"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["set"], "alpha");
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
        assert_eq!(body["profiles"][3]["profile_id"], "unassigned-1");
        // The join lands its audit row on the profile that moved.
        let rows = audit_rows(&state.db).await;
        assert!(
            rows.iter().any(|r| r.entity == "profile"
                && r.entity_id == "unassigned-1"
                && r.action == "update"),
            "the join is audited"
        );
    }

    /// The admin face resolves ids inside the catalog space only: a row
    /// one space over is a miss (404, never a silent write).
    #[tokio::test]
    async fn admin_profile_writes_refuse_a_foreign_space_row() {
        let state = seeded_state().await;
        foreign_profile(&state, "unassigned-x").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/unassigned-x",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "m", "set": null})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        let stayed = profile::Entity::find_by_id((
            LOCAL_ADMIN_OWNER.to_owned(),
            "personal".to_owned(),
            "unassigned-x".to_owned(),
        ))
        .one(&state.db)
        .await
        .expect("profile read")
        .expect("the foreign row stays where it is");
        assert!(stayed.set_name.is_none(), "the foreign row keeps its state");
    }

    /// The failover order is the submitted order: the set's `/order`
    /// route reads back exactly as listed, in whichever direction.
    #[tokio::test]
    async fn user_set_order_keeps_the_submitted_order() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                user,
                true,
                Some(serde_json::json!({"name": "home", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for pid in ["p-b", "p-a"] {
            let (status, _) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    "/user/providers",
                    user,
                    true,
                    Some(serde_json::json!({"provider_id": pid, "family": "deepseek"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let (status, _) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    "/user/collections/home/profiles",
                    user,
                    true,
                    Some(serde_json::json!({
                        "profile_id": pid,
                        "provider_id": pid,
                        "model": "deepseek-chat"
                    })),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        // The set itself starts empty: membership is the profile face's.
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections/home/sets",
                user,
                true,
                Some(serde_json::json!({"name": "ordered", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for pid in ["p-b", "p-a"] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "PUT",
                    &format!("/user/collections/home/profiles/{pid}"),
                    user,
                    true,
                    Some(serde_json::json!({"model": "deepseek-chat", "set": "ordered"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        for order in [["p-b", "p-a"], ["p-a", "p-b"]] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "PUT",
                    "/user/collections/home/sets/ordered/order",
                    user,
                    true,
                    Some(serde_json::json!({ "members": order })),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "GET",
                    "/user/collections/home/sets/ordered",
                    user,
                    false,
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["members"], serde_json::json!(order), "{body}");
        }
    }

    /// The admin create face: the path names the collection (an unknown
    /// one is a 404 before any write) and a creation lands the default
    /// anchor the first set in a collection takes (the second does not
    /// displace it).
    #[tokio::test]
    async fn admin_set_creation_requires_and_anchors_a_collection() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_BEARER);
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/ghost/sets",
                admin,
                Some(serde_json::json!({"name": "as9", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        for name in ["bs1", "bs2"] {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "POST",
                    "/admin/collections/baseline/sets",
                    admin,
                    Some(serde_json::json!({
                    "name": name,
                    "description": "d",
                    })),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let row = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq("system"))
            .filter(profile_set::Column::CollectionName.eq("baseline"))
            .filter(profile_set::Column::Name.eq("bs1"))
            .one(&state.db)
            .await
            .expect("set read");
        assert!(
            row.is_some(),
            "the set row lands anchored to the collection"
        );
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/admin/collections/baseline", admin, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["default_set"], "bs1");
    }

    /// The admin face of the anchor guard: deleting the anchored
    /// default is a 409 naming the admin transfer route, and the
    /// delete lands once the anchor transfers to another member.
    #[tokio::test]
    async fn admin_set_deletion_is_anchor_guarded() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_BEARER);
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections",
                admin,
                Some(serde_json::json!({"name": "ac8", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for name in ["g8", "g8b"] {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "POST",
                    "/admin/collections/ac8/sets",
                    admin,
                    Some(serde_json::json!({"name": name, "description": "d"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (status, body) = send(
            app(state.clone()),
            req("DELETE", "/admin/collections/ac8/sets/g8", admin, None),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("/admin/collections/ac8/default"),
            "the guard names the admin transfer route: {body}"
        );
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/ac8/default",
                admin,
                Some(serde_json::json!({"set_name": "g8b"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            req("DELETE", "/admin/collections/ac8/sets/g8", admin, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// A set's anchor is its collection: a same-name set elsewhere is a
    /// different row, and a set name resolves inside this collection only.
    #[tokio::test]
    async fn a_same_name_set_in_another_collection_is_a_different_row() {
        let state = seeded_state().await;
        seed_collection(&state, "system", "extra").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/extra/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "alpha", "description": "the twin"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["collection_name"], "extra");
        // Joining the twin lands the row in the extra set, and baseline's
        // alpha keeps its own three members.
        catalog_profile_in(&state, "extra", "ex1", "m", None).await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/extra/profiles/ex1",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "m", "set": "alpha"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["set"], "alpha");
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/extra/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["profile_id"], "ex1");
        let baseline_ids = |body: &serde_json::Value| -> Vec<String> {
            body["profiles"]
                .as_array()
                .expect("profiles array")
                .iter()
                .map(|p| p["profile_id"].as_str().expect("profile id").to_owned())
                .collect()
        };
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
        assert_eq!(baseline_ids(&body), ["p1", "p2", "p6"]);
        // A set name that only the other collection holds is a miss here:
        // the resolver never crosses the collection boundary.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/extra/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "ex-only", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/profiles/p4",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"model": "unassigned-x", "set": "ex-only"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("no such set: ex-only in collection baseline"),
            "{body}"
        );
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
        assert_eq!(baseline_ids(&body), ["p1", "p2", "p6"]);
    }

    /// The member order an admin writes is the order the data plane
    /// selects in: `set_head_for_families` walks positions, so putting a
    /// sibling first makes it the head for that family.
    #[tokio::test]
    async fn order_route_drives_the_data_plane_head() {
        let state = seeded_state().await;
        // p1 and p2 both ride the deepseek family. The seeded order puts
        // p1 first, so p1 is the head the data plane picks today.
        let head = registry::set_head_for_families(
            &state.db,
            registry::CATALOG_OWNER,
            TEST_COLLECTION,
            "alpha",
            &["deepseek"],
        )
        .await
        .expect("head read")
        .expect("a deepseek member is served");
        assert_eq!(head.profile_id, "p1");
        // Put p2 first through the order route.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/sets/alpha/order",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p2", "p1", "p6"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["profile_id"], "p2");
        assert_eq!(body["profiles"][1]["profile_id"], "p1");
        // The data plane reads the same order back: p2 is the head now.
        let head = registry::set_head_for_families(
            &state.db,
            registry::CATALOG_OWNER,
            TEST_COLLECTION,
            "alpha",
            &["deepseek"],
        )
        .await
        .expect("head read")
        .expect("a deepseek member is served");
        assert_eq!(head.profile_id, "p2");
        assert_eq!(head.family, "deepseek");
        let ordered: Vec<String> = registry::set_members_ordered(
            &state.db,
            registry::CATALOG_OWNER,
            TEST_COLLECTION,
            "alpha",
        )
        .await
        .expect("members read")
        .into_iter()
        .map(|served| served.profile.profile_id)
        .collect();
        assert_eq!(ordered, ["p2", "p1", "p6"]);
        // And back: the head follows the order rather than sticking to
        // whichever member it picked first.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/collections/baseline/sets/alpha/order",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p1", "p2", "p6"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let head = registry::set_head_for_families(
            &state.db,
            registry::CATALOG_OWNER,
            TEST_COLLECTION,
            "alpha",
            &["deepseek"],
        )
        .await
        .expect("head read")
        .expect("a deepseek member is served");
        assert_eq!(head.profile_id, "p1");
    }
    /// A create or patch body that carries `members` is refused, and nothing
    /// about the set changes: membership is the profile's `set` field.
    #[tokio::test]
    async fn set_writes_refuse_a_members_payload() {
        let state = seeded_state().await;
        // Create with members: 400, and no such set appears.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "m1", "description": "d", "members": ["p1"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("members"),
            "{body}"
        );
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body["sets"]
                .as_array()
                .expect("sets array")
                .iter()
                .any(|set| set["name"] == "m1"),
            "{body}"
        );
        // Patch with members: 400, and alpha keeps its description and order.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(
                    serde_json::json!({"description": "rewritten", "members": ["p2", "p1", "p6"]}),
                ),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
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
        assert_eq!(body["description"], "the allowed set");
        let ids: Vec<&str> = body["profiles"]
            .as_array()
            .expect("profiles array")
            .iter()
            .map(|p| p["profile_id"].as_str().expect("profile id"))
            .collect();
        assert_eq!(ids, ["p1", "p2", "p6"]);
    }
}
