//! The data-plane end-to-end tests: every test drives a real pingora
//! listener (a scratch port per test) against an in-process mock
//! upstream, the way a client would -- no oneshot shortcuts, so the
//! phase wiring itself is under test. The assertion set is the
//! forwarding suite's.

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::resolver::{Lookup, Resolver};
use super::spawn_data_plane;
use super::spawn_data_plane_with_resolver;
use crate::state::AppState;
use crate::test_support::{ObservedSpan, TEST_TAGMA_BEARER, UPSTREAM_KEY, migrated_test_db};

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

/// The process-global capture the line assertions read: installed once for
/// the whole binary by the shared helper, so every end-to-end test reads
/// the same buffer and selects its own line with a needle it alone
/// produces.
fn global_lines() -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    crate::test_support::global_capture()
}

/// Await the captured line carrying `needle`, read back the way the unit
/// tests read their own capture: quotes stripped by the fmt layer.
async fn await_line(lines: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>, needle: &str) -> String {
    for _ in 0..500 {
        let found = {
            let text = String::from_utf8(lines.lock().unwrap().clone())
                .expect("the fmt layer writes utf8")
                .replace('"', "");
            text.lines()
                .find(|line| line.contains(needle))
                .map(str::to_string)
        };
        if let Some(line) = found {
            return line;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no captured line carried {needle}");
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

/// A raw mock upstream answering one chat-completions request with a chunked
/// stream of `frames` SSE frames paced by `interval`, then abandoning the
/// body. A caller that reads the first frame and walks away, and one that
/// reads through the upstream's own break, both leave the gateway mid-relay;
/// the two differ only in who ended the exchange.
///
/// The socket is raw rather than an axum route because the framing is cut by
/// hand here: the upstream-side case abandons the body without the
/// terminating chunk, which a well-formed axum body cannot express, and the
/// caller-side case needs the frames paced out slowly enough to walk away
/// from the first one.
async fn spawn_mock_streaming_upstream(frames: u32, interval: Duration) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind streaming upstream");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 1024];
                let head_end = loop {
                    let n = match socket.read(&mut chunk).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let declared: usize = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + declared {
                    let n = match socket.read(&mut chunk).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                if socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
                    )
                    .await
                    .is_err()
                {
                    return;
                }
                for i in 0..frames {
                    let data = format!("data: {{\"i\":{i}}}\n\n");
                    let frame = format!("{:x}\r\n{data}\r\n", data.len());
                    if socket.write_all(frame.as_bytes()).await.is_err() {
                        return;
                    }
                    if socket.flush().await.is_err() {
                        return;
                    }
                    tokio::time::sleep(interval).await;
                }
            });
        }
    });
    format!("http://{addr}")
}

/// The caller's own break: the client walks away mid-stream, so the
/// forward ends on the downstream side, not as an upstream failure.
#[tokio::test]
async fn a_client_that_walks_away_mid_stream_is_not_an_upstream_error() {
    let db = migrated_test_db().await;
    let upstream_base = spawn_mock_streaming_upstream(40, Duration::from_millis(100)).await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    let (base, metrics) = spawn_plane_with_metrics(db).await;
    let addr = base.strip_prefix("http://").expect("base carries a host");

    let mut client = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect the gateway");
    let body = "{}";
    let request = format!(
        "POST /chat/completions HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {TEST_TAGMA_BEARER}\r\n{}: p1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        crate::forward::PROFILE_HEADER,
        body.len()
    );
    client
        .write_all(request.as_bytes())
        .await
        .expect("write the request");

    // Read until the first chunked frame of the streamed body is in, then drop
    // the socket: the gateway's next write to this client is the one under test.
    let mut seen: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    while !seen.windows(7).any(|w| w == b"data: {") {
        let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut chunk))
            .await
            .expect("the upstream stream reaches the client")
            .expect("read the response");
        assert!(
            n > 0,
            "the stream must stay open; saw: {}",
            String::from_utf8_lossy(&seen)
        );
        seen.extend_from_slice(&chunk[..n]);
    }
    drop(client);

    await_forward_count(&metrics, crate::metrics::ForwardOutcome::DownstreamError, 1).await;
    let rendered = metrics.render();
    assert!(
        !rendered.contains("outcome=\"upstream_error\""),
        "no upstream failure happened: {rendered}"
    );
}

