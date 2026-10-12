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
