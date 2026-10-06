//! The admin collection face: the publishable bundles above sets,
//! their publication rows on both audience families, and the
//! default-set anchor transfers.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::AdminToken;
use crate::management::user_routes::{
    CascadeFace, UserCollectionCreate, UserCollectionDefaultBody, UserCollectionUpdate,
    UserPublicationBody, delete_collection_cascade, user_collection_view,
};
use crate::management::validate_set_name;
use crate::registry::{
    self, EVERYONE_GROUP_ID, collection, platform_groups, platform_publications, profile_set,
};
use crate::state::AppState;
use kallipai_common::protocol::ApiError;

use sea_orm::sea_query::{Expr, Query as SeaQuery};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
};

// -- collections -----------------------------------------------------------
/// The admin collection view: the user-space shape plus the live
/// platform audiences (the groups this collection is visible to
/// on the reach face). The user face keeps its own shape; this
/// field exists only in the admin response.
#[derive(Clone, Debug, Serialize)]
pub struct AdminCollectionView {
    pub name: String,
    pub description: String,
    pub sets: Vec<String>,
    pub publications: Vec<String>,
    /// The collection's default-set anchor (null = none chosen yet).
    pub default_set: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdminCollectionViews {
    pub collections: Vec<AdminCollectionView>,
}

async fn admin_collection_view<C: sea_orm::ConnectionTrait>(
    db: &C,
    owner: String,
    row: collection::Model,
) -> Result<AdminCollectionView, ApiError> {
    let base = user_collection_view(db, owner.clone(), row).await?;
    let pubs = platform_publications::Entity::find()
        .filter(platform_publications::Column::Owner.eq(owner))
        .filter(platform_publications::Column::CollectionName.eq(base.name.clone()))
        .all(db)
        .await
        .map_err(db::map_db_err)?;
    let mut publications: Vec<String> = pubs.into_iter().map(|p| p.group_id).collect();
    publications.sort();
    Ok(AdminCollectionView {
        name: base.name,
        description: base.description,
        sets: base.sets,
        default_set: base.default_set,
        publications,
    })
}

/// GET /admin/collections: the catalog owner's collections with their
/// (unordered) set members.
pub async fn list_collections(
    State(state): State<AppState>,
    _admin: AdminToken,
) -> Result<Json<AdminCollectionViews>, ApiError> {
    let owner = registry::CATALOG_OWNER.to_string();
    let rows = collection::Entity::find()
        .filter(collection::Column::Owner.eq(owner.clone()))
        .order_by_asc(collection::Column::Name)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        views.push(admin_collection_view(&state.db, owner.clone(), row).await?);
    }
    Ok(Json(AdminCollectionViews { collections: views }))
}

/// GET /admin/collections/{name}.
pub async fn get_collection(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(name): Path<String>,
) -> Result<Json<AdminCollectionView>, ApiError> {
    let owner = registry::CATALOG_OWNER.to_string();
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&state.db)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    Ok(Json(admin_collection_view(&state.db, owner, row).await?))
}

/// POST /admin/collections: the catalog owner's publishable bundle.
pub async fn create_collection(
    State(state): State<AppState>,
    admin: AdminToken,
    Json(body): Json<UserCollectionCreate>,
) -> Result<Json<AdminCollectionView>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_string();
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
    let view = admin_collection_view(&txn, owner.clone(), row).await?;
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

/// PATCH /admin/collections/{name}: `description` only (absent means
/// unchanged); the set membership is not a collection attribute.
/// Set membership changes only by creating or deleting a set.
pub async fn update_collection(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(name): Path<String>,
    Json(body): Json<UserCollectionUpdate>,
) -> Result<Json<AdminCollectionView>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_string();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    let before = admin_collection_view(&txn, owner.clone(), row).await?;
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
    let after = admin_collection_view(&txn, owner.clone(), row).await?;
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

