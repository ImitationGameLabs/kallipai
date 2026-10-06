//! The data-plane end-to-end tests: every test drives a real pingora
//! listener (a scratch port per test) against an in-process mock
//! upstream, the way a client would -- no oneshot shortcuts, so the
//! phase wiring itself is under test. The assertion set is the
//! forwarding suite's.

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::spawn_data_plane;
use crate::state::AppState;
use crate::test_support::{TEST_TAGMA_BEARER, UPSTREAM_KEY, migrated_test_db};

/// One scratch port per test: the suite runs in one process and the
/// ports are handed out from a private, colliding-with-nothing range.
static PORT: AtomicU16 = AtomicU16::new(17720);

fn next_addr() -> String {
    let port = PORT.fetch_add(1, Ordering::SeqCst);
    format!("127.0.0.1:{port}")
}

fn wait_for_listener(addr: &str) {
    for _ in 0..200 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("listener {addr} never came up");
}

/// The shared app state over the given store.
fn test_state(db: crate::db::Db) -> AppState {
    AppState {
        db,
        public_base_url: "http://gw.test:7501".to_string(),
        management: crate::test_support::test_management(),
        identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
        metrics: crate::metrics::Metrics::private(),
    }
}

/// Spawn the data plane on a scratch port over the given store; returns
/// the base URL. The CORS allowlist is empty (the origin pins live in
/// the dedicated tests).
async fn spawn_plane(db: crate::db::Db) -> String {
    let addr = next_addr();
    spawn_data_plane(addr.clone(), test_state(db), String::new()).expect("spawn data plane");
    wait_for_listener(&addr);
    format!("http://{addr}")
}

/// The same plane with a metrics handle the test can read: the handle
/// shares the collectors the phases write, so assertions read what the
/// phases recorded.
async fn spawn_plane_with_metrics(db: crate::db::Db) -> (String, crate::metrics::Metrics) {
    let addr = next_addr();
    let metrics = crate::metrics::Metrics::private();
    let state = AppState {
        db,
        public_base_url: "http://gw.test:7501".to_string(),
        management: crate::test_support::test_management(),
        identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
        metrics: metrics.clone(),
    };
    spawn_data_plane(addr.clone(), state, String::new()).expect("spawn data plane");
    wait_for_listener(&addr);
    (format!("http://{addr}"), metrics)
}

/// Await a forward-outcome count: the terminal `logging` phase races
/// with the client seeing its response, so the assertion polls briefly
/// instead of assuming ordering.
async fn await_forward_count(
    metrics: &crate::metrics::Metrics,
    outcome: crate::metrics::ForwardOutcome,
    expected: u64,
) {
    for _ in 0..500 {
        if metrics.forward_count(crate::forward::dialect::Endpoint::ChatCompletions, outcome)
            >= expected
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("forward outcome {outcome:?} never reached {expected}");
}

/// A taken port is the caller's error, not a background panic: the
/// probe bind fails before the thread exists.
#[tokio::test]
async fn spawn_data_plane_refuses_a_taken_port() {
    let db = migrated_test_db().await;
    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = holder.local_addr().unwrap().to_string();
    assert!(spawn_data_plane(taken, test_state(db), String::new()).is_err());
}

/// A malformed listen address is the caller's error too.
#[tokio::test]
async fn spawn_data_plane_refuses_a_bad_listen_addr() {
    let db = migrated_test_db().await;
    let err = spawn_data_plane("not-a-socket".to_string(), test_state(db), String::new());
    assert!(err.is_err());
}

/// POST a generation request the way a client does.
async fn post_generation(
    base: &str,
    path: &str,
    bearer: &str,
    profile: Option<&str>,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> reqwest::Response {
    let client = reqwest::Client::new();
    let mut builder = client
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"));
    if let Some(profile) = profile {
        builder = builder.header(crate::forward::PROFILE_HEADER, profile);
    }
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(body.to_string())
        .send()
        .await
        .expect("infallible send")
}

async fn get_data_plane(base: &str, path: &str, bearer: Option<&str>) -> reqwest::Response {
    let client = reqwest::Client::new();
    let mut builder = client.get(format!("{base}{path}"));
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    builder.send().await.expect("infallible send")
}

/// An in-process mock upstream capturing the forwarded headers and
/// answering a canned openai-wire completion. Returns its base URL.
async fn spawn_mock_upstream() -> (
    String,
    std::sync::Arc<tokio::sync::Mutex<Option<axum::http::HeaderMap>>>,
) {
    use axum::routing::post;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("local addr");
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let seen_for_handler = seen.clone();
    let canned_responses = serde_json::json!({
        "id": "resp-test-1",
        "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
    });
    let app = axum::Router::new()
        .route(
            "/chat/completions",
            post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
                    *seen_for_handler.lock().await = Some(headers);
                    assert!(!body.is_empty(), "forwarded body must not be empty");
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "id": "cmpl-test-1",
                            "object": "chat.completion",
                            "choices": [{"message": {"role": "assistant", "content": "hi"}}],
                            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
                        })),
                    )
                },
            ),
        )
        .route(
            "/responses",
            post(move |body: axum::body::Bytes| async move {
                assert!(!body.is_empty(), "forwarded body must not be empty");
                (axum::http::StatusCode::OK, axum::Json(canned_responses))
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock upstream serve");
    });
    (format!("http://{addr}"), seen)
}

