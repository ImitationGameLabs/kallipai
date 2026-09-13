//! Integration pins for the listing route (GET /): the grant
//! prefix derives from the caller's identity, the per-row matrix decision
//! is the same one the single-record routes make, the prefix matches
//! verbatim (the LIKE metacharacters included), the page cap is pinned,
//! and a tagma's listing reaches its own region plus the shared region --
//! never anyone else's, in its own space or out of it.

// This suite exercises a subset of the shared harness (no revocation,
// custom cap, or direct mock probing); the rest of `common` stays put
// for api_tests, so the unused remainder is allowed here.
#[allow(dead_code)]
mod common;

use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::TestWorld;

fn cookie_for(world: &TestWorld, which: usize) -> String {
    let cookie = if which == 1 {
        &world.user1_cookie
    } else {
        &world.user2_cookie
    };
    format!("kallip_session={cookie}")
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

async fn respond(
    router: &axum::Router,
    method: axum::http::Method,
    uri: &str,
    auth: Option<String>,
    body: Vec<u8>,
) -> axum::http::Response<axum::body::Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        builder = match auth.strip_prefix("Bearer ") {
            Some(_) => builder.header("authorization", auth),
            None => builder.header("cookie", auth),
        };
    }
    let request = builder
        .body(axum::body::Body::from(body))
        .expect("build request");
    router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds")
}

async fn body_of(response: axum::http::Response<axum::body::Body>) -> Vec<u8> {
    use http_body_util::BodyExt as _;
    response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec()
}

/// Upload and assert `201 Created`; returns the record id.
async fn put_ok(world: &TestWorld, auth: &str, path: &str, body: &[u8]) -> String {
    let response = respond(
        &world.router,
        axum::http::Method::PUT,
        &format!("/?path={path}"),
        Some(auth.to_owned()),
        body.to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED, "PUT {path}");
    let value: serde_json::Value =
        serde_json::from_slice(&body_of(response).await).expect("json body");
    value["record_id"].as_str().expect("record_id").to_owned()
}

/// List and assert `200`; returns the JSON array.
async fn list_ok(world: &TestWorld, auth: &str, query: &str) -> Vec<serde_json::Value> {
    let response = respond(
        &world.router,
        axum::http::Method::GET,
        &format!("/?{query}"),
        Some(auth.to_owned()),
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "GET /?{query}");
    let value: serde_json::Value =
        serde_json::from_slice(&body_of(response).await).expect("json body");
    value.as_array().expect("array").clone()
}
/// POST .../send with the JSON content type the route requires; asserts
/// `201 Created`.
async fn send_to_tagma_ok(world: &TestWorld, auth: &str, record_id: &str, tagma: &str) {
    let request = Request::builder()
        .method(axum::http::Method::POST)
        .uri(format!("/{record_id}/send"))
        .header("content-type", "application/json")
        .header("cookie", auth)
        .body(axum::body::Body::from(
            format!("{{\"to_tagma\": \"{tagma}\"}}").into_bytes(),
        ))
        .expect("build send request");
    let sent = world
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    assert_eq!(
        sent.status(),
        StatusCode::CREATED,
        "send {record_id} -> {tagma}"
    );
}

fn paths_of(entries: &[serde_json::Value]) -> Vec<&str> {
    entries
        .iter()
        .map(|e| e["path"].as_str().expect("path"))
        .collect()
}

#[tokio::test]
async fn shared_listing_is_scoped_to_the_callers_own_space() {
    let world = TestWorld::new().await;
    let u1 = cookie_for(&world, 1);
    let u2 = cookie_for(&world, 2);
    let user1_shared = |rest: &str| format!("/users/{}/shared/{rest}", world.user1);
    let user2_shared = |rest: &str| format!("/users/{}/shared/{rest}", world.user2);

    put_ok(&world, &u1, &user1_shared("a.txt"), b"a").await;
    put_ok(&world, &u1, &user1_shared("b.txt"), b"bb").await;
    put_ok(&world, &u2, &user2_shared("c.txt"), b"ccc").await;

    let mine = list_ok(&world, &u1, "space=shared").await;
    assert_eq!(
        paths_of(&mine),
        vec![
            user1_shared("a.txt").as_str(),
            user1_shared("b.txt").as_str()
        ]
    );

    // The other user's shared rows are in a different space: never listed.
    let theirs = list_ok(&world, &u2, "space=shared").await;
    assert_eq!(paths_of(&theirs), vec![user2_shared("c.txt").as_str()]);

    // Sizes come from the content catalog (same transaction as the rows).
    assert_eq!(mine[0]["size"].as_i64(), Some(1));
    assert_eq!(mine[1]["size"].as_i64(), Some(2));
}

