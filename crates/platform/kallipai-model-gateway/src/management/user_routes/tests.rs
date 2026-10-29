//! The `/user` face's end-to-end suite: one seeded state driven through the
//! router, provider to collection publication and back.

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
    // The scratch collection lands first: a profile belongs to exactly
    // one collection, so the row below anchors where its set will live.
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
    // Profile referencing the provider (the family rides along).
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "POST",
            "/user/collections/scratch/profiles",
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
    assert_eq!(body["collection_name"], "scratch");
    // A profile pointing at a provider outside the space is 404.
    let (status, _) = send(
        app(state.clone()),
        cookie_req(
            "POST",
            "/user/collections/scratch/profiles",
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
    // s0 holds the collection's anchor so "mine" can travel.
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
    // Set with the profile as its member: the set is born empty, and the
    // membership is the profile row's own write.
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "POST",
            "/user/collections/scratch/sets",
            user,
            true,
            Some(serde_json::json!({"name": "mine", "description": "d"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["members"], serde_json::json!([]));
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "PUT",
            "/user/collections/scratch/profiles/p-user",
            user,
            true,
            Some(serde_json::json!({"model": "deepseek-chat", "set": "mine"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["set"], "mine");
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "GET",
            "/user/collections/scratch/sets/mine",
            user,
            false,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["members"], serde_json::json!(["p-user"]));
    // A second row in the same collection stays unassigned; the counts the
    // delete confirmation states carry both rows.
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "POST",
            "/user/collections/scratch/profiles",
            user,
            true,
            Some(serde_json::json!({
                "profile_id": "p-unassigned",
                "provider_id": "up1",
                "model": "deepseek-chat"
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        app(state.clone()),
        cookie_req("GET", "/user/collections/scratch", user, false, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["set_count"], 2, "{body}");
    assert_eq!(body["profile_count"], 2, "{body}");
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
    // The delete takes the collection and its rows with it: the list no
    // longer carries it, so the counts it stated are spent.
    let (status, body) = send(
        app(state.clone()),
        cookie_req("DELETE", "/user/collections/scratch", user, true, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        app(state.clone()),
        cookie_req("GET", "/user/collections", user, false, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["collections"]
        .as_array()
        .expect("collection list")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(!names.contains(&"scratch"), "{body}");
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

/// The membership write is scoped to the path's collection: a row that lives
/// in another collection cannot be addressed through this one, and it keeps
/// its state while the refusal is answered.
#[tokio::test]
async fn user_membership_writes_are_scoped_to_the_paths_collection() {
    let state = seeded_state().await;
    let user = Some(TEST_USER_COOKIE);
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
                "api_key": "sk-user-x"
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for name in ["scratch", "other"] {
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/collections",
                user,
                true,
                Some(serde_json::json!({"name": name, "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "POST",
            "/user/collections/other/profiles",
            user,
            true,
            Some(serde_json::json!({
                "profile_id": "p-other",
                "provider_id": "up1",
                "model": "m"
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
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
    assert_eq!(status, StatusCode::OK, "{body}");
    // The row lives in `other`; the scratch path has no such row.
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "PUT",
            "/user/collections/scratch/profiles/p-other",
            user,
            true,
            Some(serde_json::json!({"model": "m", "set": "s0"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("no such profile: p-other in collection scratch"),
        "{body}"
    );
    // The refused write joined nothing.
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "GET",
            "/user/collections/scratch/sets/s0",
            user,
            false,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["members"], serde_json::json!([]));
}

/// The set face refuses a membership payload here too: membership is the
/// profile PUT's `set` field and order is the set's `/order` route, so a
/// body that carries `members` lands nothing.
#[tokio::test]
async fn user_set_writes_refuse_a_members_payload() {
    let state = seeded_state().await;
    let user = Some(TEST_USER_COOKIE);
    for (path, payload) in [
        (
            "/user/providers",
            serde_json::json!({"provider_id": "up1", "family": "deepseek", "api_key": "sk-x"}),
        ),
        (
            "/user/collections",
            serde_json::json!({"name": "scratch", "description": "d"}),
        ),
        (
            "/user/collections/scratch/profiles",
            serde_json::json!({"profile_id": "p-a", "provider_id": "up1", "model": "m"}),
        ),
    ] {
        let (status, body) = send(
            app(state.clone()),
            cookie_req("POST", path, user, true, Some(payload)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    // Create with members: 400, and the set row does not land.
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "POST",
            "/user/collections/scratch/sets",
            user,
            true,
            Some(serde_json::json!({"name": "s9", "description": "d", "members": ["p-a"]})),
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
        cookie_req("GET", "/user/sets", user, false, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sets"], serde_json::json!([]), "{body}");
    // A patch carrying members is refused too, and the set keeps both its
    // description and its order.
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "POST",
            "/user/collections/scratch/sets",
            user,
            true,
            Some(serde_json::json!({"name": "s1", "description": "d"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "PUT",
            "/user/collections/scratch/profiles/p-a",
            user,
            true,
            Some(serde_json::json!({"model": "m", "set": "s1"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "PATCH",
            "/user/collections/scratch/sets/s1",
            user,
            true,
            Some(serde_json::json!({"description": "rewritten", "members": ["p-a"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "GET",
            "/user/collections/scratch/sets/s1",
            user,
            false,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["description"], "d", "{body}");
    assert_eq!(body["members"], serde_json::json!(["p-a"]), "{body}");
}

/// The user face audits the set a profile's membership leaves and joins:
/// the join appends the row to the target's order, the delete drops it
/// from the source's.
#[tokio::test]
async fn user_membership_writes_audit_the_set_orders() {
    let state = seeded_state().await;
    let user = Some(TEST_USER_COOKIE);
    for (path, payload) in [
        (
            "/user/providers",
            serde_json::json!({"provider_id": "up1", "family": "deepseek", "api_key": "sk-x"}),
        ),
        (
            "/user/collections",
            serde_json::json!({"name": "scratch", "description": "d"}),
        ),
        (
            "/user/collections/scratch/profiles",
            serde_json::json!({"profile_id": "p-a", "provider_id": "up1", "model": "m"}),
        ),
        (
            "/user/collections/scratch/sets",
            serde_json::json!({"name": "s1", "description": "d"}),
        ),
    ] {
        let (status, body) = send(
            app(state.clone()),
            cookie_req("POST", path, user, true, Some(payload)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "PUT",
            "/user/collections/scratch/profiles/p-a",
            user,
            true,
            Some(serde_json::json!({"model": "m", "set": "s1"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "DELETE",
            "/user/collections/scratch/profiles/p-a",
            user,
            true,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = audit_rows(&state.db).await;
    let set_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.action == "update" && r.entity == "profile_set" && r.entity_id == "s1")
        .collect();
    assert_eq!(set_rows.len(), 2);
    assert_eq!(
        set_rows[0].before.as_ref().expect("join before")["members"],
        serde_json::json!([])
    );
    assert_eq!(
        set_rows[0].after.as_ref().expect("join after")["members"],
        serde_json::json!(["p-a"])
    );
    assert_eq!(
        set_rows[1].before.as_ref().expect("delete before")["members"],
        serde_json::json!(["p-a"])
    );
    assert_eq!(
        set_rows[1].after.as_ref().expect("delete after")["members"],
        serde_json::json!([])
    );
    assert!(set_rows.iter().all(|r| r.actor == "user-plain"));
}
/// The user query shapes are closed: a key the face does not read answers
/// 400 instead of being ignored.
#[tokio::test]
async fn user_query_shapes_refuse_unknown_keys() {
    let state = seeded_state().await;
    let user = Some(TEST_USER_COOKIE);
    for path in [
        "/user/profiles?collection_name=own-col&parked=true",
        "/user/sets?collection_name=own-col&set=s1",
    ] {
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", path, user, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }
}

/// The user `/order` route answers through the same protocol as the admin
/// one: a list that is not a permutation of the membership is a 400, and a
/// set the caller has no row for is a 404.
#[tokio::test]
async fn user_set_order_refuses_a_non_permutation_and_a_missing_set() {
    let state = seeded_state().await;
    let user = Some(TEST_USER_COOKIE);
    for (path, payload) in [
        (
            "/user/providers",
            serde_json::json!({"provider_id": "up1", "family": "deepseek", "api_key": "sk-x"}),
        ),
        (
            "/user/collections",
            serde_json::json!({"name": "scratch", "description": "d"}),
        ),
        (
            "/user/collections/scratch/profiles",
            serde_json::json!({"profile_id": "p-a", "provider_id": "up1", "model": "m"}),
        ),
        (
            "/user/collections/scratch/sets",
            serde_json::json!({"name": "s1", "description": "d"}),
        ),
    ] {
        let (status, body) = send(
            app(state.clone()),
            cookie_req("POST", path, user, true, Some(payload)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "PUT",
            "/user/collections/scratch/profiles/p-a",
            user,
            true,
            Some(serde_json::json!({"model": "m", "set": "s1"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "PUT",
            "/user/collections/scratch/sets/s1/order",
            user,
            true,
            Some(serde_json::json!({"members": ["p-a", "p-b"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = send(
        app(state.clone()),
        cookie_req(
            "PUT",
            "/user/collections/scratch/sets/gone/order",
            user,
            true,
            Some(serde_json::json!({"members": []})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}
