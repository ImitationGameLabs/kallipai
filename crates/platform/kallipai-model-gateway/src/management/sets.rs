//! The set face: collection-nested create and list plus the nested item
//! routes, with the membership rewrite that promotes and re-parks member
//! profiles in the same transaction.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::AdminToken;
use crate::management::user_routes::{anchor_collection_default, collection_default_guard};
use crate::management::{Deleted, resolve_set, set_state_json, validate_set_name};
use crate::registry::{self, collection, profile, profile_set, set_member};
use crate::state::AppState;
use kallipai_common::protocol::ApiError;

use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
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
    pub parked: bool,
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
            parked: p.parked,
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
}

/// PATCH /admin/collections/{collection}/sets/{name}: `description` and
/// `members` are independent -- absent means unchanged. `members`, when
/// present, replaces the whole membership (the list order is the failover
/// order).
#[derive(Deserialize)]
pub struct SetUpdate {
    pub description: Option<String>,
    pub members: Option<Vec<String>>,
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
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let set = resolve_set(&txn, &owner, &collection, &name).await?;
    let before = set_state_json(&txn, &set.owner, &set.collection_name, &name)
        .await?
        .expect("the set was resolved above");
    if let Some(members) = &body.members {
        // Duplicate ids cannot carry two positions; unknown ids would make
        // the set unresolvable for the forwarding face. Both checks run
        // before any write, and the transaction rolls back on either.
        let mut seen = std::collections::HashSet::new();
        for id in members {
            if !seen.insert(id) {
                return Err(ApiError::bad_request(format!(
                    "duplicate member {id:?} in the members list"
                )));
            }
        }
        // The set's collection gates its members: a catalog set serves its
        // members through the distribution face, so a member row must
        // live in the set's own collection -- a same-id row in another
        // collection is a 404 (the composite foreign key backs this).
        // The rows seed the promotion flip below (parked lives on the
        // profile row, not the membership row) and are taken with a row
        // lock: a profile write takes the same row first, so the two
        // serialize here -- profile rows before member rows, never the
        // reverse. The lock set is the members plus the ids leaving this
        // set: a leaver's profile row is updated by the re-park flip after
        // its member rows are gone, so it has to be locked before any member
        // row is written. Locks are taken in id order, ascending,
        // independent of the order the request lists its members in, so two
        // rewrites that share profiles cannot deadlock on a disagreement;
        // the rows are then read back in request order.
        let old_ids: std::collections::HashSet<String> = set_member::Entity::find()
            .filter(set_member::Column::Owner.eq(&set.owner))
            .filter(set_member::Column::CollectionName.eq(&set.collection_name))
            .filter(set_member::Column::SetName.eq(&name))
            .all(&txn)
            .await
            .map_err(db::map_db_err)?
            .into_iter()
            .map(|m| m.profile_id)
            .collect();
        let mut lock_ids: Vec<&String> = members.iter().chain(old_ids.iter()).collect();
        lock_ids.sort_unstable();
        lock_ids.dedup();
        let mut locked_rows = std::collections::HashMap::with_capacity(lock_ids.len());
        for id in lock_ids {
            let row =
                registry::profile_by_id_for_update(&txn, &set.owner, &set.collection_name, id)
                    .await
                    .map_err(db::map_db_err)?;
            locked_rows.insert(id.clone(), row);
        }
        // The 404 still names the first unknown id in request order: the
        // walk below reads the locked rows back in that order.
        let mut rows = Vec::with_capacity(members.len());
        for id in members {
            let row = locked_rows.remove(id).flatten().ok_or_else(|| {
                ApiError::not_found(format!("profile {id} is not in collection {collection}"))
            })?;
            rows.push(row);
        }
        set_member::Entity::delete_many()
            .filter(set_member::Column::Owner.eq(&set.owner))
            .filter(set_member::Column::CollectionName.eq(&set.collection_name))
            .filter(set_member::Column::SetName.eq(&name))
            .exec(&txn)
            .await
            .map_err(db::map_db_err)?;
        for (position, id) in members.iter().enumerate() {
            set_member::ActiveModel {
                owner: Set(set.owner.clone()),
                collection_name: Set(set.collection_name.clone()),
                set_name: Set(name.clone()),
                profile_id: Set(id.clone()),
                position: Set(position as i32),
            }
            .insert(&txn)
            .await
            .map_err(db::map_db_err)?;
        }
        // Membership drives the served face: any parked member of the
        // rewritten list is promoted -- a set member is served, that
        // is the invariant, so a stale parked row heals on the next
        // save -- and a member leaving its last set in this collection
        // becomes unassigned again. The same transaction keeps both
        // faces consistent; each flip lands its own profile update
        // event (the set event records the membership change, this row
        // records the served-face flip it caused).
        for (id, row) in members.iter().zip(&rows) {
            if row.parked {
                flip_parked(
                    &txn,
                    actor.clone(),
                    &set.owner,
                    &set.collection_name,
                    id,
                    false,
                )
                .await?;
            }
        }
        for id in &old_ids {
            if members.contains(id) {
                continue;
            }
            let remaining = set_member::Entity::find()
                .filter(set_member::Column::Owner.eq(&set.owner))
                .filter(set_member::Column::CollectionName.eq(&set.collection_name))
                .filter(set_member::Column::ProfileId.eq(id))
                .count(&txn)
                .await
                .map_err(db::map_db_err)?;
            if remaining == 0 {
                flip_parked(
                    &txn,
                    actor.clone(),
                    &set.owner,
                    &set.collection_name,
                    id,
                    true,
                )
                .await?;
            }
        }
    }
    // The set row is written after the profile rows: the collection
    // cascade takes the profile rows first and the set row after, so this
    // write keeps the module's order and the two paths cannot cycle.
    if let Some(description) = &body.description {
        profile_set::ActiveModel {
            name: Set(name.clone()),
            description: Set(description.clone()),
            owner: Set(set.owner.clone()),
            collection_name: Set(set.collection_name.clone()),
        }
        .update(&txn)
        .await
        .map_err(db::map_db_err)?;
    }
    let after = set_state_json(&txn, &set.owner, &set.collection_name, &name)
        .await?
        .expect("the set existed at transaction start");
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
/// Flip one member's `parked` flag inside the caller's transaction and
/// audit it as a profile update (the same event shape the profile PUT
/// emits): the set event records the membership change, this row records
/// the served-face flip that change caused. The write names its row in
/// full, and the owner in that name is load-bearing: an id is unique
/// inside its collection, not inside the store, so another space's
/// same-id row is a real row a narrower filter would flip too
/// (`a_same_id_row_in_another_space_survives_the_rewrite` pins it).
async fn flip_parked(
    txn: &impl sea_orm::ConnectionTrait,
    actor: std::borrow::Cow<'static, str>,
    owner: &str,
    collection_name: &str,
    profile_id: &str,
    to: bool,
) -> Result<(), ApiError> {
    let before = registry::profile_by_id(txn, owner, collection_name, profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("member row vanished mid-rewrite"))?;
    let before_served = registry::served_profile(txn, before)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    profile::Entity::update_many()
        .col_expr(profile::Column::Parked, Expr::value(to))
        .filter(profile::Column::Owner.eq(owner))
        .filter(profile::Column::CollectionName.eq(collection_name))
        .filter(profile::Column::ProfileId.eq(profile_id))
        .exec(txn)
        .await
        .map_err(db::map_db_err)?;
    let after = registry::profile_by_id(txn, owner, collection_name, profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("member row vanished mid-rewrite"))?;
    let after_served = registry::served_profile(txn, after)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("profile row without its provider"))?;
    management_event::record(
        txn,
        ManagementEventRow {
            action: "update",
            entity: "profile",
            entity_id: profile_id.to_owned(),
            actor: actor.clone(),
            before: Some(
                serde_json::to_value(AdminProfile::from(before_served))
                    .expect("AdminProfile always serializes"),
            ),
            after: Some(
                serde_json::to_value(AdminProfile::from(after_served))
                    .expect("AdminProfile always serializes"),
            ),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    Ok(())
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
    let before = set_state_json(&txn, &set.owner, &set.collection_name, &name)
        .await?
        .expect("the set was resolved above");
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
            "members and allowed-set grants were removed with the set; \
             tagma-side agents still bound to it dangle until rebound \
             (the proxy does not track per-agent binds)"
                .to_owned(),
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::testkit::*;
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
        // Members: two profiles in a chosen order.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/gamma",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p4", "p1"]})),
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
                "PATCH",
                "/admin/collections/baseline/sets/gamma",
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
        // Audit trail: one create + three updates, entity profile_set.
        let rows = audit_rows(&state.db).await;
        let gamma: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "profile_set" && r.entity_id == "gamma")
            .collect();
        assert_eq!(gamma.len(), 4);
        assert_eq!(gamma[0].action, "create");
        assert!(gamma[0].before.is_none());
        assert_eq!(gamma.iter().filter(|r| r.action == "update").count(), 3);
        assert!(rows.iter().all(|r| r.actor == "admin"));
    }