#[tokio::test]
async fn user_self_listing_spans_the_whole_own_space_including_inbox() {
    let world = TestWorld::new().await;
    let u1 = cookie_for(&world, 1);
    let user1_shared = |rest: &str| format!("/users/{}/shared/{rest}", world.user1);

    put_ok(&world, &u1, &user1_shared("kept.txt"), b"k").await;
    let gift = put_ok(&world, &u1, &user1_shared("gift.txt"), b"g").await;
    // Deliver a copy into t1's inbox (row 9's landing): the owner's own
    // listing must show the landing row too -- the inbox is part of their
    // space (row 1).
    send_to_tagma_ok(&world, &u1, &gift, world.t1.as_ref()).await;

    let rows = list_ok(&world, &u1, "space=self").await;
    let paths = paths_of(&rows);
    assert!(paths.contains(&user1_shared("kept.txt").as_str()));
    assert!(
        paths
            .iter()
            .any(|p| p.contains(&format!("/tagmas/{}/inbox/gift.txt", world.t1))),
        "inbox landing visible to the owner: {paths:?}"
    );
}

#[tokio::test]
async fn tagma_listing_reaches_own_region_and_shared_never_others() {
    let world = TestWorld::new().await;
    let t1 = bearer(&world.t1_token);
    let t2 = bearer(&world.t2_token);
    let t1_region = |rest: &str| format!("/users/{}/tagmas/{}/{}", world.user1, world.t1, rest);
    let t2_region = |rest: &str| format!("/users/{}/tagmas/{}/{}", world.user1, world.t2, rest);
    let shared = |rest: &str| format!("/users/{}/shared/{}", world.user1, rest);

    put_ok(&world, &t1, &t1_region("mine.txt"), b"m").await;
    put_ok(&world, &t1, &shared("common.txt"), b"c").await;
    put_ok(&world, &t2, &t2_region("theirs.txt"), b"t").await;

    // t1's private face: its own region only (never t2's region).
    let rows = list_ok(&world, &t1, "space=self").await;
    assert_eq!(paths_of(&rows), vec![t1_region("mine.txt").as_str()]);

    // t1's shared face: the shared region (row 3 grants read across the
    // whole region; the owner field only narrows deletes).
    let rows = list_ok(&world, &t1, "space=shared").await;
    assert_eq!(paths_of(&rows), vec![shared("common.txt").as_str()]);

    // t2's private face shows its own row, never t1's.
    let rows = list_ok(&world, &t2, "space=self").await;
    assert_eq!(paths_of(&rows), vec![t2_region("theirs.txt").as_str()]);
}

#[tokio::test]
async fn tagma_sees_its_inbox_landing_in_the_self_listing() {
    let world = TestWorld::new().await;
    let u1 = cookie_for(&world, 1);
    let t1 = bearer(&world.t1_token);
    let shared = |rest: &str| format!("/users/{}/shared/{}", world.user1, rest);

    let gift = put_ok(&world, &u1, &shared("memo.txt"), b"m").await;
    send_to_tagma_ok(&world, &u1, &gift, world.t1.as_ref()).await;

    // The private face includes the tagma's inbox (the region prefix
    // covers it), so a delivery is discoverable without a per-area route.
    let rows = list_ok(&world, &t1, "space=self").await;
    let paths = paths_of(&rows);
    assert!(
        paths
            .iter()
            .any(|p| p.contains(&format!("/tagmas/{}/inbox/memo.txt", world.t1))),
        "inbox landing visible to the tagma: {paths:?}"
    );
}