/// A mock upstream answering one wire path with one canned JSON body
/// (the mock only has to be predictable).
async fn spawn_mock_upstream_answering(path: &'static str, canned: serde_json::Value) -> String {
    use axum::routing::post;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("local addr");
    let app = axum::Router::new().route(
        path,
        post(move |body: axum::body::Bytes| async move {
            assert!(!body.is_empty(), "forwarded body must not be empty");
            (axum::http::StatusCode::OK, axum::Json(canned))
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock upstream serve");
    });
    format!("http://{addr}")
}

/// The forwarding path end to end against the mock upstream: override
/// selection, Authorization injection (the caller's bearer must NOT
/// travel), and response passthrough.
#[tokio::test]
async fn forward_injects_upstream_authorization_and_passes_through() {
    let db = migrated_test_db().await;
    let (upstream_base, seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    let (base, metrics) = spawn_plane_with_metrics(db).await;

    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p1"),
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["id"], "cmpl-test-1");
    let seen_headers = seen.lock().await;
    let auth = seen_headers
        .as_ref()
        .and_then(|h| h.get("authorization"))
        .and_then(|v| v.to_str().ok());
    assert_eq!(auth, Some(format!("Bearer {UPSTREAM_KEY}").as_str()));
    // The forward landed its terminal class and the in-flight gauge
    // paired its decrement with the selection-time increment.
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::Upstream2xx, 1).await;
    assert_eq!(metrics.in_flight_value(), 0);
}