/// The other half of the split: a break the upstream caused stays an upstream
/// error, so the split cannot swallow real upstream trouble.
#[tokio::test]
async fn an_upstream_that_breaks_mid_stream_stays_an_upstream_error() {
    let db = migrated_test_db().await;
    let upstream_base = spawn_mock_streaming_upstream(1, Duration::ZERO).await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    let (base, metrics) = spawn_plane_with_metrics(db).await;

    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p1"),
        "{}",
        &[],
    )
    .await;
    // Read to the break: the client stays until the gateway ends the exchange.
    let _ = response.bytes().await;

    await_forward_count(&metrics, crate::metrics::ForwardOutcome::UpstreamError, 1).await;
    let rendered = metrics.render();
    assert!(
        !rendered.contains("outcome=\"downstream_error\""),
        "the break was the upstream's: {rendered}"
    );
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
/// method is a 405, and an unknown path is a plain 404. None of the five is
/// routed to a generation endpoint, so none carries a gateway outcome or an
/// upstream attribute.
#[tokio::test]
async fn health_admin_and_error_faces_answer_without_an_upstream() {
    crate::test_support::global_capture();
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

    let paths = [
        "/health",
        "/admin/keys",
        "/admin",
        "/chat/completions",
        "/no/such/path",
    ];
    let spans = await_spans(|spans| {
        paths.iter().all(|path| {
            spans.iter().any(|span| {
                span.kind == "server"
                    && span.attribute("http.request.method") == Some("GET")
                    && span.attribute("url.path") == Some(*path)
            })
        })
    })
    .await;
    for path in paths {
        let span = spans
            .iter()
            .find(|span| {
                span.kind == "server"
                    && span.attribute("http.request.method") == Some("GET")
                    && span.attribute("url.path") == Some(path)
            })
            .unwrap_or_else(|| panic!("no GET server span for {path}"));
        assert_eq!(
            span.attribute("http.route"),
            Some(super::routes::route_of(&axum::http::Method::GET, path).label()),
            "the route label of GET {path} comes from Route::label"
        );
        assert_eq!(
            span.attribute("gateway.outcome"),
            None,
            "no outcome on GET {path}"
        );
        assert_eq!(
            span.attribute("upstream.authority"),
            None,
            "no authority on GET {path}"
        );
        assert_eq!(
            span.attribute("upstream.api_family"),
            None,
            "no family on GET {path}"
        );
    }
}

