//! HTTP integration tests for the files service: the ACL matrix over real
//! routes, the send combinations, Range semantics, the body-cap streaming
//! contract, and the invariant that records are born only from real
//! uploads and deliveries.

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
/// Attach credentials: cookie strings ride the `cookie` header, bearer
/// tokens the `authorization` header.
fn with_auth(builder: axum::http::request::Builder, auth: String) -> axum::http::request::Builder {
    match auth.strip_prefix("Bearer ") {
        Some(_) => builder.header("authorization", auth),
        None => builder.header("cookie", auth),
    }
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
        builder = with_auth(builder, auth);
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

async fn bytes_of(response: axum::http::Response<axum::body::Body>) -> Vec<u8> {
    use http_body_util::BodyExt as _;
    response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec()
}

async fn json_of(response: axum::http::Response<axum::body::Body>) -> serde_json::Value {
    let raw = bytes_of(response).await;
    serde_json::from_slice(&raw).expect("json body")
}

fn user1_shared(world: &TestWorld, rest: &str) -> String {
    format!("/users/{}/shared/{rest}", world.user1)
}

async fn put(
    world: &TestWorld,
    auth: String,
    path: &str,
    body: &[u8],
) -> axum::http::Response<axum::body::Body> {
    respond(
        &world.router,
        axum::http::Method::PUT,
        &format!("/?path={path}"),
        Some(auth),
        body.to_vec(),
    )
    .await
}

/// Upload and assert `201 Created`; returns `(record_id, blob_id)`.
async fn put_ok(world: &TestWorld, auth: String, path: &str, body: &[u8]) -> (String, String) {
    let response = put(world, auth, path, body).await;
    assert_eq!(response.status(), StatusCode::CREATED, "PUT {path}");
    let value = json_of(response).await;
    (
        value["record_id"].as_str().expect("record_id").to_owned(),
        value["blob_id"].as_str().expect("blob_id").to_owned(),
    )
}

async fn get(
    world: &TestWorld,
    auth: String,
    record_id: &str,
) -> axum::http::Response<axum::body::Body> {
    respond(
        &world.router,
        axum::http::Method::GET,
        &format!("/{record_id}"),
        Some(auth),
        Vec::new(),
    )
    .await
}

async fn send(
    world: &TestWorld,
    auth: String,
    record_id: &str,
    body: serde_json::Value,
) -> axum::http::Response<axum::body::Body> {
    let request = with_auth(
        Request::builder()
            .method(axum::http::Method::POST)
            .uri(format!("/{record_id}/send"))
            .header("content-type", "application/json"),
        auth,
    )
    .body(axum::body::Body::from(body.to_string()))
    .expect("build send request");
    world
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds")
}

async fn admin_events(world: &TestWorld, blob_id: Option<&str>) -> serde_json::Value {
    let suffix = match blob_id {
        Some(id) => format!("?blob_id={id}"),
        None => String::new(),
    };
    let response = respond(
        &world.router,
        axum::http::Method::GET,
        &format!("/admin/delivery-events{suffix}"),
        Some(bearer(&world.admin_token)),
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    json_of(response).await
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected() {
    let world = TestWorld::new().await;
    let uri = format!("/?path={}", user1_shared(&world, "a.txt"));

    let response = respond(
        &world.router,
        axum::http::Method::PUT,
        &uri,
        None,
        b"data".to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = respond(
        &world.router,
        axum::http::Method::PUT,
        &uri,
        Some(bearer("sk-tagma-bogus")),
        b"data".to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn three_credential_faces_authenticate() {
    let world = TestWorld::new().await;
    let shared = user1_shared(&world, "f.txt");

    // Session cookie (user face).
    let response = put(&world, cookie_for(&world, 1), &shared, b"hello").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let record_id = json_of(response).await["record_id"]
        .as_str()
        .expect("record id")
        .to_owned();

    // Tagma bearer (tagma face): read what the user put in shared.
    let response = get(&world, bearer(&world.t1_token), &record_id).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await, b"hello");

    // Admin bearer (management face): the delivery-events route answers.
    let response = respond(
        &world.router,
        axum::http::Method::GET,
        "/admin/delivery-events",
        Some(bearer(&world.admin_token)),
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn user_reads_and_writes_own_space() {
    let world = TestWorld::new().await;
    let path = user1_shared(&world, "notes.txt");
    let (record_id, _) = put_ok(&world, cookie_for(&world, 1), &path, b"shared bytes").await;

    // The owner reads and heads it.
    let response = get(&world, cookie_for(&world, 1), &record_id).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await, b"shared bytes");
    let head = respond(
        &world.router,
        axum::http::Method::HEAD,
        &format!("/{record_id}"),
        Some(cookie_for(&world, 1)),
        Vec::new(),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["content-length"], "12");
    assert!(bytes_of(head).await.is_empty());

    // Another user has no reach into user1's space (matrix row 4).
    let response = put(&world, cookie_for(&world, 2), &path, b"x").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = get(&world, cookie_for(&world, 2), &record_id).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = respond(
        &world.router,
        axum::http::Method::DELETE,
        &format!("/{record_id}"),
        Some(cookie_for(&world, 2)),
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn tagma_area_matrix() {
    let world = TestWorld::new().await;

    // Row 2: a member writes its own tagma region.
    let own = format!("/users/{}/tagmas/{}/src.txt", world.user1, world.t1);
    let (own_record, _) = put_ok(&world, bearer(&world.t1_token), &own, b"own region").await;
    let response = get(&world, bearer(&world.t1_token), &own_record).await;
    assert_eq!(bytes_of(response).await, b"own region");

    // Row 3: a member writes the space's shared region.
    let shared = user1_shared(&world, "from-tagma.txt");
    put_ok(&world, bearer(&world.t1_token), &shared, b"shared write").await;

    // Row 5: a sibling member's tagma region is out of bounds.
    let foreign = format!("/users/{}/tagmas/{}/other.txt", world.user1, world.t2);
    let response = put(&world, bearer(&world.t1_token), &foreign, b"x").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // The space's inbox region is unreachable for tagmas entirely.
    let inbox = format!("/users/{}/inbox/drop.txt", world.user1);
    let response = put(&world, bearer(&world.t1_token), &inbox, b"x").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn relative_put_lands_in_the_caller_private_region() {
    let world = TestWorld::new().await;

    // A tagma's relative path resolves into its own private region.
    let (record_id, _) = put_ok(&world, bearer(&world.t1_token), "images/pic.png", b"rel").await;
    let response = get(&world, bearer(&world.t1_token), &record_id).await;
    assert_eq!(bytes_of(response).await, b"rel");
    let listing = respond(
        &world.router,
        axum::http::Method::GET,
        "/?space=self&prefix=images/",
        Some(bearer(&world.t1_token)),
        Vec::new(),
    )
    .await;
    let rows = json_of(listing).await;
    let expected = format!("/users/{}/tagmas/{}/images/pic.png", world.user1, world.t1);
    assert_eq!(rows[0]["path"], expected);

    // A user has no private region to resolve into; relative is tagma-only.
    let response = put(&world, cookie_for(&world, 1), "notes/a.txt", b"x").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // `..` never re-points.
    let response = put(&world, bearer(&world.t1_token), "../escape.txt", b"x").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_face_and_admin_content_pins() {
    let world = TestWorld::new().await;
    let path = user1_shared(&world, "a.txt");
    let (record_id, _) = put_ok(&world, cookie_for(&world, 1), &path, b"content").await;

    // Row 8: admin never reads, writes, or deletes content.
    let response = get(&world, bearer(&world.admin_token), &record_id).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = put(&world, bearer(&world.admin_token), &path, b"x").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = respond(
        &world.router,
        axum::http::Method::DELETE,
        &format!("/{record_id}"),
        Some(bearer(&world.admin_token)),
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // And no one but admin reaches the management face.
    for auth in [cookie_for(&world, 1), bearer(&world.t1_token)] {
        let response = respond(
            &world.router,
            axum::http::Method::GET,
            "/admin/delivery-events",
            Some(auth),
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn delete_removes_the_record_only() {
    let world = TestWorld::new().await;
    let (record_id, _) = put_ok(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "gone.txt"),
        b"doomed",
    )
    .await;

    let response = respond(
        &world.router,
        axum::http::Method::DELETE,
        &format!("/{record_id}"),
        Some(cookie_for(&world, 1)),
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = get(&world, cookie_for(&world, 1), &record_id).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = respond(
        &world.router,
        axum::http::Method::DELETE,
        &format!("/{record_id}"),
        Some(cookie_for(&world, 1)),
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn user_to_user_delivery_lands_in_inbox() {
    let world = TestWorld::new().await;
    let (source, blob_id) = put_ok(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "hello.txt"),
        b"for you",
    )
    .await;

    let response = send(
        &world,
        cookie_for(&world, 1),
        &source,
        serde_json::json!({ "to_user": world.user2.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let value = json_of(response).await;
    // Zero copy: the delivery shares the source blob.
    assert_eq!(value["blob_id"], blob_id.as_str());
    assert_eq!(
        value["path"],
        format!("/users/{}/inbox/hello.txt", world.user2)
    );
    let delivered = value["record_id"]
        .as_str()
        .expect("delivered id")
        .to_owned();

    // The recipient owns the inbox copy; the sender cannot read it (row 4).
    let response = get(&world, cookie_for(&world, 2), &delivered).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await, b"for you");
    let response = get(&world, cookie_for(&world, 1), &delivered).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // The delivery event is on the admin ledger, fully addressed.
    let events = admin_events(&world, Some(&blob_id)).await;
    let events = events.as_array().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["from_principal"], world.user1.to_string());
    assert_eq!(events[0]["to_principal"], world.user2.to_string());
    assert_eq!(events[0]["blob_id"], blob_id.as_str());
}

#[tokio::test]
async fn tagma_deliveries_land_in_tagma_inboxes() {
    let world = TestWorld::new().await;

    // U -> T: the user delivers into the tagma's inbox in their own space.
    let (source, _) = put_ok(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "brief.txt"),
        b"brief",
    )
    .await;
    let response = send(
        &world,
        cookie_for(&world, 1),
        &source,
        serde_json::json!({ "to_tagma": world.t1.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let value = json_of(response).await;
    assert_eq!(
        value["path"],
        format!("/users/{}/tagmas/{}/inbox/brief.txt", world.user1, world.t1)
    );
    let delivered = value["record_id"]
        .as_str()
        .expect("delivered id")
        .to_owned();
    let response = get(&world, bearer(&world.t1_token), &delivered).await;
    assert_eq!(bytes_of(response).await, b"brief");

    // T -> T: a member passes a file to a sibling member of the same space.
    let own = format!("/users/{}/tagmas/{}/handoff.txt", world.user1, world.t1);
    let (source, _) = put_ok(&world, bearer(&world.t1_token), &own, b"handoff").await;
    let response = send(
        &world,
        bearer(&world.t1_token),
        &source,
        serde_json::json!({ "to_tagma": world.t2.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let value = json_of(response).await;
    assert_eq!(
        value["path"],
        format!(
            "/users/{}/tagmas/{}/inbox/handoff.txt",
            world.user1, world.t2
        )
    );
    let delivered = value["record_id"]
        .as_str()
        .expect("delivered id")
        .to_owned();

    // The receiving member reads it; the sending one cannot (row 5).
    let response = get(&world, bearer(&world.t2_token), &delivered).await;
    assert_eq!(bytes_of(response).await, b"handoff");
    let response = get(&world, bearer(&world.t1_token), &delivered).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Both deliveries are on the ledger, addressed to the tagmas.
    let events = admin_events(&world, None).await;
    let events = events.as_array().expect("events");
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .any(|e| e["to_principal"] == world.t1.to_string())
    );
    assert!(
        events
            .iter()
            .any(|e| e["to_principal"] == world.t2.to_string())
    );
}

#[tokio::test]
async fn send_validates_targets() {
    let world = TestWorld::new().await;
    let (source, _) = put_ok(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "x.txt"),
        b"x",
    )
    .await;
    let own_tagma = format!("/users/{}/tagmas/{}/x.txt", world.user1, world.t1);
    let (tagma_source, _) = put_ok(&world, bearer(&world.t1_token), &own_tagma, b"x").await;

    // Self-sends are caller mistakes, not deliveries.
    let response = send(
        &world,
        cookie_for(&world, 1),
        &source,
        serde_json::json!({ "to_user": world.user1.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = send(
        &world,
        bearer(&world.t1_token),
        &tagma_source,
        serde_json::json!({ "to_tagma": world.t1.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Exactly one target is required.
    let response = send(
        &world,
        cookie_for(&world, 1),
        &source,
        serde_json::json!({
            "to_user": world.user2.to_string(),
            "to_tagma": world.t1.to_string()
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = send(
        &world,
        cookie_for(&world, 1),
        &source,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A tagma outside the caller's space cannot receive.
    let response = send(
        &world,
        bearer(&world.t1_token),
        &tagma_source,
        serde_json::json!({ "to_tagma": world.t3.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // T -> U does not exist; admin cannot send content.
    let response = send(
        &world,
        bearer(&world.t1_token),
        &tagma_source,
        serde_json::json!({ "to_user": world.user1.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = send(
        &world,
        bearer(&world.admin_token),
        &source,
        serde_json::json!({ "to_user": world.user2.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Any recipient string that is not a live user id mints nothing —
    // user ids are opaque, so malformed and unknown look the same — and
    // no ledger event is written (the undeletable-record hole).
    let stranger = kallip_archeion_common::ids::UserId::from(uuid::Uuid::new_v4().to_string());
    let response = send(
        &world,
        cookie_for(&world, 1),
        &source,
        serde_json::json!({ "to_user": stranger.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let events = admin_events(&world, None).await;
    assert!(events.as_array().expect("events").is_empty());
}

#[tokio::test]
async fn range_requests_serve_slices() {
    let world = TestWorld::new().await;
    let (record_id, _) = put_ok(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "digits.txt"),
        b"0123456789",
    )
    .await;
    let uri = format!("/{record_id}");

    let slice = |range: &'static str| {
        let request = with_auth(
            Request::builder().method(axum::http::Method::GET).uri(&uri),
            cookie_for(&world, 1),
        )
        .header("range", range)
        .body(axum::body::Body::empty())
        .expect("build request");
        world.router.clone().oneshot(request)
    };

    // A closed range serves the slice with a 206 and a content-range.
    let response = slice("bytes=2-5").await.expect("router responds");
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "bytes 2-5/10");
    assert_eq!(bytes_of(response).await, b"2345");

    // Suffix and open-ended ranges clamp to the blob size.
    let response = slice("bytes=-3").await.expect("router responds");
    assert_eq!(response.headers()["content-range"], "bytes 7-9/10");
    assert_eq!(bytes_of(response).await, b"789");
    let response = slice("bytes=7-").await.expect("router responds");
    assert_eq!(response.headers()["content-range"], "bytes 7-9/10");
    assert_eq!(bytes_of(response).await, b"789");

    // An unsatisfiable range is a 416 with the canonical content-range.
    let response = slice("bytes=100-").await.expect("router responds");
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()["content-range"], "bytes */10");

    // Multi-range and malformed specs fall back to the full body.
    let response = slice("bytes=0-1,3-4").await.expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await, b"0123456789");
    let response = slice("bytes=abc").await.expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await, b"0123456789");
}

#[tokio::test]
async fn body_cap_rejects_without_residue() {
    let world = TestWorld::new().await;
    let path = user1_shared(&world, "big.bin");
    let cap = 1024 * 1024usize;

    // Exactly the cap passes; one byte over is a 413.
    let exact = vec![7u8; cap];
    let (record_id, _) = put_ok(&world, cookie_for(&world, 1), &path, &exact).await;
    let response = get(&world, cookie_for(&world, 1), &record_id).await;
    assert_eq!(bytes_of(response).await.len(), cap);

    let oversized = vec![7u8; cap + 1];
    let response = put(&world, cookie_for(&world, 1), &path, &oversized).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // Ingest stages into `<root>/tmp/` and commits by rename, so a
    // rejected stream must leave that directory empty.
    let tmp = world.blob_dir.path().join("tmp");
    let leftovers = std::fs::read_dir(&tmp)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "staging dir must be empty after a 413");
}

#[tokio::test]
async fn dedup_keeps_sibling_records_readable() {
    let world = TestWorld::new().await;
    let content = b"shared content bytes";
    let (first, blob_id) = put_ok(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "one.txt"),
        content,
    )
    .await;
    let (second, second_blob) = put_ok(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "two.txt"),
        content,
    )
    .await;
    assert_ne!(first, second, "two uploads are two records");
    assert_eq!(blob_id, second_blob, "identical bytes share one blob");

    // Deleting one record must not touch the sibling's blob.
    let response = respond(
        &world.router,
        axum::http::Method::DELETE,
        &format!("/{first}"),
        Some(cookie_for(&world, 1)),
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = get(&world, cookie_for(&world, 1), &second).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await, content);
}

#[tokio::test]
async fn service_boots_and_answers_health() {
    // Grab a free port, release it, and race the server to it: the window
    // is tiny and the retry loop tolerates losing the race.
    let addr = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("probe addr")
    };
    let db_url = common::test_db_url().await;
    let blob = tempfile::TempDir::new().expect("blob dir");
    let boot = kallip_files::state::BootConfig {
        listen_addr: addr.to_string(),
        database_url: db_url,
        archeion_internal_url: "http://127.0.0.1:1".to_owned(),
        archeion_internal_token: "unused".to_owned(),
        notify_url: String::new(),
        notify_token: String::new(),
        blob_root: std::path::PathBuf::from(blob.path()),
        files: kallip_files::state::FilesConfig {
            max_body_bytes: 1024 * 1024,
            cors_origins: String::new(),
            degrade_fail_soft: false,
            gc: kallip_files::gc::GcConfig::default(),
        },
    };
    let handle = tokio::spawn(async move {
        kallip_files::state::run(boot).await.expect("service runs");
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/health");
    let mut healthy = false;
    for _ in 0..50 {
        if let Ok(response) = client.get(&url).send().await
            && response.status().as_u16() == 200
        {
            healthy = response.text().await.map(|b| b == "ok").unwrap_or(false);
            if healthy {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    handle.abort();
    assert!(healthy, "service never answered {url}");
}
/// A store wrapper recording the largest `get_range` window and the window
/// count: evidence that streaming serves through bounded windows instead of
/// one whole-blob read.
struct WindowRecorder {
    inner: kallip_files::LocalBackend,
    max_window: std::sync::atomic::AtomicUsize,
    windows: std::sync::atomic::AtomicUsize,
}

impl WindowRecorder {
    fn new(root: &std::path::Path) -> Self {
        Self {
            inner: kallip_files::LocalBackend::new(root),
            max_window: std::sync::atomic::AtomicUsize::new(0),
            windows: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl kallip_files::BlobStore for WindowRecorder {
    async fn put(
        &self,
        content: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    ) -> Result<kallip_files::BlobId, kallip_files::Error> {
        self.inner.put(content).await
    }

    async fn get(&self, id: &kallip_files::BlobId) -> Result<Vec<u8>, kallip_files::Error> {
        self.inner.get(id).await
    }

    async fn get_range(
        &self,
        id: &kallip_files::BlobId,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, kallip_files::Error> {
        self.max_window
            .fetch_max(len as usize, std::sync::atomic::Ordering::SeqCst);
        self.windows
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get_range(id, offset, len).await
    }

    async fn stat(
        &self,
        id: &kallip_files::BlobId,
    ) -> Result<Option<kallip_files::BlobInfo>, kallip_files::Error> {
        self.inner.stat(id).await
    }

    async fn delete(&self, id: &kallip_files::BlobId) -> Result<(), kallip_files::Error> {
        self.inner.delete(id).await
    }
}

#[tokio::test]
async fn body_cap_survives_large_frames() {
    // A frame larger than both the 64 KiB reader buffer and the remaining
    // allowance walks the pending path with a small remainder; unclamped,
    // the u64 remainder counter would wrap (panic in debug, cap escape in
    // release). A 200 KiB frame against a 100 KiB cap puts
    // the first read exactly in that window.
    let world = TestWorld::with_cap(100 * 1024).await;
    let oversized = vec![9u8; 200 * 1024];
    let response = put(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "f.bin"),
        &oversized,
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // The rejected stream leaves no staging leftovers.
    let tmp = world.blob_dir.path().join("tmp");
    let leftovers = std::fs::read_dir(&tmp)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "staging dir must be empty after a 413");
}

#[tokio::test]
async fn revocation_takes_effect_on_the_next_request() {
    // Verification is per request with no cache: a session valid for one
    // upload must be rejected by the very next one after revocation.
    let world = TestWorld::new().await;
    let path = user1_shared(&world, "revoked.txt");
    let response = put(&world, cookie_for(&world, 1), &path, b"one").await;
    assert_eq!(response.status(), StatusCode::CREATED);

    world.mock.revoke_session(&world.user1_cookie);
    let response = put(&world, cookie_for(&world, 1), &path, b"two").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn streaming_reads_stay_windowed() {
    // Evidence for bounded memory on the serving side: a 1 MiB GET is
    // streamed through 128 KiB store windows, never one whole-blob read.
    let blob_dir = tempfile::TempDir::new().expect("blob dir");
    let blob_root = blob_dir.path().to_path_buf();
    let recorder = WindowRecorder::new(blob_dir.path());
    let recorder = std::sync::Arc::new(recorder);
    let world = TestWorld::with_parts(
        recorder.clone() as std::sync::Arc<dyn kallip_files::BlobStore>,
        blob_root,
        1024 * 1024,
        blob_dir,
        None,
    )
    .await;

    let big = vec![3u8; 1024 * 1024];
    let (record_id, _) = put_ok(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "big.bin"),
        &big,
    )
    .await;
    let response = get(&world, cookie_for(&world, 1), &record_id).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await.len(), big.len());

    assert!(
        recorder
            .max_window
            .load(std::sync::atomic::Ordering::SeqCst)
            <= 128 * 1024,
        "serving must read in bounded windows"
    );
    assert!(
        recorder.windows.load(std::sync::atomic::Ordering::SeqCst) >= 8,
        "a 1 MiB blob must not be served in a single read"
    );
}

// --- notify push contract regression guard ---

/// Records every push the send transaction hands over.
type PushLog = std::sync::Arc<std::sync::Mutex<Vec<PushRecord>>>;

/// One recorded push: recipient, record, landing path, sender, name, size.
type PushRecord = (String, uuid::Uuid, String, String, String, u64);
#[derive(Default, Clone)]
struct NotifySpy(PushLog);

#[async_trait::async_trait]
impl kallip_files::notify::NotifyPusher for NotifySpy {
    async fn file_delivered(
        &self,
        to_user: &str,
        record_id: uuid::Uuid,
        path: &str,
        from: &str,
        name: &str,
        size: u64,
    ) {
        self.0.lock().unwrap().push((
            to_user.to_owned(),
            record_id,
            path.to_owned(),
            from.to_owned(),
            name.to_owned(),
            size,
        ));
    }
}

/// The disabled posture never builds a client at all: an unset URL/token
/// yields None (the safe default the fixtures all ride on).
#[test]
fn disabled_notify_builds_no_client() {
    assert!(kallip_files::notify::LescheNotifyClient::new(String::new(), "s".into()).is_none());
    assert!(
        kallip_files::notify::LescheNotifyClient::new("http://x".into(), String::new()).is_none()
    );
}

/// A successful send must trigger exactly one push carrying the delivery
/// facts (recipient, record, landing path, sender, name, size). The push
/// is fire-and-forget, so the test polls briefly for the spawned task.
#[tokio::test]
async fn send_pushes_the_delivery_event_once() {
    let spy = NotifySpy::default();
    let blob_dir = tempfile::TempDir::new().expect("blob dir");
    let blob_root = blob_dir.path().to_path_buf();
    let world = TestWorld::with_parts(
        kallip_files::LocalBackend::arc(&blob_root),
        blob_root,
        1024 * 1024,
        blob_dir,
        Some(std::sync::Arc::new(spy.clone()) as _),
    )
    .await;
    let (source, _blob_id) = put_ok(
        &world,
        cookie_for(&world, 1),
        &user1_shared(&world, "hello.txt"),
        b"for you",
    )
    .await;
    let response = send(
        &world,
        cookie_for(&world, 1),
        &source,
        serde_json::json!({ "to_user": world.user2.to_string() }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let value = json_of(response).await;
    for _ in 0..40 {
        if !spy.0.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let pushes = spy.0.lock().unwrap();
    assert_eq!(pushes.len(), 1);
    let (to_user, record_id, path, from, name, size) = &pushes[0];
    assert_eq!(to_user, &world.user2.to_string());
    assert_eq!(
        record_id.to_string(),
        value["record_id"].as_str().expect("id")
    );
    assert_eq!(path, &format!("/users/{}/inbox/hello.txt", world.user2));
    assert_eq!(from, &world.user1.to_string());
    assert_eq!(name, "hello.txt");
    assert_eq!(*size, 7);
}

/// A lesche that answers `delivered: false` (recipient offline) gets its
/// single request, and the client returns without error and without a
/// retry -- the delivery itself is already committed (the compensation
/// posture: the recipient discovers the file via the listing surface).
#[tokio::test]
async fn lesche_delivered_false_answer_is_tolerated() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = axum::Router::new().route(
        "/internal/file-delivered",
        axum::routing::post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({ "delivered": false }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client =
        kallip_files::notify::LescheNotifyClient::new(format!("http://{addr}"), "s".into())
            .unwrap();
    kallip_files::notify::NotifyPusher::file_delivered(
        &client,
        "u2",
        uuid::Uuid::new_v4(),
        "/users/u2/inbox/x",
        "u1",
        "x",
        1,
    )
    .await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