/// The upstream label on the upstream families is the credential's URL
/// authority (host and port), never a directory identity: this
/// fixture's profile id and provider id disagree on purpose, so a
/// directory-keyed label would surface as the wrong series (or
/// none) carrying the observation.
#[tokio::test]
async fn upstream_histograms_label_the_url_authority() {
    use crate::registry::{profile, provider, set_member};
    use sea_orm::ActiveModelTrait;
    use sea_orm::Set;

    let db = migrated_test_db().await;
    let (upstream_base, _seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;

    provider::ActiveModel {
        owner: Set(crate::registry::CATALOG_OWNER.to_string()),
        provider_id: Set("pv-9".to_string()),
        family: Set("deepseek".to_string()),
        base_url: Set(Some(upstream_base.clone())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("insert the distinct provider");
    crate::secret::provider_credential::ActiveModel {
        owner: Set(crate::registry::CATALOG_OWNER.to_string()),
        provider_id: Set("pv-9".to_string()),
        api_key: Set(crate::test_support::UPSTREAM_KEY.to_string()),
    }
    .insert(&db)
    .await
    .expect("insert the distinct credential");
    profile::ActiveModel {
        profile_id: Set("pf-9".to_string()),
        provider_id: Set("pv-9".to_string()),
        model: Set("deepseek-chat".to_string()),
        max_context_window: Set(Some(128_000)),
        effort: Set(Some("high".to_string())),
        modalities: Set(Some("[\"text\"]".to_string())),
        parked: Set(false),
        store: Set(None),
        owner: Set(crate::registry::CATALOG_OWNER.to_string()),
    }
    .insert(&db)
    .await
    .expect("insert the distinct profile");
    set_member::ActiveModel {
        set_name: Set("alpha".to_string()),
        profile_id: Set("pf-9".to_string()),
        position: Set(9),
        owner: Set(crate::registry::CATALOG_OWNER.to_string()),
    }
    .insert(&db)
    .await
    .expect("insert the distinct member");

    let (base, metrics) = spawn_plane_with_metrics(db).await;
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("pf-9"),
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::Upstream2xx, 1).await;
    let text = metrics.render();
    let authority = upstream_base
        .strip_prefix("http://")
        .unwrap_or(&upstream_base);
    assert!(
        text.contains(&format!(
            "kallipai_model_gateway_upstream_time_to_first_header_seconds_count{{upstream=\"{authority}\"}}"
        )),
        "the label must be the upstream authority:\n{text}"
    );
    assert!(
        text.contains(&format!(
            "kallipai_model_gateway_upstream_responses_total{{api_family=\"deepseek\",status_class=\"2xx\",upstream=\"{authority}\"}} 1"
        )),
        "the attribution table must carry the response class:\n{text}"
    );
    assert!(
        !text.contains("system/pf-9") && !text.contains("system/pv-9"),
        "no directory identity may leak into an upstream label:\n{text}"
    );
}

/// Authorization scoping on the forward path: unknown bearer 401, a
/// known identity asking for an out-of-domain profile 403 (the pro
/// catalog sits behind the platform group), unknown profile 404.
#[tokio::test]
async fn forward_rejects_unknown_bearers_and_out_of_domain_profiles() {
    let db = migrated_test_db().await;
    let (upstream_base, _seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    let (base, metrics) = spawn_plane_with_metrics(db).await;

    let response =
        post_generation(&base, "/chat/completions", "not-a-bearer", None, "{}", &[]).await;
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);

    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p7"),
        "{}",
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);

    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("ghost"),
        "{}",
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);

    // The observability face: one identity resolution per attempt, one
    // forward outcome per request, each under its own class.
    assert_eq!(
        metrics.identity_count(crate::metrics::IdentityOutcome::Unauthorized),
        1
    );
    assert_eq!(
        metrics.identity_count(crate::metrics::IdentityOutcome::Ok),
        2
    );
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::GatedAuth, 1).await;
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::GatedVisibility, 1).await;
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::GatedRoute, 1).await;
}

/// The un-overridden face: the selected collection's default set
/// serves (the single store the pull face shares), with the
/// no-selection and no-default states naming themselves. The
/// explicit override still serves the responses wire through the
/// same seed.
#[tokio::test]
async fn unoverridden_requests_serve_the_selected_default_set() {
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
    let db = migrated_test_db().await;
    let (upstream_base, _seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;

    // No selection: the 404 names the empty state.
    let base = spawn_plane(db.clone()).await;
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        None,
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "no collection selected");
    drop(base);

    // A selection whose collection has no default anchor: the 404
    // names that state instead.
    crate::registry::gateway_selection::ActiveModel {
        tagma_id: Set("tagma-under-test".to_string()),
        owner: Set("system".to_string()),
        collection_name: Set("catalog-pro".to_string()),
        updated_at: Set(time::OffsetDateTime::now_utc()),
    }
    .insert(&db)
    .await
    .expect("insert selection");
    let base = spawn_plane(db.clone()).await;
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        None,
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "the selected collection has no default set"
    );
    drop(base);

    // The selected collection's default set serves the
    // un-overridden chat wire through its head member.
    crate::registry::gateway_selection::Entity::update_many()
        .col_expr(
            crate::registry::gateway_selection::Column::CollectionName,
            sea_orm::sea_query::Expr::value("baseline"),
        )
        .filter(crate::registry::gateway_selection::Column::TagmaId.eq("tagma-under-test"))
        .exec(&db)
        .await
        .expect("repoint selection");
    crate::registry::collection::Entity::update_many()
        .col_expr(
            crate::registry::collection::Column::DefaultSetName,
            sea_orm::sea_query::Expr::value("alpha"),
        )
        .filter(crate::registry::collection::Column::Owner.eq("system"))
        .filter(crate::registry::collection::Column::Name.eq("baseline"))
        .exec(&db)
        .await
        .expect("anchor the default set");
    let base = spawn_plane(db).await;
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        None,
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["id"], "cmpl-test-1");

    let response = post_generation(
        &base,
        "/responses",
        TEST_TAGMA_BEARER,
        Some("p6"),
        r#"{"model":"gpt-x-responses","input":"hi"}"#,
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["id"], "resp-test-1");
}