/// The CORS preflight: the browser's probe (OPTIONS + Origin + the
/// request-method marker) is answered by the plane itself before any
/// routing. A listed origin echoes back with the advertised method and
/// header lists plus credentials; an unlisted origin gets no CORS
/// headers at all.
#[tokio::test]
async fn cors_preflight_is_answered_with_the_cors_headers() {
    crate::test_support::global_capture();
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

    // The probe is a request like any other: it leaves one server span, and
    // the label is the one thing that says what it was.
    let spans = await_spans(|spans| {
        spans
            .iter()
            .any(|span| span.attribute("http.route") == Some("preflight"))
    })
    .await;
    let probe = spans
        .iter()
        .find(|span| span.attribute("http.route") == Some("preflight"))
        .unwrap_or_else(|| panic!("the probe left no span labelled preflight: {spans:#?}"));
    // Answered before the routing decision is used and before a credential is
    // looked up, so the outcome and the upstream fields are absent, as the
    // docs say.
    assert_eq!(probe.kind, "server");
    assert_eq!(probe.attribute("http.request.method"), Some("OPTIONS"));
    assert_eq!(probe.attribute("http.response.status_code"), Some("200"));
    assert_eq!(probe.attribute("gateway.outcome"), None);
    assert_eq!(probe.attribute("upstream.authority"), None);
    assert_eq!(probe.attribute("upstream.api_family"), None);
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

/// The internal-failure face end to end: the selected default set serves
/// through a profile whose credential row is gone, so the failure happens
/// on the gateway side, before the upstream -- and the observability face
/// has to say so.
#[tokio::test]
async fn an_internal_failure_names_its_reason_and_profile() {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let db = migrated_test_db().await;
    let (upstream_base, _seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    seed_selection(&db).await;

    let removed = crate::secret::provider_credential::Entity::delete_many()
        .filter(crate::secret::provider_credential::Column::ProviderId.eq("p1"))
        .exec(&db)
        .await
        .expect("delete the head member's credential");
    assert_eq!(removed.rows_affected, 1);
    // The capture installs before the plane spawns: the terminal phase
    // writes its line on pingora's own thread.
    let lines = global_lines();

    let (base, metrics) = spawn_plane_with_metrics(db).await;
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        None,
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "internal error");

    await_forward_count(&metrics, crate::metrics::ForwardOutcome::InternalError, 1).await;
    let line = await_line(&lines, "reason=missing_credential").await;
    for field in [
        "endpoint=chat",
        "outcome=internal_error",
        "tagma_id=tagma-under-test",
        "reason=missing_credential",
        "error=no provider credential for profile p1",
        "connected=false",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
}

/// A stored endpoint prefix that is not an http(s) endpoint: the forward
/// path refuses to guess a dial target, the client gets the redacted 5xx,
/// and the line names the profile and the reason beside the request.
#[tokio::test]
async fn an_unparseable_endpoint_prefix_is_named_on_the_line() {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let db = migrated_test_db().await;
    let (upstream_base, _seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    seed_selection(&db).await;
    crate::registry::provider::Entity::update_many()
        .col_expr(
            crate::registry::provider::Column::BaseUrl,
            sea_orm::sea_query::Expr::value("ftp://upstream.test/v1"),
        )
        .filter(crate::registry::provider::Column::Owner.eq("system"))
        .filter(crate::registry::provider::Column::ProviderId.eq("p1"))
        .exec(&db)
        .await
        .expect("point the served provider at an unparseable prefix");

    let lines = global_lines();
    let (base, metrics) = spawn_plane_with_metrics(db).await;
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        None,
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "internal error");

    await_forward_count(&metrics, crate::metrics::ForwardOutcome::InternalError, 1).await;
    let line = await_line(&lines, "reason=unparseable_endpoint_prefix").await;
    for field in [
        "endpoint=chat",
        "outcome=internal_error",
        "tagma_id=tagma-under-test",
        "reason=unparseable_endpoint_prefix",
        "error=the upstream endpoint prefix for profile p1 does not parse",
        "connected=false",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
}

/// A store failure behind the selection: the credential read fails, the
/// client gets the mapping's 5xx with the redacted text, and the line
/// names the request, the `store_error` reason and the sentence the
/// mapping logged -- the whole chain from the failing read to the record.
#[tokio::test]
async fn a_store_failure_behind_the_selection_is_named_on_the_line() {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    let db = migrated_test_db().await;
    let (upstream_base, _seen) = spawn_mock_upstream().await;
    crate::test_support::seed_registry(&db, &upstream_base).await;
    seed_selection(&db).await;
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        "DROP TABLE provider_credentials CASCADE".to_string(),
    ))
    .await
    .expect("take the credential table away");

    let lines = global_lines();
    let (base, metrics) = spawn_plane_with_metrics(db).await;
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        None,
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "internal error");

    await_forward_count(&metrics, crate::metrics::ForwardOutcome::InternalError, 1).await;
    let line = await_line(&lines, "reason=store_error").await;
    for field in [
        "endpoint=chat",
        "outcome=internal_error",
        "tagma_id=tagma-under-test",
        "reason=store_error",
        "error=database error:",
        "connected=false",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
}

/// The selection the failure-line tests ride: the tagma pinned to the
/// baseline collection, whose default set anchors on `alpha` (the fixture
/// set whose head member is profile p1).
async fn seed_selection(db: &crate::db::Db) {
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

    crate::registry::gateway_selection::ActiveModel {
        tagma_id: Set("tagma-under-test".to_string()),
        owner: Set("system".to_string()),
        collection_name: Set("baseline".to_string()),
        updated_at: Set(time::OffsetDateTime::now_utc()),
    }
    .insert(db)
    .await
    .expect("insert selection");
    crate::registry::collection::Entity::update_many()
        .col_expr(
            crate::registry::collection::Column::DefaultSetName,
            sea_orm::sea_query::Expr::value("alpha"),
        )
        .filter(crate::registry::collection::Column::Owner.eq("system"))
        .filter(crate::registry::collection::Column::Name.eq("baseline"))
        .exec(db)
        .await
        .expect("anchor the default set");
}

/// A fake archeion on loopback: `verify-bearer` names a tagma this fixture
/// never enrolls, and `enrollment-lookup` gives the collapsed 404, so a
/// request through the real verifier ends at the bare 401 the data plane
/// answers an identity it cannot resolve. Both calls leave through
/// `telemetry::http_client`, which is where the client spans come from.
async fn spawn_fake_archeion() -> String {
    use axum::Json;
    use axum::routing::post;

    let app = axum::Router::new()
        .route(
            "/internal/verify-bearer",
            post(|Json(_body): Json<serde_json::Value>| async {
                Json(serde_json::json!({
                    "principal": {"kind": "tagma", "tagma_id": "tagma-unenrolled"}
                }))
            }),
        )
        .route(
            "/internal/enrollment-lookup",
            post(|| async { axum::http::StatusCode::NOT_FOUND }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("the fake archeion serves");
    });
    format!("http://{addr}")
}

/// The spans the process exported, waited for: the terminal phase and the
/// exporter race with the client seeing its answer.
async fn await_spans<F>(ready: F) -> Vec<ObservedSpan>
where
    F: Fn(&[ObservedSpan]) -> bool,
{
    for _ in 0..200 {
        let spans = crate::test_support::global_spans();
        if ready(&spans) {
            return spans;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    crate::test_support::global_spans()
}

/// The honest 401 path, traced: the archeion check a rejected request causes
/// hangs under the span of that request. With no data-plane span the check is
/// the root of a trace of its own, and the trace cannot say which request it
/// answered -- the debt this assertion exists to keep paid.
#[tokio::test]
async fn the_archeion_check_of_a_rejected_request_hangs_under_its_request_span() {
    // Installed before the first request: the process gets one subscriber, and
    // a span opened before it is installed is exported nowhere.
    crate::test_support::global_capture();
    let db = migrated_test_db().await;
    let archeion = spawn_fake_archeion().await;
    let addr = next_addr();
    let state = AppState {
        db,
        public_base_url: "http://gw.test:7501".to_string(),
        management: crate::management::AdminAuth::Platform(std::sync::Arc::new(
            crate::management::ArcheionVerifier::new(archeion, "internal-token".to_string()),
        )),
        identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
        metrics: crate::metrics::Metrics::private(),
    };
    spawn_data_plane(addr.clone(), state, String::new()).expect("spawn data plane");
    wait_for_listener(&addr);

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/messages"))
        .bearer_auth("a-bearer-nobody-enrolled")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("the plane answers");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Only this fixture wires the real verifier, so the checks into the fake
    // archeion are the ones this request caused.
    let spans = await_spans(|spans| {
        let checks: Vec<&ObservedSpan> = spans
            .iter()
            .filter(|span| span.name.starts_with("POST /internal/"))
            .collect();
        // The request span ends with the terminal phase, after the checks
        // it caused have ended, so the wait covers both halves of the pair.
        !checks.is_empty()
            && checks.iter().all(|check| {
                spans
                    .iter()
                    .any(|span| Some(span.span_id.as_str()) == check.parent_span_id.as_deref())
            })
    })
    .await;
    let checks: Vec<&ObservedSpan> = spans
        .iter()
        .filter(|span| span.name.starts_with("POST /internal/"))
        .collect();
    assert!(
        !checks.is_empty(),
        "the verifier's calls left client spans: {spans:#?}"
    );
    for check in checks {
        let parent = spans
            .iter()
            .find(|span| Some(span.span_id.as_str()) == check.parent_span_id.as_deref());
        let parent = parent.unwrap_or_else(|| {
            panic!("the check {check:#?} is a root: no span carries its parent id, in {spans:#?}")
        });
        assert_eq!(
            parent.kind, "server",
            "the check hangs under the request span: {check:#?} under {parent:#?}"
        );
        assert_eq!(parent.attribute("url.path"), Some("/messages"));
        assert_eq!(check.trace_id, parent.trace_id);
        assert_eq!(parent.attribute("http.route"), Some("/messages"));
        assert_eq!(parent.attribute("http.request.method"), Some("POST"));
        assert_eq!(parent.attribute("http.response.status_code"), Some("401"));
        assert_eq!(parent.attribute("gateway.outcome"), Some("gated_auth"));
        assert!(
            parent.parent_span_id.is_none(),
            "no caller context travels: the request span is a root"
        );
    }
}

/// The one server span of a trace the test minted: the trace id is the
/// selector, so concurrent tests in this binary cannot collide with it.
async fn await_server_span(trace: &str) -> ObservedSpan {
    let spans = await_spans(|spans| {
        spans
            .iter()
            .any(|span| span.trace_id == trace && span.kind == "server")
    })
    .await;
    let found = spans
        .iter()
        .find(|span| span.trace_id == trace && span.kind == "server");
    match found {
        Some(span) => span.clone(),
        None => panic!("no server span in trace {trace}: {spans:#?}"),
    }
}

/// A caller's W3C context is adopted rather than dropped: the request span
/// continues that trace and hangs under the caller's span, so one trace
/// covers the hop into the gateway. The health probe is the cheapest route
/// that answers without an identity or an upstream.
#[tokio::test]
async fn an_incoming_traceparent_becomes_the_request_spans_parent() {
    crate::test_support::global_capture();
    let db = migrated_test_db().await;
    let base = spawn_plane(db).await;
    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let caller_span = "00f067aa0ba902b7";

    let response = reqwest::Client::new()
        .get(format!("{base}/health"))
        .header("traceparent", format!("00-{trace}-{caller_span}-01"))
        .send()
        .await
        .expect("the health probe answers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let span = await_server_span(trace).await;
    assert_eq!(span.parent_span_id.as_deref(), Some(caller_span));
    assert_eq!(span.name, "GET /health");
    assert_eq!(span.attribute("http.route"), Some("/health"));
    assert_eq!(span.attribute("http.request.method"), Some("GET"));
    assert_eq!(span.attribute("url.path"), Some("/health"));
    assert_eq!(span.attribute("http.response.status_code"), Some("200"));
    assert_eq!(span.attribute("error.type"), None, "a 2xx names no error");
    assert_eq!(
        span.attribute("gateway.outcome"),
        None,
        "a health probe takes no credential: no outcome"
    );
    assert_eq!(
        span.attribute("upstream.authority"),
        None,
        "nothing was selected for a probe: no authority"
    );
    assert_eq!(span.status, "unset");
}

/// The unreachable-upstream path, traced: the answer the client sees and the
/// failure the span names are one event. Nothing listens on port 1, so the
/// dial fails and no upstream ever speaks.
#[tokio::test]
async fn a_forward_to_an_unreachable_upstream_is_a_failed_server_span() {
    crate::test_support::global_capture();
    let db = migrated_test_db().await;
    crate::test_support::seed_registry(&db, "http://127.0.0.1:1").await;
    let base = spawn_plane(db).await;
    let trace = "11111111111111111111111111111111";
    let caller_span = "2222222222222222";

    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p1"),
        r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
        &[("traceparent", &format!("00-{trace}-{caller_span}-01"))],
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);

    let span = await_server_span(trace).await;
    assert_eq!(span.name, "POST /chat/completions");
    assert_eq!(span.attribute("http.route"), Some("/chat/completions"));
    assert_eq!(span.attribute("http.response.status_code"), Some("502"));
    assert_eq!(span.attribute("error.type"), Some("http_502"));
    assert_eq!(span.attribute("gateway.outcome"), Some("connect_error"));
    assert_eq!(
        span.attribute("upstream.authority"),
        Some("127.0.0.1:1"),
        "the authority the selection took from the base URL"
    );
    assert_eq!(
        span.attribute("upstream.api_family"),
        Some("deepseek"),
        "the registry family of the selected provider"
    );
    assert_eq!(span.parent_span_id.as_deref(), Some(caller_span));
    assert!(
        span.status.starts_with("error"),
        "a 5xx fails the span: {span:#?}"
    );
}

/// The unresolvable-upstream path: the gateway answers a retryable 503
/// and records the reason on the failure line, while the resolver's
/// error class goes to the counter -- so a resolver outage reads as a
/// dependency failure rather than a gateway bug. The host carries a
/// space, so the lookup is rejected locally as a malformed name before
/// any query leaves the host: the class is `lookup_error` with no name
/// server having to be reachable.
#[tokio::test]
async fn a_forward_to_an_unresolvable_upstream_is_a_retryable_503() {
    let db = migrated_test_db().await;
    crate::test_support::seed_registry(&db, "http://bad host/v1").await;
    let lines = global_lines();
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
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get("retry-after").map(|v| v.as_bytes()),
        Some(&b"5"[..]),
        "the answer carries the retry hint"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "the selected upstream could not be resolved; retry shortly"
    );

    await_forward_count(&metrics, crate::metrics::ForwardOutcome::ResolveError, 1).await;
    let exposition = metrics.render();
    assert!(
        exposition.contains(
            "kallipai_model_gateway_upstream_resolution_failures_total{error_class=\"lookup_error\",upstream=\"bad host\"} 1"
        ),
        "the resolution counter:\n{exposition}"
    );

    let line = await_line(&lines, "reason=unresolved_upstream").await;
    for field in [
        "endpoint=chat",
        "outcome=resolve_error",
        "tagma_id=tagma-under-test",
        "reason=unresolved_upstream",
        "error=",
        "connected=false",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
    assert!(
        line.contains("bad host"),
        "the host rides the error text: {line}"
    );
}

/// An upstream mock that answers the chat path, recording the body it
/// received and how many requests it served.
struct MockUpstream {
    base: String,
    body: std::sync::Arc<tokio::sync::Mutex<Vec<u8>>>,
    hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

async fn spawn_mock_upstream_counting_body() -> MockUpstream {
    use axum::routing::post;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("local addr");
    let body = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let body_for_handler = body.clone();
    let hits_for_handler = hits.clone();
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |received: axum::body::Bytes| {
            let body = body_for_handler.clone();
            let hits = hits_for_handler.clone();
            async move {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                body.lock().await.extend_from_slice(&received);
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "id": "cmpl-candidate-1",
                        "object": "chat.completion",
                        "choices": [{"message": {"role": "assistant", "content": "hi"}}],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
                    })),
                )
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock upstream serve");
    });
    MockUpstream {
        base: format!("http://{addr}"),
        body,
        hits,
    }
}

