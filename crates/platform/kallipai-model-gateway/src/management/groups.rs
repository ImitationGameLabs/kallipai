//! The admin group face: audience groups over the registry space,
//! their members, and the platform-side sentinel groups the
//! publication families read.

use axum::Json;
use axum::extract::{Path, State};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
};

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::AdminToken;
use crate::management::user_routes::{
    UserGroupCreate, UserGroupUpdate, UserGroupView, UserGroupViews,
};
use crate::registry::{self, EVERYONE_GROUP_ID, platform_group_members, platform_groups};
use crate::state::AppState;
use kallipai_common::protocol::ApiError;

/// GET /admin/groups: the platform groups with their (unordered) members.
pub async fn list_groups(
    State(state): State<AppState>,
    _admin: AdminToken,
) -> Result<Json<UserGroupViews>, ApiError> {
    let owner = registry::CATALOG_OWNER.to_string();
    let rows = platform_groups::Entity::find()
        .filter(platform_groups::Column::Owner.eq(owner))
        .order_by_asc(platform_groups::Column::GroupId)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        views.push(platform_group_view(&state.db, row).await?);
    }
    Ok(Json(UserGroupViews { groups: views }))
}

/// GET /admin/groups/{group_id}.
pub async fn get_group(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(group_id): Path<String>,
) -> Result<Json<UserGroupView>, ApiError> {
    let row = platform_group(&state.db, &group_id).await?;
    Ok(Json(platform_group_view(&state.db, row).await?))
}