/// The no-foreign-key promise end to end: deleting the selected
/// collection leaves the pointer row behind, and both readers --
/// the pull and the forward -- answer the same honest 404 instead
/// of a cascade or a silent serve.
#[tokio::test]
async fn a_deleted_selected_collection_answers_404_on_both_faces() {
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};
    use tower::ServiceExt;

    let db = migrated_test_db().await;
    crate::test_support::seed_registry(&db, "https://api.upstream.test").await;

    crate::registry::gateway_selection::ActiveModel {
        tagma_id: Set("tagma-under-test".to_string()),
        owner: Set("acc-bound".to_string()),
        collection_name: Set("user-col".to_string()),
        updated_at: Set(time::OffsetDateTime::now_utc()),
    }
    .insert(&db)
    .await
    .expect("insert selection");

    let deleted = crate::registry::collection::Entity::delete_by_id((
        "acc-bound".to_string(),
        "user-col".to_string(),
    ))
    .exec(&db)
    .await
    .expect("delete the selected collection");
    assert_eq!(
        deleted.rows_affected, 1,
        "the seed really carried the selected collection"
    );

    // The pointer survives the delete: nothing cascades it away, so
    // the readers must say the target is gone themselves.
    let pointer =
        crate::registry::gateway_selection::Entity::find_by_id("tagma-under-test".to_string())
            .one(&db)
            .await
            .expect("read the selection row");
    assert!(pointer.is_some(), "the selection row survives the delete");

    // The pull face: the management router names the dangling state.
    let app = crate::routes::management_plane_router(test_state(db.clone()), "");
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/selected-collection")
                .header("authorization", format!("Bearer {TEST_TAGMA_BEARER}"))
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["error"]["message"],
        "the selected collection no longer exists"
    );

    // The forward face: the data plane answers the same 404.
    let base = spawn_plane(db).await;
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        None,
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "the selected collection no longer exists"
    );
}

/// The set-level domain gate: the selected collection can be inside
/// the viewer's domain while its anchored default set sits outside
/// it. The forward denies with a 403 -- the default deployment must
/// not become a fail-open side door around the visibility domain.
#[tokio::test]
async fn an_out_of_domain_default_set_denies_the_forward() {
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

    let db = migrated_test_db().await;
    crate::test_support::seed_registry(&db, "https://api.upstream.test").await;

    // `user-col` is published to everyone (the collection is inside
    // the domain); its anchored default `grp-set` is not, because
    // `grp-col`'s publication names a group acct-test does not sit in.
    crate::registry::gateway_selection::ActiveModel {
        tagma_id: Set("tagma-under-test".to_string()),
        owner: Set("acc-bound".to_string()),
        collection_name: Set("user-col".to_string()),
        updated_at: Set(time::OffsetDateTime::now_utc()),
    }
    .insert(&db)
    .await
    .expect("insert selection");
    crate::registry::collection::Entity::update_many()
        .col_expr(
            crate::registry::collection::Column::DefaultSetName,
            sea_orm::sea_query::Expr::value("grp-set"),
        )
        .filter(crate::registry::collection::Column::Owner.eq("acc-bound"))
        .filter(crate::registry::collection::Column::Name.eq("user-col"))
        .exec(&db)
        .await
        .expect("anchor the out-of-domain default set");

    let base = spawn_plane(db).await;
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        None,
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "default profile is outside the visibility domain"
    );
}