/// The resolver answer the tests script: every host resolves to the same
/// addresses, in the order given.
struct FixedAnswers(Vec<std::net::IpAddr>);

#[async_trait::async_trait]
impl Lookup for FixedAnswers {
    async fn lookup(&self, _host: &str) -> std::io::Result<Vec<std::net::IpAddr>> {
        Ok(self.0.clone())
    }
}

fn port_of(base: &str) -> &str {
    base.rsplit(':').next().expect("a base URL carries a port")
}

/// The data plane over a resolver scripted with `answers`: the candidate
/// list every dial walks is the resolver's answer, in its order.
async fn spawn_plane_with_addresses(
    db: crate::db::Db,
    answers: &[&str],
) -> (String, crate::metrics::Metrics) {
    let addr = next_addr();
    let metrics = crate::metrics::Metrics::private();
    let state = AppState {
        db,
        public_base_url: "http://gw.test:7501".to_string(),
        management: crate::test_support::test_management(),
        identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
        metrics: metrics.clone(),
    };
    let answers = answers
        .iter()
        .map(|answer| answer.parse().expect("an ip literal"))
        .collect();
    let resolver = Resolver::with_lookup(
        metrics.clone(),
        Duration::from_secs(1),
        std::sync::Arc::new(FixedAnswers(answers)),
    );
    spawn_data_plane_with_resolver(addr.clone(), state, String::new(), resolver)
        .expect("spawn data plane");
    wait_for_listener(&addr);
    (format!("http://{addr}"), metrics)
}