#[tokio::test]
async fn admin_listing_is_refused_like_every_content_face() {
    let world = TestWorld::new().await;
    let response = respond(
        &world.router,
        axum::http::Method::GET,
        "/?space=shared",
        Some(bearer(&world.admin_token)),
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn bad_space_or_absolute_prefix_is_a_400() {
    let world = TestWorld::new().await;
    let u1 = cookie_for(&world, 1);
    for query in ["space=bogus", "space=shared&prefix=%2Fusers"] {
        let response = respond(
            &world.router,
            axum::http::Method::GET,
            &format!("/?{query}"),
            Some(u1.clone()),
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }
}

#[tokio::test]
async fn prefix_narrows_within_the_grant_and_ordering_is_path_ascending() {
    let world = TestWorld::new().await;
    let u1 = cookie_for(&world, 1);
    let user1_shared = |rest: &str| format!("/users/{}/shared/{}", world.user1, rest);

    put_ok(&world, &u1, &user1_shared("docs/b.txt"), b"1").await;
    put_ok(&world, &u1, &user1_shared("docs/a.txt"), b"2").await;
    put_ok(&world, &u1, &user1_shared("root.txt"), b"3").await;

    let rows = list_ok(&world, &u1, "space=shared&prefix=docs/").await;
    // Path-ascending within the narrowed set.
    assert_eq!(
        paths_of(&rows),
        vec![
            user1_shared("docs/a.txt").as_str(),
            user1_shared("docs/b.txt").as_str()
        ]
    );
}

#[tokio::test]
async fn limit_caps_the_page() {
    let world = TestWorld::new().await;
    let u1 = cookie_for(&world, 1);
    let user1_shared = |rest: &str| format!("/users/{}/shared/{}", world.user1, rest);

    for name in ["one", "three", "two"] {
        put_ok(&world, &u1, &user1_shared(name), b"x").await;
    }

    // A smaller page is honored (a path-ascending prefix of the full list).
    let rows = list_ok(&world, &u1, "space=shared&limit=2").await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["path"].as_str().unwrap(), user1_shared("one"));

    // Asking for the sky still works; the clamp itself is pinned below.
    let rows = list_ok(&world, &u1, "space=shared&limit=99999").await;
    assert_eq!(rows.len(), 3);
}

#[test]
fn limit_clamp_is_pinned_to_the_constant() {
    assert_eq!(kallip_files::api::list::clamp_limit(None), 200);
    assert_eq!(kallip_files::api::list::clamp_limit(Some(7)), 7);
    assert_eq!(
        kallip_files::api::list::clamp_limit(Some(u64::MAX)),
        kallip_files::api::list::MAX_LIST_LIMIT
    );
}

#[tokio::test]
async fn prefix_matches_verbatim_including_like_metacharacters() {
    let world = TestWorld::new().await;
    let u1 = cookie_for(&world, 1);
    let user1_shared = |rest: &str| format!("/users/{}/shared/{}", world.user1, rest);

    put_ok(&world, &u1, &user1_shared("my_repo/a.txt"), b"1").await;
    put_ok(&world, &u1, &user1_shared("my-repo/b.txt"), b"2").await;
    put_ok(&world, &u1, &user1_shared("50%/c.txt"), b"3").await;
    put_ok(&world, &u1, &user1_shared("50x/d.txt"), b"4").await;

    // `_` matches itself, not any character: the my_repo row only.
    let rows = list_ok(&world, &u1, "space=shared&prefix=my_repo").await;
    assert_eq!(
        paths_of(&rows),
        vec![user1_shared("my_repo/a.txt").as_str()]
    );

    // `%` matches itself too (sent URL-encoded, as any client must).
    let rows = list_ok(&world, &u1, "space=shared&prefix=50%25").await;
    assert_eq!(paths_of(&rows), vec![user1_shared("50%/c.txt").as_str()]);
}

#[tokio::test]
async fn tagma_of_another_space_lists_only_its_own_shared_region() {
    let world = TestWorld::new().await;
    let t3 = bearer(&world.t3_token);
    let user2_shared = |rest: &str| format!("/users/{}/shared/{}", world.user2, rest);
    let user1_shared = |rest: &str| format!("/users/{}/shared/{}", world.user1, rest);

    put_ok(
        &world,
        &cookie_for(&world, 2),
        &user2_shared("u2-note.txt"),
        b"a",
    )
    .await;
    put_ok(
        &world,
        &cookie_for(&world, 1),
        &user1_shared("u1-note.txt"),
        b"b",
    )
    .await;

    // t3 is enrolled in user2's space: its shared face is user2's shared
    // region -- user1's rows live in another space and never appear.
    let rows = list_ok(&world, &t3, "space=shared").await;
    assert_eq!(paths_of(&rows), vec![user2_shared("u2-note.txt").as_str()]);
}