/// accept-encoding never reaches the upstream: forwarding the header
/// would buy a compressed body whose content-encoding the proxy drops on
/// the way back (unlabeled corruption).
#[tokio::test]
async fn forward_strips_accept_encoding() {
    let db = migrated_test_db().await;
    let (upstream_base, seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    let base = spawn_plane(db).await;

    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p1"),
        "{}",
        &[("accept-encoding", "gzip")],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(
        seen.lock()
            .await
            .as_ref()
            .and_then(|h| h.get("accept-encoding"))
            .is_none()
    );
}

/// Client-supplied credential headers never travel: x-api-key and
/// anthropic-version are the upstream's vocabulary, and a client
/// presenting either must not have it forwarded on its behalf.
#[tokio::test]
async fn forward_strips_client_credential_headers() {
    let db = migrated_test_db().await;
    let (upstream_base, seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    let base = spawn_plane(db).await;

    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p1"),
        "{}",
        &[
            ("x-api-key", "client-key-must-not-travel"),
            ("anthropic-version", "2023-06-01"),
        ],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let seen_headers = seen.lock().await;
    assert!(
        seen_headers
            .as_ref()
            .and_then(|h| h.get("x-api-key"))
            .is_none()
    );
    assert!(
        seen_headers
            .as_ref()
            .and_then(|h| h.get("anthropic-version"))
            .is_none()
    );
}

/// The parking gate: a parked profile inside the
/// domain passes authorization but fails availability -- 403 on the
/// override path.
#[tokio::test]
async fn forward_rejects_parked_profiles_inside_the_domain() {
    let db = migrated_test_db().await;
    let (upstream_base, _seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    // Park p5 into the baseline set `alpha` (position 2; the failover
    // slots 0/1 are taken).
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Set, Statement};
    // The provider row lands first: the profile's FK needs it.
    crate::registry::provider::ActiveModel {
        owner: Set("system".to_string()),
        provider_id: Set("p5".to_string()),
        family: Set("deepseek".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("insert parked provider");
    crate::registry::profile::ActiveModel {
        profile_id: Set("p5".to_string()),
        provider_id: Set("p5".to_string()),
        model: Set("deepseek-parked".to_string()),
        max_context_window: Set(None),
        effort: Set(None),
        modalities: Set(None),
        parked: Set(true),
        store: Set(None),
        owner: Set("system".to_string()),
    }
    .insert(&db)
    .await
    .expect("insert parked profile");
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO set_members (set_name, profile_id, position) VALUES ('alpha', 'p5', 3)",
    ))
    .await
    .expect("park the parked profile into the allowed set");
    let (base, metrics) = spawn_plane_with_metrics(db).await;

    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p5"),
        "{}",
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    // Parking is its own observable class: a platform state, not an
    // error.
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::GatedParking, 1).await;
}

/// The family gate: an endpoint refuses profiles whose family it
/// cannot serve (no upstream call).
#[tokio::test]
async fn family_gate_rejects_cross_family_endpoints() {
    let db = migrated_test_db().await;
    let upstream_base = spawn_mock_upstream_answering("/responses", serde_json::json!({})).await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    let base = spawn_plane(db.clone()).await;

    // A responses-family profile cannot serve the chat endpoint.
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p6"),
        "{}",
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);

    // A chat-family profile cannot serve the responses endpoint.
    let response = post_generation(
        &base,
        "/responses",
        TEST_TAGMA_BEARER,
        Some("p1"),
        "{}",
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}

/// A mock anthropic upstream capturing the forwarded headers and
/// answering a canned messages completion.
async fn spawn_mock_anthropic() -> (
    String,
    std::sync::Arc<tokio::sync::Mutex<Option<axum::http::HeaderMap>>>,
) {
    use axum::routing::post;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock anthropic upstream");
    let addr = listener.local_addr().expect("local addr");
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let seen_for_handler = seen.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
                *seen_for_handler.lock().await = Some(headers);
                assert!(!body.is_empty(), "forwarded body must not be empty");
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "id": "msg-test-1",
                        "type": "message",
                        "role": "assistant",
                        "usage": {
                            "input_tokens": 12,
                            "output_tokens": 5,
                            "cache_read_input_tokens": 3,
                            "cache_creation_input_tokens": 4
                        }
                    })),
                )
            },
        ),
    );
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock anthropic serve");
    });
    (format!("http://{addr}"), seen)
}