/// A listener that accepts and closes at once: the dial succeeds and the
/// request dies on the way in, which is the failure a retry must not
/// paper over. Returns the count of connections it took.
fn spawn_dropping_listener(
    addr: std::net::SocketAddr,
) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    let listener = std::net::TcpListener::bind(addr).expect("bind dropping listener");
    listener
        .set_nonblocking(true)
        .expect("a non-blocking listener");
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    std::thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    drop(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
    });
    accepted
}

/// The first candidate refuses the dial and pingora walks to the second:
/// the answer is the second candidate's, that candidate served it once,
/// and the refused dial is counted under one forwarded request.
#[tokio::test]
async fn a_refused_candidate_falls_over_to_the_next_one() {
    let db = migrated_test_db().await;
    let mock = spawn_mock_upstream_counting_body().await;
    let upstream = format!("http://upstream.test:{}", port_of(&mock.base));
    crate::test_support::seed_registry(&db, &upstream).await;
    let (base, metrics) = spawn_plane_with_addresses(db, &["127.0.0.2", "127.0.0.1"]).await;

    let body = serde_json::json!({
        "model": "deepseek-chat",
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p1"),
        &body,
        &[],
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(mock.hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        metrics.connect_failure_count(crate::metrics::ConnectErrorClass::Refused),
        1
    );
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::Upstream2xx, 1).await;
}

/// The dial fails before the request body is read, so the second dial
/// hands the upstream the body whole: 100 KiB arrives byte for byte.
#[tokio::test]
async fn a_body_bearing_retry_reaches_the_upstream_whole() {
    let db = migrated_test_db().await;
    let mock = spawn_mock_upstream_counting_body().await;
    let upstream = format!("http://upstream.test:{}", port_of(&mock.base));
    crate::test_support::seed_registry(&db, &upstream).await;
    let (base, metrics) = spawn_plane_with_addresses(db, &["127.0.0.2", "127.0.0.1"]).await;

    let payload = "x".repeat(100 * 1024);
    let body = serde_json::json!({
        "model": "deepseek-chat",
        "messages": [{"role": "user", "content": payload}]
    })
    .to_string();
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p1"),
        &body,
        &[],
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(mock.body.lock().await.clone(), body.as_bytes());
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::Upstream2xx, 1).await;
}