/// POST /admin/groups: a platform group in the catalog owner's space
/// (the group id is minted here; the name is unique per owner).
pub async fn create_group(
    State(state): State<AppState>,
    admin: AdminToken,
    Json(body): Json<UserGroupCreate>,
) -> Result<Json<UserGroupView>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_string();
    if body.name.is_empty() {
        return Err(ApiError::bad_request("group name must not be empty"));
    }
    if body.name == EVERYONE_GROUP_ID {
        return Err(ApiError::bad_request("the group name is reserved"));
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let clash = platform_groups::Entity::find()
        .filter(platform_groups::Column::Owner.eq(owner.clone()))
        .filter(platform_groups::Column::Name.eq(body.name.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?;
    if clash.is_some() {
        return Err(ApiError::conflict("group already exists"));
    }
    let group_id = uuid::Uuid::new_v4().to_string();
    platform_groups::ActiveModel {
        group_id: Set(group_id.clone()),
        owner: Set(owner),
        name: Set(body.name.clone()),
        ..Default::default()
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    if let Some(members) = &body.members {
        insert_platform_group_members(&txn, &group_id, members).await?;
    }
    let row = platform_group(&txn, &group_id).await?;
    let view = platform_group_view(&txn, row).await?;
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

/// PATCH /admin/groups/{group_id}: `name` and `members` are independent
/// (absent means unchanged). The everyone sentinel rejects the whole
/// request: its name and (empty) membership are migration constants.
pub async fn update_group(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(group_id): Path<String>,
    Json(body): Json<UserGroupUpdate>,
) -> Result<Json<UserGroupView>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_string();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = platform_group(&txn, &group_id).await?;
    if row.group_id == EVERYONE_GROUP_ID {
        return Err(ApiError::conflict("the everyone group is reserved"));
    }
    let before = platform_group_view(&txn, row).await?;
    if let Some(name) = &body.name {
        if name.is_empty() {
            return Err(ApiError::bad_request("group name must not be empty"));
        }
        if name == EVERYONE_GROUP_ID {
            return Err(ApiError::bad_request("the group name is reserved"));
        }
        let clash = platform_groups::Entity::find()
            .filter(platform_groups::Column::Owner.eq(owner.clone()))
            .filter(platform_groups::Column::Name.eq(name.clone()))
            .one(&txn)
            .await
            .map_err(db::map_db_err)?;
        if clash.is_some() {
            return Err(ApiError::conflict("group already exists"));
        }
        let active = platform_group(&txn, &group_id).await?;
        let mut active = platform_groups::ActiveModel::from(active);
        active.name = Set(name.clone());
        active.update(&txn).await.map_err(db::map_db_err)?;
    }
    if let Some(members) = &body.members {
        platform_group_members::Entity::delete_many()
            .filter(platform_group_members::Column::GroupId.eq(group_id.clone()))
            .exec(&txn)
            .await
            .map_err(db::map_db_err)?;
        insert_platform_group_members(&txn, &group_id, members).await?;
    }
    let row = platform_group(&txn, &group_id).await?;
    let after = platform_group_view(&txn, row).await?;
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

/// DELETE /admin/groups/{group_id}: the member and publication rows
/// cascade with it (the platform_groups foreign keys).
pub async fn delete_group(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(group_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = admin.operator.as_str();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = platform_group(&txn, &group_id).await?;
    if row.group_id == EVERYONE_GROUP_ID {
        return Err(ApiError::conflict("the everyone group is reserved"));
    }
    let before = platform_group_view(&txn, row).await?;
    platform_groups::Entity::delete_by_id(group_id.clone())
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

/// POST /admin/groups/{group_id}/members: add one member by account id.
/// The everyone sentinel rejects members: the audience is the predicate,
/// its row carries no member list.
pub async fn add_group_member(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(group_id): Path<String>,
    Json(body): Json<AddGroupMemberBody>,
) -> Result<Json<UserGroupView>, ApiError> {
    let actor = admin.operator.as_str();
    if body.member_account.is_empty() {
        return Err(ApiError::bad_request("member account must not be empty"));
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = platform_group(&txn, &group_id).await?;
    if row.group_id == EVERYONE_GROUP_ID {
        return Err(ApiError::conflict("the everyone group carries no members"));
    }
    let clash =
        platform_group_members::Entity::find_by_id((group_id.clone(), body.member_account.clone()))
            .one(&txn)
            .await
            .map_err(db::map_db_err)?;
    if clash.is_some() {
        return Err(ApiError::conflict("account is already a member"));
    }
    platform_group_members::ActiveModel {
        group_id: Set(group_id.clone()),
        member_account: Set(body.member_account.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    let row = platform_group(&txn, &group_id).await?;
    let view = platform_group_view(&txn, row).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "add_member",
            entity: "group",
            entity_id: group_id.clone(),
            actor,
            before: None,
            after: Some(serde_json::json!({ "member_account": body.member_account })),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(view))
}

/// DELETE /admin/groups/{group_id}/members/{member_account}: remove one
/// member (an absent membership is a 404; the sentinel has none).
pub async fn remove_group_member(
    State(state): State<AppState>,
    admin: AdminToken,
    Path((group_id, member_account)): Path<(String, String)>,
) -> Result<Json<UserGroupView>, ApiError> {
    let actor = admin.operator.as_str();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    platform_group(&txn, &group_id).await?;
    let deleted =
        platform_group_members::Entity::delete_by_id((group_id.clone(), member_account.clone()))
            .exec(&txn)
            .await
            .map_err(db::map_db_err)?
            .rows_affected;
    if deleted == 0 {
        return Err(ApiError::not_found(format!(
            "not a member: {member_account}"
        )));
    }
    let row = platform_group(&txn, &group_id).await?;
    let view = platform_group_view(&txn, row).await?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "remove_member",
            entity: "group",
            entity_id: group_id.clone(),
            actor,
            before: Some(serde_json::json!({ "member_account": member_account })),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(view))
}

/// The request body for adding one group member.
#[derive(Debug, serde::Deserialize)]
pub struct AddGroupMemberBody {
    pub member_account: String,
}

/// The platform group by id, or 404. The everyone sentinel resolves
/// like any other row: the guards above keep it inert.
async fn platform_group<C: sea_orm::ConnectionTrait>(
    db: &C,
    group_id: &str,
) -> Result<platform_groups::Model, ApiError> {
    platform_groups::Entity::find_by_id(group_id.to_owned())
        .one(db)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such group: {group_id}")))
}

async fn insert_platform_group_members(
    txn: &sea_orm::DatabaseTransaction,
    group_id: &str,
    members: &[String],
) -> Result<(), ApiError> {
    for member in members {
        if member.is_empty() {
            return Err(ApiError::bad_request("member account must not be empty"));
        }
        platform_group_members::ActiveModel {
            group_id: Set(group_id.to_owned()),
            member_account: Set(member.clone()),
        }
        .insert(txn)
        .await
        .map_err(db::map_db_err)?;
    }
    Ok(())
}

async fn platform_group_view<C: sea_orm::ConnectionTrait>(
    db: &C,
    row: platform_groups::Model,
) -> Result<UserGroupView, ApiError> {
    let members = platform_group_members::Entity::find()
        .filter(platform_group_members::Column::GroupId.eq(row.group_id.clone()))
        .all(db)
        .await
        .map_err(db::map_db_err)?;
    Ok(UserGroupView {
        group_id: row.group_id,
        name: row.name,
        members: members.into_iter().map(|m| m.member_account).collect(),
    })
}

// -- the account search face ----------------------------------------------

#[cfg(test)]
mod tests {
    use crate::management::testkit::create_group;
    use crate::management::testkit::*;
    use axum::http::StatusCode;
    use sea_orm::ConnectionTrait;
    use sea_orm::Statement;

    /// The reserved name is refused on create and on rename (exact
    /// match: casing variants stay legal, because the audience identity
    /// is the id and the name is a per-owner display label).
    #[tokio::test]
    async fn group_name_everyone_is_reserved() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/groups",
                user,
                true,
                Some(serde_json::json!({"name": "everyone"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/groups",
                user,
                true,
                Some(serde_json::json!({"name": "Everyone"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let group_id = body["group_id"].as_str().expect("group id");
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                &format!("/user/groups/{group_id}"),
                user,
                true,
                Some(serde_json::json!({"name": "everyone"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// A blank member account would store a binding that matches no
    /// session, so create and update both refuse it at the member
    /// insert's choke point.
    #[tokio::test]
    async fn group_members_reject_a_blank_account() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/groups",
                user,
                true,
                Some(serde_json::json!({"name": "g", "members": [""]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/groups",
                user,
                true,
                Some(serde_json::json!({"name": "g"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let group_id = body["group_id"].as_str().expect("group id");
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                &format!("/user/groups/{group_id}"),
                user,
                true,
                Some(serde_json::json!({"members": [""]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// The group CRUD round trip: create with members, list, get,
    /// rename, replace the members, delete; the audit trail carries the
    /// fixed local-admin account on every write.
    #[tokio::test]
    async fn admin_group_crud_round_trip() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        let (status, body) = create_group(
            &state,
            serde_json::json!({"name": "crew", "members": ["acct-1", "acct-2"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let group_id = body["group_id"].as_str().expect("group id").to_owned();
        assert_eq!(body["name"], "crew");
        assert_eq!(body["members"], serde_json::json!(["acct-1", "acct-2"]));
        // Read back: one and list.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "GET",
                &format!("/admin/groups/{group_id}"),
                admin,
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/admin/groups", admin, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.to_string().contains("crew"));
        // Update: name and members are independent.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                &format!("/admin/groups/{group_id}"),
                admin,
                true,
                Some(serde_json::json!({"name": "squad"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["name"], "squad");
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                &format!("/admin/groups/{group_id}"),
                admin,
                true,
                Some(serde_json::json!({"members": ["acct-1"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["members"], serde_json::json!(["acct-1"]));
        // Delete; the row is gone.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "DELETE",
                &format!("/admin/groups/{group_id}"),
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
                &format!("/admin/groups/{group_id}"),
                admin,
                false,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // The audit trail carries the fixed local-admin account.
        let rows = audit_rows(&state.db).await;
        for action in ["create", "update", "delete"] {
            let row = rows
                .iter()
                .find(|r| r.entity == "group" && r.entity_id == group_id && r.action == action)
                .unwrap_or_else(|| panic!("the {action} audit row"));
            assert_eq!(row.actor, "admin");
        }
    }

    /// Name guards: empty and the reserved name refuse on create and on
    /// rename; a same-owner clash is a conflict.
    #[tokio::test]
    async fn admin_group_name_guards_reject() {
        let state = seeded_state().await;
        let (status, body) = create_group(&state, serde_json::json!({"name": ""})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = create_group(&state, serde_json::json!({"name": "everyone"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = create_group(&state, serde_json::json!({"name": "crew"})).await;
        assert_eq!(status, StatusCode::OK);
        let group_id = body["group_id"].as_str().expect("group id").to_owned();
        let (status, body) = create_group(&state, serde_json::json!({"name": "crew"})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        // The rename legs of the same guards.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                "/admin/groups/no-such",
                Some(TEST_ADMIN_COOKIE),
                true,
                Some(serde_json::json!({"name": "x"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                "/admin/groups/no-such",
                Some(TEST_ADMIN_COOKIE),
                true,
                Some(serde_json::json!({"name": ""})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        // Renaming a real group into the reserved name: the
        // update-side leg of the reserved-name guard.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                &format!("/admin/groups/{group_id}"),
                Some(TEST_ADMIN_COOKIE),
                true,
                Some(serde_json::json!({"name": "everyone"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    /// The everyone sentinel is inert: readable (it has a row and a
    /// view), but rename, delete, and member adds all refuse.
    #[tokio::test]
    async fn admin_everyone_sentinel_is_inert() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/admin/groups/everyone", admin, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["group_id"], "everyone");
        assert_eq!(body["members"], serde_json::json!([]));
        for (method, path, body) in [
            (
                "PATCH",
                "/admin/groups/everyone",
                Some(serde_json::json!({"name": "renamed"})),
            ),
            ("DELETE", "/admin/groups/everyone", None),
        ] {
            let (status, body) = send(
                app(state.clone()),
                cookie_req(method, path, admin, true, body),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
        }
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/groups/everyone/members",
                admin,
                true,
                Some(serde_json::json!({"member_account": "acct-1"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    /// The member endpoints: add, duplicate conflict, empty-account
    /// refusal, remove, absent removal 404; an unknown group is 404.
    #[tokio::test]
    async fn admin_group_member_endpoints_round_trip() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        let (status, body) = create_group(&state, serde_json::json!({"name": "crew"})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let group_id = body["group_id"].as_str().expect("group id").to_owned();
        assert_eq!(body["members"], serde_json::json!([]));
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                &format!("/admin/groups/{group_id}/members"),
                admin,
                true,
                Some(serde_json::json!({"member_account": "acct-1"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["members"], serde_json::json!(["acct-1"]));
        // Duplicate: the primary key answers as a conflict.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                &format!("/admin/groups/{group_id}/members"),
                admin,
                true,
                Some(serde_json::json!({"member_account": "acct-1"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        // The empty-account throat check.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                &format!("/admin/groups/{group_id}/members"),
                admin,
                true,
                Some(serde_json::json!({"member_account": ""})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        // Remove; a second remove is a 404.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "DELETE",
                &format!("/admin/groups/{group_id}/members/acct-1"),
                admin,
                true,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["members"], serde_json::json!([]));
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "DELETE",
                &format!("/admin/groups/{group_id}/members/acct-1"),
                admin,
                true,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        // The group-existence gate answers before the membership one.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/groups/no-such/members",
                admin,
                true,
                Some(serde_json::json!({"member_account": "acct-1"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        // The member writes carry the admin attribution.
        let rows = audit_rows(&state.db).await;
        for action in ["add_member", "remove_member"] {
            let row = rows
                .iter()
                .find(|r| r.entity == "group" && r.entity_id == group_id && r.action == action)
                .unwrap_or_else(|| panic!("the {action} audit row"));
            assert_eq!(row.actor, "admin");
        }
    }

    /// Deleting a group takes its publication rows with it (the
    /// platform_groups foreign key), so the collection simply stops
    /// being visible to that audience.
    #[tokio::test]
    async fn admin_group_delete_cascades_publications() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections",
                admin,
                true,
                Some(serde_json::json!({"name": "bundle", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = create_group(&state, serde_json::json!({"name": "crew"})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let group_id = body["group_id"].as_str().expect("group id").to_owned();
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/admin/collections/bundle/publications",
                admin,
                true,
                Some(serde_json::json!({"audience": group_id})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let count = |db: &sea_orm::DatabaseConnection, group: String| {
            let db = db.clone();
            async move {
                let row = db
                    .query_one(Statement::from_string(
                        db.get_database_backend(),
                        format!(
                            "SELECT count(*) AS n FROM platform_publications WHERE group_id = '{group}'"
                        ),
                    ))
                    .await
                    .expect("count query")
                    .expect("aggregate row");
                let n: i64 = row.try_get("", "n").expect("count column");
                n
            }
        };
        assert_eq!(count(&state.db, group_id.clone()).await, 1);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "DELETE",
                &format!("/admin/groups/{group_id}"),
                admin,
                true,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(count(&state.db, group_id.clone()).await, 0);
    }
}