/// A streaming anthropic upstream: the canned SSE stream states an
/// explicitly all-zero usage accounting (start says 0/0, the delta
/// confirms 0).
async fn spawn_mock_anthropic_stream() -> String {
    use axum::routing::post;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock anthropic stream upstream");
    let addr = listener.local_addr().expect("local addr");
    let body = "event: message_start\n\
                data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\
                \n\
                event: message_delta\n\
                data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":0}}\n\n";
    let app = axum::Router::new().route(
        "/v1/messages",
        post(move || async {
            axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                .body(axum::body::Body::from(body.to_owned()))
                .expect("static response")
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock anthropic stream serve");
    });
    format!("http://{addr}")
}

/// An anthropic-family profile with a credential against the mock,
/// seated in the fixture's allowed set (position 9, off to the side
/// of the pinned 0..2 order). Direct SQL-layer seeding: the shared
/// fixture's member order is pinned by other tests.
async fn seed_anthropic_profile(db: &crate::db::Db, upstream_base: &str) {
    use crate::registry::{profile, provider, set_member};
    use crate::secret::provider_credential;
    use sea_orm::{ActiveModelTrait, Set};

    provider::ActiveModel {
        owner: Set("system".to_string()),
        provider_id: Set("pa".to_string()),
        family: Set(crate::forward::dialect::ANTHROPIC.to_string()),
        base_url: Set(Some(upstream_base.to_string())),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert anthropic provider");
    profile::ActiveModel {
        profile_id: Set("pa".to_string()),
        provider_id: Set("pa".to_string()),
        model: Set("claude-test".to_string()),
        max_context_window: Set(Some(128_000)),
        effort: Set(None),
        modalities: Set(Some("[\"text\"]".to_string())),
        parked: Set(false),
        store: Set(None),
        owner: Set("system".to_string()),
    }
    .insert(db)
    .await
    .expect("insert anthropic profile");
    provider_credential::ActiveModel {
        owner: Set("system".to_string()),
        provider_id: Set("pa".to_string()),
        api_key: Set(UPSTREAM_KEY.to_string()),
    }
    .insert(db)
    .await
    .expect("insert anthropic credential");
    set_member::ActiveModel {
        set_name: Set("alpha".to_string()),
        profile_id: Set("pa".to_string()),
        position: Set(9),
        owner: Set("system".to_string()),
    }
    .insert(db)
    .await
    .expect("insert anthropic member");
}

/// The anthropic endpoint drives the credential's x-api-key shape:
/// the upstream sees the gateway's own stamp (x-api-key plus the
/// pinned anthropic-version) and never the client's copies -- both
/// are stripped, so a client presenting them is overridden, not
/// forwarded.
#[tokio::test]
async fn anthropic_endpoint_swaps_in_the_gateway_credential() {
    let db = migrated_test_db().await;
    let (upstream_base, seen) = spawn_mock_anthropic().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    seed_anthropic_profile(&db, &upstream_base).await;
    let base = spawn_plane(db).await;

    let response = post_generation(
        &base,
        "/messages",
        TEST_TAGMA_BEARER,
        Some("pa"),
        r#"{"model":"claude-test","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
        &[
            ("x-api-key", "client-key-must-not-travel"),
            ("anthropic-version", "1999-01-01"),
        ],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let seen_headers = seen.lock().await;
    let seen = seen_headers.as_ref().expect("upstream saw headers");
    assert_eq!(
        seen.get("x-api-key").and_then(|v| v.to_str().ok()),
        Some(UPSTREAM_KEY),
        "the gateway's stamp, not the client's copy"
    );
    assert_eq!(
        seen.get("anthropic-version").and_then(|v| v.to_str().ok()),
        Some(crate::secret::ANTHROPIC_API_VERSION),
        "the pinned wire version, not the client's"
    );
    assert!(seen.get("authorization").is_none());
}

/// A streamed SSE response passes through verbatim: the content-type
/// marker and the event bytes reach the client untouched.
#[tokio::test]
async fn anthropic_stream_passes_through() {
    let db = migrated_test_db().await;
    let upstream_base = spawn_mock_anthropic_stream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    seed_anthropic_profile(&db, &upstream_base).await;
    let base = spawn_plane(db).await;

    let response =
        post_generation(&base, "/messages", TEST_TAGMA_BEARER, Some("pa"), "{}", &[]).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let body = response.bytes().await.unwrap();
    assert!(
        body.windows(13).any(|w| w == b"message_start"),
        "the SSE events arrive: {body:?}"
    );
}

/// The scratch faces: health answers unauthenticated (compose
/// healthcheck has no credentials), the whole /admin family is a 404 on
/// the public port (bare prefix included), a known path with the wrong
/// method is a 405, and an unknown path is a plain 404.
#[tokio::test]
async fn health_admin_and_error_faces_answer_without_an_upstream() {
    let db = migrated_test_db().await;
    let base = spawn_plane(db).await;

    let health = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(health.status(), axum::http::StatusCode::OK);
    assert_eq!(health.text().await.unwrap(), "ok");

    let admin = get_data_plane(&base, "/admin/keys", Some(TEST_TAGMA_BEARER)).await;
    assert_eq!(admin.status(), axum::http::StatusCode::NOT_FOUND);
    let bare_admin = get_data_plane(&base, "/admin", Some(TEST_TAGMA_BEARER)).await;
    assert_eq!(bare_admin.status(), axum::http::StatusCode::NOT_FOUND);

    let client = reqwest::Client::new();
    let method_mismatch = client
        .get(format!("{base}/chat/completions"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        method_mismatch.status(),
        axum::http::StatusCode::METHOD_NOT_ALLOWED
    );

    let unknown = get_data_plane(&base, "/no/such/path", None).await;
    assert_eq!(unknown.status(), axum::http::StatusCode::NOT_FOUND);
}

/// The CORS preflight: the browser's probe (OPTIONS + Origin + the
/// request-method marker) is answered by the plane itself before any
/// routing. A listed origin echoes back with the advertised method and
/// header lists plus credentials; an unlisted origin gets no CORS
/// headers at all.
#[tokio::test]
async fn cors_preflight_is_answered_with_the_cors_headers() {
    let db = migrated_test_db().await;
    let addr = next_addr();
    let origin = "https://app.example";
    spawn_data_plane(addr.clone(), test_state(db), origin.to_string()).expect("spawn data plane");
    wait_for_listener(&addr);
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();
    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{base}/chat/completions"))
        .header("origin", origin)
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), axum::http::StatusCode::OK);
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(origin)
    );
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok()),
        Some("GET, POST, PUT, DELETE")
    );
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-headers")
            .and_then(|v| v.to_str().ok()),
        Some("authorization, content-type")
    );
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-credentials")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );

    let stranger = client
        .request(reqwest::Method::OPTIONS, format!("{base}/chat/completions"))
        .header("origin", "https://elsewhere.example")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(stranger.status(), axum::http::StatusCode::OK);
    assert!(
        stranger
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
}

/// The declared-body bound answers a 413 through the bridge, so the
/// CORS headers ride along like every other short-circuit answer. The
/// socket stays raw on purpose: the declared content-length is larger
/// than the bound but the body is never streamed, which is exactly the
/// shape the bound judges -- an HTTP client would race the server's
/// early answer against its own oversized upload.
#[tokio::test]
async fn oversize_body_is_a_413_with_cors_headers() {
    let db = migrated_test_db().await;
    let addr = next_addr();
    let origin = "https://app.example";
    spawn_data_plane(addr.clone(), test_state(db), origin.to_string()).expect("spawn data plane");
    wait_for_listener(&addr);

    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let request = format!(
        "POST /chat/completions HTTP/1.1\r\nhost: {addr}\r\norigin: {origin}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        8 * 1024 * 1024 + 1
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(&[b'x'; 32]).await.unwrap();

    let mut head = String::new();
    let mut buf = [0u8; 2048];
    while !head.contains("\r\n\r\n") {
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "connection closed before a response head");
        head.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(head.starts_with("HTTP/1.1 413"));
    assert!(
        head.to_ascii_lowercase()
            .contains("access-control-allow-origin: https://app.example")
    );
}