/// Every candidate is dialled once and the request dies as one connect
/// failure: the walk does not revisit an address to fill a retry count,
/// and the client reads a 502.
#[tokio::test]
async fn exhausting_the_candidates_costs_one_dial_each() {
    let db = migrated_test_db().await;
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe");
        probe.local_addr().expect("local addr").port()
    };
    let upstream = format!("http://upstream.test:{port}");
    crate::test_support::seed_registry(&db, &upstream).await;
    let (base, metrics) = spawn_plane_with_addresses(db, &["127.0.0.2", "127.0.0.3"]).await;

    let body = serde_json::json!({
        "model": "deepseek-chat",
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p1"),
        &body,
        &[],
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert_eq!(
        metrics.connect_failure_count(crate::metrics::ConnectErrorClass::Refused),
        2
    );
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::ConnectError, 1).await;
}

/// A dial that connects and then dies on the way in is not replayed: the
/// address answered the handshake, so the failure is upstream-side and
/// the second candidate is never dialled.
#[tokio::test]
async fn an_upstream_that_drops_the_connection_is_not_replayed() {
    let db = migrated_test_db().await;
    let mock = spawn_mock_upstream_counting_body().await;
    let port = port_of(&mock.base).to_string();
    let dropping: std::net::SocketAddr =
        format!("127.0.0.2:{port}").parse().expect("a socket addr");
    let accepted = spawn_dropping_listener(dropping);
    let upstream = format!("http://upstream.test:{port}");
    crate::test_support::seed_registry(&db, &upstream).await;
    let (base, metrics) = spawn_plane_with_addresses(db, &["127.0.0.2", "127.0.0.1"]).await;

    let body = serde_json::json!({
        "model": "deepseek-chat",
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    let response = post_generation(
        &base,
        "/chat/completions",
        TEST_TAGMA_BEARER,
        Some("p1"),
        &body,
        &[],
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert!(accepted.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    assert_eq!(mock.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        metrics.connect_failure_count(crate::metrics::ConnectErrorClass::Refused),
        0
    );
    await_forward_count(&metrics, crate::metrics::ForwardOutcome::UpstreamError, 1).await;
}