    /// The membership rewrite drives the served face: joining a set
    /// promotes an unassigned row, leaving the last set re-parks it, and
    /// leaving one of several sets keeps it served. The set event's
    /// member rows carry the flip (the audit record).
    #[tokio::test]
    async fn set_member_rewrites_promote_and_re_park() {
        let state = seeded_state().await;
        // p4 is the seeded unassigned row; alpha holds p1/p2/p6, beta p3.
        // Join: p4 enters alpha and is promoted on the same request.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p1", "p2", "p6", "p4"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!parked_flag(&state, "baseline", "p4").await);
        // Leave the only set: p4 becomes unassigned again.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p1", "p2", "p6"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(parked_flag(&state, "baseline", "p4").await);
        // Leave one of two sets: p3 stays served while beta still
        // holds it.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p1", "p2", "p3", "p6"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!parked_flag(&state, "baseline", "p3").await);
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p1", "p2", "p6"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!parked_flag(&state, "baseline", "p3").await);
        // The audit record: each flip lands its own profile update
        // event (same shape the profile PUT emits), p4 moving true ->
        // false -> true across the two rewrites (write order, as the
        // crud test above relies on).
        let rows = audit_rows(&state.db).await;
        let p4_flips: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "profile" && r.entity_id == "p4" && r.action == "update")
            .collect();
        assert_eq!(p4_flips.len(), 2);
        assert_eq!(p4_flips[0].before.as_ref().expect("before")["parked"], true);
        assert_eq!(p4_flips[0].after.as_ref().expect("after")["parked"], false);
        assert_eq!(
            p4_flips[1].before.as_ref().expect("before")["parked"],
            false
        );
        assert_eq!(p4_flips[1].after.as_ref().expect("after")["parked"], true);
    }

    /// An id is unique inside its collection, not inside the store: the
    /// same id and collection name under another owner is a different
    /// profile, and the membership rewrite leaves it where it was.
    #[tokio::test]
    async fn a_same_id_row_in_another_space_survives_the_rewrite() {
        let state = seeded_state().await;
        seed_collection(&state, "user-other", "baseline").await;
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
            parked: Set(true),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .expect("other-space profile insert");

        // The catalog rewrite that heals system's p1.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p1", "p2"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let parked = |owner: &'static str| {
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
                    .parked
            }
        };
        assert!(!parked("system").await, "the catalog row is served");
        assert!(
            parked("user-other").await,
            "the other space's row keeps its own flag"
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

    #[tokio::test]
    async fn set_member_edits_reject_unknown_and_duplicate_ids() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p1", "ghost"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["p1", "p1"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // The failed writes rolled back: alpha still has exactly p1, p2.
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
        assert!(body["warning"].as_str().expect("warning").contains("bind"));
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
                    "parked": false
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

    /// The same-space member path: a catalog unassigned row joins a set
    /// through the membership rewrite, and the rewrite promotes it (a
    /// set member is served). Parking a row in the catalog space and
    /// joining a set is the one path into the served face.
    #[tokio::test]
    async fn set_membership_promotes_a_catalog_unassigned_row() {
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
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["unassigned-1"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["profile_id"], "unassigned-1");
        let member = profile::Entity::find_by_id((
            registry::CATALOG_OWNER.to_owned(),
            "baseline".to_owned(),
            "unassigned-1".to_owned(),
        ))
        .one(&state.db)
        .await
        .expect("profile read")
        .expect("the unassigned row joined the set");
        assert!(!member.parked, "a member is served");
        // The join is audited twice: the set event for the membership
        // rewrite and a profile update for the served-face flip.
        let rows = audit_rows(&state.db).await;
        assert!(
            rows.iter()
                .any(|r| r.entity == "profile_set" && r.entity_id == "alpha")
        );
        assert!(
            rows.iter()
                .any(|r| r.entity == "profile" && r.entity_id == "unassigned-1")
        );
    }

    /// A foreign space's row cannot ride along: the membership rewrite
    /// resolves member ids in the set's space only, so a row parked
    /// one space over is a miss (404, never a silent move).
    #[tokio::test]
    async fn set_membership_refuses_a_foreign_space_row() {
        let state = seeded_state().await;
        foreign_parked(&state, "unassigned-x").await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["unassigned-x"]})),
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
        assert!(stayed.parked);
    }

    /// The failover order is the submitted order: a replacement PATCH
    /// reads back exactly as listed, in whichever direction.
    #[tokio::test]
    async fn user_set_members_keep_the_submitted_order() {
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
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections/home/sets",
                user,
                true,
                Some(serde_json::json!({
                    "name": "ordered",
                    "description": "d",
                    "members": ["p-b"]
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for order in [["p-b", "p-a"], ["p-a", "p-b"]] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "PATCH",
                    "/user/collections/home/sets/ordered",
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
    /// different row, and a member row from another collection is not a
    /// member here.
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
        // Serving a member through the twin leaves baseline's alpha alone.
        catalog_profile_in(&state, "extra", "ex1", "m", true).await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/extra/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["ex1"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["profile_id"], "ex1");
        assert_eq!(body["profiles"][0]["parked"], false);
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
        // A member row that lives in another collection is not reachable
        // from this set, and the rejected rewrite leaves the list intact.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"members": ["ex1"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("is not in collection baseline"),
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
    async fn patch_order_drives_the_data_plane_head() {
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
        // Put p2 first through the admin face.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
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
                "PATCH",
                "/admin/collections/baseline/sets/alpha",
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
}