/// DELETE /admin/collections/{name}: the membership and publication rows
/// cascade with it.
pub async fn delete_collection(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_string();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    let before = admin_collection_view(&txn, owner.clone(), row).await?;
    delete_collection_cascade(&txn, &owner, &name, &actor, CascadeFace::Admin).await?;
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

/// POST /admin/collections/{name}/publications: make the collection
/// visible to a platform group (the publication is a row in the platform
/// side of the reach layer; a duplicate conflicts).
pub async fn publish_collection(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(name): Path<String>,
    Json(body): Json<UserPublicationBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_string();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    // The reserved audience needs no member face: the sentinel row is
    // the only platform group id the publish face accepts; the
    // publications foreign key backs every other id.
    if body.audience != EVERYONE_GROUP_ID {
        platform_groups::Entity::find_by_id(body.audience.clone())
            .one(&txn)
            .await
            .map_err(db::map_db_err)?
            .ok_or_else(|| ApiError::not_found(format!("no such group: {}", body.audience)))?;
    }
    if platform_publications::Entity::find_by_id((
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
    platform_publications::ActiveModel {
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

/// DELETE /admin/collections/{name}/publications/{group}: withdraw from
/// one platform group.
pub async fn unpublish_collection(
    State(state): State<AppState>,
    admin: AdminToken,
    Path((name, group)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_string();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    platform_publications::Entity::find_by_id((owner.clone(), name.clone(), group.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("not published to group {group}")))?;
    platform_publications::Entity::delete_by_id((owner.clone(), name.clone(), group.clone()))
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

/// PATCH /admin/collections/{name}/default: transfer the catalog
/// collection's default-set anchor (the user face's twin over the
/// catalog owner). The target must be a set anchored to this
/// collection; anything else is a 404.
pub async fn set_collection_default(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(name): Path<String>,
    Json(body): Json<UserCollectionDefaultBody>,
) -> Result<Json<AdminCollectionView>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_string();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = collection::Entity::find_by_id((owner.clone(), name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such collection: {name}")))?;
    let before = admin_collection_view(&txn, owner.clone(), row).await?;
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
    let member_of_collection = SeaQuery::select()
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
    let after = admin_collection_view(&txn, owner.clone(), row).await?;
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

// -- the group face ------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::management::testkit::*;
    use crate::registry::{
        self, EVERYONE_GROUP_ID, collection, collection_publication, group, platform_group_members,
        platform_groups, platform_publications, profile, profile_set, set_member,
    };
    use axum::http::StatusCode;
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Set};

    /// GET /user/platform-collections: the reach matrix on the user
    /// face. No platform membership sees the baseline bundle alone;
    /// joining a platform group adds its publications, ordered by
    /// owner then name; a published collection with no sets never
    /// surfaces.
    #[tokio::test]
    async fn user_platform_catalog_reach_matrix() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/user/platform-collections", user, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["collections"],
            serde_json::json!([{
                "owner": "system",
                "name": "baseline",
                "description": "the platform default audience",
                "sets": ["alpha", "beta"],
            }])
        );
        // Join the platform group (the membership row the join flow
        // would write), publish a setless collection to it, and
        // double-publish catalog-pro to everyone (two audience rows).
        platform_group_members::ActiveModel {
            group_id: Set("pg-bound".to_owned()),
            member_account: Set("user-plain".to_owned()),
        }
        .insert(&state.db)
        .await
        .expect("insert test membership");
        collection::ActiveModel {
            owner: Set("system".to_owned()),
            name: Set("empty-col".to_owned()),
            description: Set("published but setless".to_owned()),
            default_set_name: Set(None),
        }
        .insert(&state.db)
        .await
        .expect("insert empty collection");
        platform_publications::ActiveModel {
            owner: Set("system".to_owned()),
            collection_name: Set("empty-col".to_owned()),
            group_id: Set("pg-bound".to_owned()),
        }
        .insert(&state.db)
        .await
        .expect("insert empty publication");
        // The double audience: catalog-pro already publishes to
        // pg-bound; the everyone row puts two publication rows in
        // reach -- the DISTINCT keeps each set exactly once.
        platform_publications::ActiveModel {
            owner: Set("system".to_owned()),
            collection_name: Set("catalog-pro".to_owned()),
            group_id: Set("everyone".to_owned()),
        }
        .insert(&state.db)
        .await
        .expect("insert everyone publication");
        let (status, body) = send(
            app(state),
            cookie_req("GET", "/user/platform-collections", user, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let names: Vec<&str> = body["collections"]
            .as_array()
            .expect("collections array")
            .iter()
            .map(|c| c["name"].as_str().expect("name is a string"))
            .collect();
        assert_eq!(names, ["baseline", "catalog-pro"], "{body}");
        assert_eq!(
            body["collections"][1],
            serde_json::json!({
                "owner": "system",
                "name": "catalog-pro",
                "description": "the pro catalog",
                "sets": ["pro-set"],
            })
        );
    }

    /// Publishing to the reserved audience lands without any group row,
    /// and the audit row records the audience like any other publish.
    #[tokio::test]
    async fn publish_to_everyone_needs_no_group_row() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                user,
                true,
                Some(serde_json::json!({"name": "bundle", "description": "d"})),
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
                Some(serde_json::json!({"audience": "everyone"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["audience"], "everyone");
        let row = audit_rows(&state.db)
            .await
            .into_iter()
            .find(|r| r.entity == "collection" && r.entity_id == "bundle" && r.action == "publish")
            .expect("the publish audit row");
        assert_eq!(row.action, "publish");
        let after = row.after.expect("the audit after payload");
        assert_eq!(after["group_id"], "everyone");
    }

    /// A repeat publish to the reserved audience still conflicts: the
    /// publication row is the dedupe key, reserved id or not.
    #[tokio::test]
    async fn publish_to_everyone_twice_conflicts() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                user,
                true,
                Some(serde_json::json!({"name": "bundle", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for expected in [StatusCode::OK, StatusCode::CONFLICT] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    "/user/collections/bundle/publications",
                    user,
                    true,
                    Some(serde_json::json!({"audience": "everyone"})),
                ),
            )
            .await;
            assert_eq!(status, expected, "{body}");
        }
    }

    /// Unpublishing withdraws the reserved-audience publication (a
    /// follow-up publish succeeds again).
    #[tokio::test]
    async fn unpublish_from_everyone_removes_the_publication() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                user,
                true,
                Some(serde_json::json!({"name": "bundle", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let publish = || {
            cookie_req(
                "POST",
                "/user/collections/bundle/publications",
                Some(TEST_USER_COOKIE),
                true,
                Some(serde_json::json!({"audience": "everyone"})),
            )
        };
        let (status, body) = send(app(state.clone()), publish()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "DELETE",
                "/user/collections/bundle/publications/everyone",
                user,
                true,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], true);
        let (status, body) = send(app(state), publish()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// The admin collection face: a full CRUD round trip against the
    /// catalog owner's space, with the audit trail attributed to the
    /// fixed local-admin account.
    #[tokio::test]
    async fn admin_collection_crud_round_trip() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections",
                admin,
                true,
                Some(serde_json::json!({
                    "name": "catalog-bundle",
                    "description": "d"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["name"], "catalog-bundle");
        // A set joins through the nested door; the view reads it back.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections/catalog-bundle/sets",
                admin,
                true,
                Some(serde_json::json!({"name": "cb1", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "GET",
                "/admin/collections/catalog-bundle",
                admin,
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["sets"], serde_json::json!(["cb1"]));
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "GET",
                "/admin/collections/catalog-bundle",
                admin,
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/admin/collections", admin, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.to_string().contains("catalog-bundle"));
        // Update: description only.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                "/admin/collections/catalog-bundle",
                admin,
                true,
                Some(serde_json::json!({"description": "d2"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["description"], "d2");
        // Delete; the row is gone.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "DELETE",
                "/admin/collections/catalog-bundle",
                admin,
                true,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], true);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "GET",
                "/admin/collections/catalog-bundle",
                admin,
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // The audit trail carries the fixed local-admin account.
        let rows = audit_rows(&state.db).await;
        let row = rows
            .iter()
            .find(|r| {
                r.entity == "collection" && r.entity_id == "catalog-bundle" && r.action == "delete"
            })
            .expect("the delete audit row");
        assert_eq!(row.actor, "admin");
    }

    /// The platform publish face writes the platform side of the reach
    /// layer: the publication row lands in `platform_publications`, a
    /// repeat publish conflicts, and unpublish removes it. The migration
    /// shape holds: both everyone rows exist, the shared table carries no
    /// system rows, and the platform table starts with the baseline row.
    #[tokio::test]
    async fn admin_publish_platform_side_round_trip() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections",
                admin,
                true,
                Some(serde_json::json!({"name": "catalog-bundle", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The migration's two-row everyone arrangement: the user-side
        // row stays, the platform-side sentinel is planted.
        let sentinel = platform_groups::Entity::find_by_id("everyone")
            .one(&state.db)
            .await
            .expect("platform groups read");
        assert!(sentinel.is_some(), "the platform-side sentinel row");
        let user_side = group::Entity::find_by_id("everyone")
            .one(&state.db)
            .await
            .expect("user groups read");
        assert!(user_side.is_some(), "the user-side everyone row");
        // The shared table holds no system rows after the migration.
        let shared = collection_publication::Entity::find()
            .filter(collection_publication::Column::Owner.eq(registry::CATALOG_OWNER))
            .all(&state.db)
            .await
            .expect("shared publications read");
        assert!(shared.is_empty());
        // Publish to the reserved audience: no member face consulted.
        for expected in [StatusCode::OK, StatusCode::CONFLICT] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    "/admin/collections/catalog-bundle/publications",
                    admin,
                    true,
                    Some(serde_json::json!({"audience": "everyone"})),
                ),
            )
            .await;
            assert_eq!(status, expected, "{body}");
        }
        let published = platform_publications::Entity::find()
            .all(&state.db)
            .await
            .expect("platform publications read");
        assert_eq!(published.len(), 3, "baseline, catalog-pro, and the new one");
        assert!(
            published
                .iter()
                .any(|p| p.collection_name == "catalog-bundle" && p.group_id == "everyone")
        );
        // Unpublish withdraws the row.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "DELETE",
                "/admin/collections/catalog-bundle/publications/everyone",
                admin,
                true,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], true);
        let published = platform_publications::Entity::find()
            .all(&state.db)
            .await
            .expect("platform publications read");
        assert_eq!(published.len(), 2, "baseline and catalog-pro remain");
        assert!(
            published
                .iter()
                .all(|p| p.collection_name != "catalog-bundle")
        );
        // The admin view carries the live audiences: the face reports
        // this collection's own rows (one here, baseline to everyone).
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/admin/collections/baseline", admin, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["publications"], serde_json::json!(["everyone"]));
        // The audit rows attribute to the fixed local-admin account.
        let rows = audit_rows(&state.db).await;
        let row = rows
            .iter()
            .find(|r| {
                r.entity == "collection" && r.entity_id == "catalog-bundle" && r.action == "publish"
            })
            .expect("the publish audit row");
        assert_eq!(row.actor, "admin");
    }

    /// Deleting a collection cascades away both child families: a
    /// same-name recreation starts with no inherited publications
    /// and can be published again without a false 409.
    #[tokio::test]
    async fn admin_delete_collection_clears_children() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        // Create with one set, publish to the sentinel audience, then
        // delete the whole thing.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections",
                admin,
                true,
                Some(serde_json::json!(
                    {"name": "catalog-bundle", "description": "d"}
                )),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections/catalog-bundle/sets",
                admin,
                true,
                Some(serde_json::json!({"name": "cd1", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections/catalog-bundle/publications",
                admin,
                true,
                Some(serde_json::json!({"audience": "everyone"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // Both child families carry exactly this collection's rows.
        let sets = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq("system"))
            .filter(profile_set::Column::CollectionName.eq("catalog-bundle"))
            .all(&state.db)
            .await
            .expect("profile sets read");
        assert_eq!(sets.len(), 1);
        let pubs = platform_publications::Entity::find()
            .filter(platform_publications::Column::Owner.eq("system"))
            .filter(platform_publications::Column::CollectionName.eq("catalog-bundle"))
            .all(&state.db)
            .await
            .expect("platform publications read");
        assert_eq!(pubs.len(), 1);
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "DELETE",
                "/admin/collections/catalog-bundle",
                admin,
                true,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // No child rows survive the delete.
        let sets = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq("system"))
            .filter(profile_set::Column::CollectionName.eq("catalog-bundle"))
            .all(&state.db)
            .await
            .expect("profile sets read");
        assert!(sets.is_empty(), "profile_sets rows survive");
        let pubs = platform_publications::Entity::find()
            .filter(platform_publications::Column::Owner.eq("system"))
            .filter(platform_publications::Column::CollectionName.eq("catalog-bundle"))
            .all(&state.db)
            .await
            .expect("platform publications read");
        assert!(pubs.is_empty(), "platform_publications rows survive");
        // A same-name recreation inherits nothing.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections",
                admin,
                true,
                Some(serde_json::json!({"name": "catalog-bundle", "description": "d2"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["publications"],
            serde_json::json!([]),
            "no ghost publications"
        );
        // Publishing again answers OK, not a false conflict.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections/catalog-bundle/publications",
                admin,
                true,
                Some(serde_json::json!({"audience": "everyone"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// Anchoring: the first set in a collection takes the default
    /// anchor (the second does not displace it), and every creation
    /// lands its set row anchored to the collection.
    #[tokio::test]
    async fn user_first_set_anchors_the_collection_default() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        user_provider_profile(&state, "up1", "pA").await;
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                user,
                true,
                Some(serde_json::json!({"name": "c1", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for name in ["s1", "s2"] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    "/user/collections/c1/sets",
                    user,
                    true,
                    Some(serde_json::json!({
                        "name": name,
                        "description": "d",
                        "members": ["pA"],
                    })),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        // The view the read faces serve (set membership) plus the
        // anchor: first set in, second does not displace it.
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/user/collections/c1", user, true, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["sets"], serde_json::json!(["s1", "s2"]));
        assert_eq!(body["default_set"], "s1");
        // The set row carries its collection anchor.
        let row = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq("user-plain"))
            .filter(profile_set::Column::CollectionName.eq("c1"))
            .filter(profile_set::Column::Name.eq("s2"))
            .one(&state.db)
            .await
            .expect("set read");
        assert!(
            row.is_some(),
            "the second set lands anchored to the collection"
        );
        // An unknown collection is a 404 before anything is written.
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections/ghost/sets",
                user,
                true,
                Some(serde_json::json!({"name": "s9", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The default transfer is membership-gated by the ownership
    /// column -- another collection's set and a missing set are
    /// 404s; a member transfer lands with its audit row.
    #[tokio::test]
    async fn user_collection_default_transfer_is_membership_gated() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        user_provider_profile(&state, "up1", "pA").await;
        for collection in ["c1", "c2"] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    "/user/collections",
                    user,
                    true,
                    Some(serde_json::json!({"name": collection, "description": "d"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        // s1 in c1 (its default), s2 in c2.
        for (path, name) in [
            ("/user/collections/c1/sets", "s1"),
            ("/user/collections/c2/sets", "s2"),
        ] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    path,
                    user,
                    true,
                    Some(serde_json::json!({"name": name, "description": "d"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        for set_name in ["s2", "ghost"] {
            let (status, _) = send(
                app(state.clone()),
                cookie_req(
                    "PATCH",
                    "/user/collections/c1/default",
                    user,
                    true,
                    Some(serde_json::json!({"set_name": set_name})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "non-member {set_name}");
        }
        // A real transfer: s2 holds c2's anchor from creation, s2b joins
        // as the second member, and the PATCH moves the anchor to it.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections/c2/sets",
                user,
                true,
                Some(serde_json::json!({"name": "s2b", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                "/user/collections/c2/default",
                user,
                true,
                Some(serde_json::json!({"set_name": "s2b"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["default_set"], "s2b");
        let rows = audit_rows(&state.db).await;
        assert!(
            rows.iter()
                .any(|r| r.action == "default" && r.entity == "collection" && r.entity_id == "c2"),
            "the transfer lands its audit row"
        );
    }

    /// The default anchor guards its holder -- deleting the default
    /// set is a 409 naming the transfer route; once the anchor
    /// transfers aside the delete is a plain 200.
    #[tokio::test]
    async fn collection_default_set_deletion_is_guarded() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        user_provider_profile(&state, "up1", "pA").await;
        // c1 with s1 (the anchored default) and s2.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                user,
                true,
                Some(serde_json::json!({"name": "c1", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for name in ["s1", "s2"] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    "/user/collections/c1/sets",
                    user,
                    true,
                    Some(serde_json::json!({"name": name, "description": "d"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        // Deleting the anchored default is a 409 naming the transfer route.
        let (status, body) = send(
            app(state.clone()),
            cookie_req("DELETE", "/user/sets/s1", user, true, None),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("/user/collections/c1/default"),
            "the guard names the transfer route: {body}"
        );
        // Once the anchor transfers to s2 both go through.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                "/user/collections/c1/default",
                user,
                true,
                Some(serde_json::json!({"set_name": "s2"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req("DELETE", "/user/sets/s1", user, true, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// The cascade: deleting a collection removes its anchored sets, their
    /// member rows, sweeps the profiles nothing else in the space
    /// references (sets another collection still holds survive), and
    /// the publication rows go with the collection row; each deletion
    /// lands its audit row and other collections stay untouched.
    #[tokio::test]
    async fn user_collection_deletion_cascades_and_sweeps_profiles() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let owner = "user-plain";
        for (provider, profile) in [("up1", "pA"), ("up2", "pB"), ("up3", "pC")] {
            user_provider_profile(&state, provider, profile).await;
        }
        // c1: sA(pA, pB) published to everyone; c2: sB2(pB) and sC(pC).
        for collection in ["c1", "c2"] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    "/user/collections",
                    user,
                    true,
                    Some(serde_json::json!({"name": collection, "description": "d"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        for (path, name, members) in [
            ("/user/collections/c1/sets", "sA", vec!["pA", "pB"]),
            ("/user/collections/c2/sets", "sB2", vec!["pB"]),
            ("/user/collections/c2/sets", "sC", vec!["pC"]),
        ] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(
                    "POST",
                    path,
                    user,
                    true,
                    Some(serde_json::json!({"name": name, "description": "d", "members": members})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections/c1/publications",
                user,
                true,
                Some(serde_json::json!({"audience": EVERYONE_GROUP_ID})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req("DELETE", "/user/collections/c1", user, true, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let n = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq(owner))
            .filter(profile_set::Column::Name.eq("sA"))
            .count(&state.db)
            .await
            .expect("set count");
        assert_eq!(n, 0, "the anchored set goes with its collection");
        let n = set_member::Entity::find()
            .filter(set_member::Column::Owner.eq(owner))
            .filter(set_member::Column::SetName.eq("sA"))
            .count(&state.db)
            .await
            .expect("member count");
        assert_eq!(n, 0, "member rows cascade with the set");
        let n = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq(owner))
            .filter(profile_set::Column::CollectionName.eq("c2"))
            .count(&state.db)
            .await
            .expect("set count");
        assert_eq!(n, 2, "the surviving collection keeps its anchored sets");
        for (profile, expected) in [("pA", 0), ("pB", 1), ("pC", 1)] {
            let n = profile::Entity::find()
                .filter(profile::Column::Owner.eq(owner))
                .filter(profile::Column::ProfileId.eq(profile))
                .count(&state.db)
                .await
                .expect("profile count");
            assert_eq!(n, expected, "profile {profile}");
        }
        let n = collection_publication::Entity::find()
            .filter(collection_publication::Column::Owner.eq(owner))
            .filter(collection_publication::Column::CollectionName.eq("c1"))
            .count(&state.db)
            .await
            .expect("publication count");
        assert_eq!(n, 0, "publication rows go with the collection");
        let c2 = collection::Entity::find_by_id((owner.to_owned(), "c2".to_owned()))
            .one(&state.db)
            .await
            .expect("collection read")
            .expect("c2 untouched");
        assert_eq!(c2.default_set_name.as_deref(), Some("sB2"));
        let rows = audit_rows(&state.db).await;
        for (entity, id) in [("set", "sA"), ("profile", "pA"), ("collection", "c1")] {
            assert!(
                rows.iter()
                    .any(|r| r.action == "delete" && r.entity == entity && r.entity_id == id),
                "audit row for {entity}/{id}"
            );
        }
    }

    /// The admin default transfer: a set outside the collection
    /// (another collection's member or a missing name) is a 404; a
    /// member transfer lands with the view and its audit row.
    #[tokio::test]
    async fn admin_collection_default_transfer_is_membership_gated() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_BEARER);
        for collection in ["ac7", "ac7b"] {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "POST",
                    "/admin/collections",
                    admin,
                    Some(serde_json::json!({"name": collection, "description": "d"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        for (name, collection) in [("as7a", "ac7"), ("as7b", "ac7"), ("as7c", "ac7b")] {
            let payload = serde_json::json!({"name": name, "description": "d"});
            let (status, body) = send(
                app(state.clone()),
                req(
                    "POST",
                    &format!("/admin/collections/{collection}/sets"),
                    admin,
                    Some(payload),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        for set_name in ["ghost", "as7c"] {
            let (status, _) = send(
                app(state.clone()),
                req(
                    "PATCH",
                    "/admin/collections/ac7/default",
                    admin,
                    Some(serde_json::json!({"set_name": set_name})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "non-member {set_name}");
        }
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/collections/ac7/default",
                admin,
                Some(serde_json::json!({"set_name": "as7b"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["default_set"], "as7b");
        let rows = audit_rows(&state.db).await;
        assert!(
            rows.iter()
                .any(|r| r.action == "default" && r.entity == "collection" && r.entity_id == "ac7"),
            "the transfer lands its audit row"
        );
    }

    /// The admin cascade: deleting a catalog collection removes its set
    /// (with the member rows and the platform publication), sweeps the
    /// profiles the deletion leaves unreferenced while one a surviving
    /// set still holds stays, and leaves the baseline collection's
    /// anchored sets untouched; each swept entity lands its audit row.
    #[tokio::test]
    async fn admin_collection_deletion_cascades_and_sweeps_profiles() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_BEARER);
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections",
                admin,
                Some(serde_json::json!({"name": "cx", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/cx/sets",
                admin,
                Some(serde_json::json!({"name": "sxA", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/sets/sxA",
                admin,
                Some(serde_json::json!({"members": ["p1", "p2"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // p2 leaves alpha: its last reference will be the doomed set.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/sets/alpha",
                admin,
                Some(serde_json::json!({"members": ["p1", "p6"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/cx/publications",
                admin,
                Some(serde_json::json!({"audience": EVERYONE_GROUP_ID})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            req("DELETE", "/admin/collections/cx", admin, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let n = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq("system"))
            .filter(profile_set::Column::Name.eq("sxA"))
            .count(&state.db)
            .await
            .expect("set count");
        assert_eq!(n, 0, "the anchored set goes with its collection");
        let n = set_member::Entity::find()
            .filter(set_member::Column::Owner.eq("system"))
            .filter(set_member::Column::SetName.eq("sxA"))
            .count(&state.db)
            .await
            .expect("member count");
        assert_eq!(n, 0, "member rows cascade with the set");
        // p2's last reference went with the collection; p1 stays (alpha).
        for (profile, expected) in [("p1", 1), ("p2", 0)] {
            let n = profile::Entity::find()
                .filter(profile::Column::Owner.eq("system"))
                .filter(profile::Column::ProfileId.eq(profile))
                .count(&state.db)
                .await
                .expect("profile count");
            assert_eq!(n, expected, "profile {profile}");
        }
        let n = platform_publications::Entity::find()
            .filter(platform_publications::Column::Owner.eq("system"))
            .filter(platform_publications::Column::CollectionName.eq("cx"))
            .count(&state.db)
            .await
            .expect("publication count");
        assert_eq!(n, 0, "the publication goes with the collection");
        let n = profile_set::Entity::find()
            .filter(profile_set::Column::Owner.eq("system"))
            .filter(profile_set::Column::CollectionName.eq("baseline"))
            .count(&state.db)
            .await
            .expect("set count");
        assert_eq!(n, 2, "the baseline anchored sets stay");
        let rows = audit_rows(&state.db).await;
        for (entity, id) in [
            ("profile_set", "sxA"),
            ("profile", "p2"),
            ("collection", "cx"),
        ] {
            assert!(
                rows.iter()
                    .any(|r| r.action == "delete" && r.entity == entity && r.entity_id == id),
                "audit row for {entity}/{id}"
            );
        }
    }

    /// Publishing to a platform group that does not exist is 404:
    /// every non-sentinel id is a miss (no group rows exist).
    #[tokio::test]
    async fn admin_publish_unknown_group_is_404() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections",
                admin,
                true,
                Some(serde_json::json!({"name": "catalog-bundle", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections/catalog-bundle/publications",
                admin,
                true,
                Some(serde_json::json!({"audience": "ghost"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
}
