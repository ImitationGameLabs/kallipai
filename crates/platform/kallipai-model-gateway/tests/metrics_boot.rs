//! The metrics face at the real-binary level: every test boots the
//! gateway as a fresh child process against an isolated database, a
//! mock archeion, and an in-test upstream, then reads the /metrics
//! text the process itself exposes.
//!
//! The e2e discipline, point by point: one child process per test (a
//! fresh metrics registry, so every assertion starts from zero); no
//! real LLM (loopback fakes only); relative assertions only (before
//! and after deltas, never absolute counters, never timing); and full
//! isolation (loopback binds, tempdir data roots, per-test database
//! and ports, the child killed on drop). The gateway's Postgres shares
//! the reusable container with the in-process suite; Docker is needed
//! at test time, and the seeding rides the real /admin write path
//! instead of SQL.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ImageExt, ReuseDirective};
use tokio::sync::OnceCell;

/// The credentials the mock archeion admits: an admin principal for
/// the seeding calls and one enrolled tagma for the forwards.
const ADMIN_BEARER: &str = "e2e-admin-bearer";
const TAGMA_BEARER: &str = "e2e-tagma-bearer";
const TAGMA_ID: &str = "tagma-e2e";
/// The account the tagma is enrolled to (the mock's enrollment answer).
const OWNER_ACCOUNT: &str = "user-e2e";

// -- the shared Postgres (the in-process suite's container, reused) ------

static SHARED_PG_PORT: OnceCell<u16> = OnceCell::const_new();
static DB_COUNTER: AtomicU64 = AtomicU64::new(0);
const CONTAINER_NAME: &str = "kallipai-testcontainers-pg-model-gateway";
const DB_PREFIX: &str = "gateway_e2e_";

async fn shared_pg_port() -> u16 {
    *SHARED_PG_PORT
        .get_or_init(|| async {
            let request = || {
                Postgres::default()
                    .with_db_name("postgres")
                    .with_user("postgres")
                    .with_password("postgres")
                    .with_tag("16-alpine")
                    .with_container_name(CONTAINER_NAME)
                    .with_reuse(ReuseDirective::Always)
            };
            for attempt in 0..4 {
                match request().start().await {
                    Ok(container) => {
                        let port = container.get_host_port_ipv4(5432).await.expect("host port");
                        // Leaked on purpose: the container outlives every
                        // test in this binary.
                        std::mem::forget(container);
                        return port;
                    }
                    // A sibling binary cold-starting the same reusable
                    // container can win the create; the retry rides its
                    // result.
                    Err(e) if attempt < 3 && e.to_string().contains("already in use") => {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    Err(e) => panic!("start postgres: {e}"),
                }
            }
            unreachable!("the retry loop returns or panics")
        })
        .await
}

/// One isolated database inside the shared container; the child
/// process runs the migrations at boot, so the URL is all the test
/// hands over.
async fn fresh_db_url() -> String {
    let port = shared_pg_port().await;
    let n = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let db_name = format!("{DB_PREFIX}{}_{n}", std::process::id());
    let root_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let root = Database::connect(&root_url)
        .await
        .expect("connect to the maintenance database");
    root.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("DROP DATABASE IF EXISTS \"{db_name}\""),
    ))
    .await
    .expect("drop a stale database");
    root.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("CREATE DATABASE \"{db_name}\""),
    ))
    .await
    .expect("create the test database");
    format!("postgres://postgres:postgres@127.0.0.1:{port}/{db_name}")
}

use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tempfile::TempDir;
use tokio::net::TcpListener;

// -- ports -------------------------------------------------------------

/// Two distinct loopback ports, picked under simultaneous binds so the
/// two cannot collide, then released for the child to take. The
/// bind-release window is the standard boot-test race; a quiet test
/// host keeps it negligible.
async fn two_free_ports() -> (u16, u16) {
    let a = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the first port");
    let b = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the second port");
    let data = a.local_addr().expect("the first address").port();
    let mgmt = b.local_addr().expect("the second address").port();
    assert_ne!(data, mgmt, "the two planes must not share a port");
    (data, mgmt)
}

/// A loopback base URL whose port is guaranteed closed: bind, read the
/// port, drop the listener. The provider seeded against it cannot be
/// reached, which is exactly the point.
async fn closed_port_base() -> String {
    let l = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the doomed port");
    let port = l.local_addr().expect("the doomed address").port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

// -- the mock archeion (the identity authority, in-test) ----------------

/// `/internal/verify-bearer`: the two principals the credentials in
/// this file name, `404` for every other token (the contract's
/// no-such-principal answer). The internal-token guard is not
/// exercised: the fake trusts its caller, a posture this file states
/// instead of simulating.
async fn verify_bearer(Json(body): Json<serde_json::Value>) -> Response {
    let principal = match body["token"].as_str() {
        Some(t) if t == ADMIN_BEARER => json!({"kind": "admin"}),
        Some(t) if t == TAGMA_BEARER => json!({"kind": "tagma", "tagma_id": TAGMA_ID}),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (StatusCode::OK, Json(json!({"principal": principal}))).into_response()
}

/// `/internal/enrollment-lookup`: one enrolled tagma under one owner,
/// `404` for any other id (unknown and revoked collapse there).
async fn enrollment_lookup(Json(body): Json<serde_json::Value>) -> Response {
    match body["tagma_id"].as_str() {
        Some(id) if id == TAGMA_ID => (
            StatusCode::OK,
            Json(json!({"user_id": OWNER_ACCOUNT, "enrolled_tagmas": [TAGMA_ID]})),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn spawn_mock_archeion() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the mock archeion");
    let addr = listener.local_addr().expect("the mock archeion address");
    let app = axum::Router::new()
        .route(
            "/internal/verify-bearer",
            axum::routing::post(verify_bearer),
        )
        .route(
            "/internal/enrollment-lookup",
            axum::routing::post(enrollment_lookup),
        );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

// -- the canned upstream (the fake LLM) --------------------------------

/// An upstream that answers every request with the canned status and
/// body. It records nothing: the assertions read the gateway's own
/// metrics, never the upstream's view of the traffic.
async fn spawn_canned_upstream(status: u16, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the canned upstream");
    let addr = listener.local_addr().expect("the canned upstream address");
    tokio::spawn(async move {
        let code = StatusCode::from_u16(status).expect("a valid canned status");
        let app = axum::Router::new().fallback(move || async move { (code, body).into_response() });
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

// -- the gateway child process -----------------------------------------

/// The child plus the tempdirs that must outlive it: the guard kills
/// and reaps the process on drop, before the directories go away.
struct GatewayChild {
    child: Child,
    _log_dir: TempDir,
    _token_dir: TempDir,
    _home_dir: TempDir,
}

impl Drop for GatewayChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One fresh gateway process: the real binary, a scrubbed environment
/// (PATH and a tempdir HOME only), its own loopback ports, a fresh
/// database whose migrations the child runs itself at boot, and the
/// mock archeion as its identity authority. `/health` on the
/// management plane is the ready signal; the wait is bounded and
/// surfaces an early child exit instead of spinning the clock.
async fn boot_gateway(db_url: &str, archeion_url: &str) -> (GatewayChild, String, String) {
    let (data_port, mgmt_port) = two_free_ports().await;
    let log_dir = tempfile::tempdir().expect("the log dir");
    let token_dir = tempfile::tempdir().expect("the token dir");
    let home_dir = tempfile::tempdir().expect("the home dir");
    let token_path = token_dir.path().join("internal-token");
    std::fs::write(&token_path, "e2e-internal-token\n").expect("write the internal token");

    let child = Command::new(env!("CARGO_BIN_EXE_kallipai-model-gateway"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home_dir.path())
        .env(
            "KALLIPAI_MODEL_GATEWAY_ADDR",
            format!("127.0.0.1:{data_port}"),
        )
        .env(
            "KALLIPAI_MODEL_GATEWAY_MANAGEMENT_ADDR",
            format!("127.0.0.1:{mgmt_port}"),
        )
        .env("KALLIPAI_MODEL_GATEWAY_DATABASE_URL", db_url)
        .env("KALLIPAI_MODEL_GATEWAY_ARCHEION_URL", archeion_url)
        .env("KALLIPAI_MODEL_GATEWAY_INTERNAL_TOKEN_FILE", &token_path)
        .env("KALLIPAI_MODEL_GATEWAY_LOG_DIR", log_dir.path())
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the gateway binary");

    let mut guard = GatewayChild {
        child,
        _log_dir: log_dir,
        _token_dir: token_dir,
        _home_dir: home_dir,
    };
    let data = format!("http://127.0.0.1:{data_port}");
    let mgmt = format!("http://127.0.0.1:{mgmt_port}");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(status)) = guard.child.try_wait() {
            panic!("the gateway child exited during boot: {status}");
        }
        if std::time::Instant::now() > deadline {
            panic!("the gateway child never became healthy on {mgmt}/health");
        }
        if matches!(
            reqwest::get(format!("{mgmt}/health")).await,
            Ok(resp) if resp.status().is_success()
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    (guard, data, mgmt)
}

// -- the seeding (the real /admin write path, no SQL) ------------------

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build the test client")
}

/// One admin (or tagma) write, asserted to land: the fail-fast form,
/// so a seeding regression names the exact step that broke.
async fn call(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    bearer: &str,
    body: serde_json::Value,
) {
    let response = http
        .request(method, &url)
        .bearer_auth(bearer)
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{url}: {e}"));
    let status = response.status();
    assert!(status.is_success(), "{url} -> {status}");
}

/// Bring one tagma to a forwardable state through the real faces: a
/// provider pointed at the (fake) upstream, a collection published to
/// the reserved everyone audience, a parked profile promoted into the
/// collection's first set, and the tagma's selection pointing at that
/// collection. Seven writes in dependency order; the first set of a
/// collection anchors as its default, so the selection resolves
/// without naming a set.
async fn seed(mgmt: &str, upstream_base: &str) {
    let http = http();
    call(
        &http,
        reqwest::Method::POST,
        format!("{mgmt}/admin/providers"),
        ADMIN_BEARER,
        json!({
            "provider_id": "p1",
            "family": "deepseek",
            "base_url": upstream_base,
            "api_key": "sk-e2e",
        }),
    )
    .await;
    call(
        &http,
        reqwest::Method::POST,
        format!("{mgmt}/admin/collections"),
        ADMIN_BEARER,
        json!({"name": "main", "description": "the e2e collection"}),
    )
    .await;
    call(
        &http,
        reqwest::Method::POST,
        format!("{mgmt}/admin/collections/main/publications"),
        ADMIN_BEARER,
        json!({"audience": "everyone"}),
    )
    .await;
    call(
        &http,
        reqwest::Method::POST,
        format!("{mgmt}/admin/parking"),
        ADMIN_BEARER,
        json!({"profile_id": "p1", "provider_id": "p1", "model": "deepseek-chat"}),
    )
    .await;
    call(
        &http,
        reqwest::Method::POST,
        format!("{mgmt}/admin/collections/main/sets"),
        ADMIN_BEARER,
        json!({"name": "alpha", "description": "the e2e set"}),
    )
    .await;
    call(
        &http,
        reqwest::Method::PATCH,
        format!("{mgmt}/admin/sets/alpha"),
        ADMIN_BEARER,
        json!({"members": ["p1"]}),
    )
    .await;
    // The selection is the tagma's own write: the owner must be named
    // because the default is the caller's account, not the catalog.
    call(
        &http,
        reqwest::Method::PUT,
        format!("{mgmt}/selection"),
        TAGMA_BEARER,
        json!({"owner": "system", "collection": "main"}),
    )
    .await;
}

// -- the metrics reader ------------------------------------------------

/// The last value of one fully named series line, matched on the
/// whole first token so a histogram's parent or bucket lines cannot
/// shadow the `_count` child the caller asked for. Series names
/// carry their label set in the encoder's order: labels sorted by
/// name, not by the family's declaration order. `None` = the
/// series is absent from the text (a zero-sample state, not a zero).
fn metric_value(text: &str, series: &str) -> Option<f64> {
    let line = text
        .lines()
        .find(|l| l.split_whitespace().next() == Some(series))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Poll `/metrics` until the series' value has grown by the wanted
/// delta over the baseline: the terminal logging phase lands
/// asynchronously to the response the caller already holds, so a
/// one-shot read races the record.
async fn await_metric(mgmt: &str, series: &str, baseline: f64, want_delta: f64) {
    for _ in 0..200 {
        let text = fetch_metrics(mgmt).await;
        if matches!(metric_value(&text, series), Some(v) if v - baseline >= want_delta) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let text = fetch_metrics(mgmt).await;
    let now = metric_value(&text, series);
    panic!("{series}: wanted a delta of {want_delta} over {baseline}, holds {now:?}");
}

async fn fetch_metrics(mgmt: &str) -> String {
    reqwest::get(format!("{mgmt}/metrics"))
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("the metrics body")
}

// -- the tests ---------------------------------------------------------

/// The boot smoke: a fresh process answers /health, its /metrics
/// carries the build stamp and the process face, and the forwarding
/// families hold zero samples (the zero-sample state: the family
/// headers exist, the sample lines do not).
#[tokio::test]
async fn boot_exposes_the_process_face_from_a_clean_registry() {
    let db = fresh_db_url().await;
    let archeion = spawn_mock_archeion().await;
    let (_child, _data, mgmt) = boot_gateway(&db, &archeion).await;

    let health = reqwest::get(format!("{mgmt}/health"))
        .await
        .expect("GET /health")
        .text()
        .await
        .expect("the health body");
    assert_eq!(health, "ok");

    let text = fetch_metrics(&mgmt).await;
    assert!(text.contains(&format!(
        "kallipai_model_gateway_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(
        text.contains("process_"),
        "the process face belongs to the same exposition"
    );
    assert!(
        !text.contains("kallipai_model_gateway_forward_total{"),
        "a fresh registry must not carry forwarding samples"
    );
}

/// The happy path: three forwards through the fake upstream land in
/// the 2xx outcome bucket with an upstream-keyed time-to-first-header
/// observation each, and the in-flight gauge settles back to zero
/// (a level, not a counter: absolute zero is the meaningful read).
#[tokio::test]
async fn happy_forwards_count_by_outcome_and_drain_in_flight() {
    let db = fresh_db_url().await;
    let archeion = spawn_mock_archeion().await;
    let upstream = spawn_canned_upstream(200, "{\"canned\": true}").await;
    let (_child, data, mgmt) = boot_gateway(&db, &archeion).await;
    seed(&mgmt, &upstream).await;

    let before = fetch_metrics(&mgmt).await;
    assert!(
        !before.contains("kallipai_model_gateway_forward_total{"),
        "seeding must not forward anything"
    );

    let http = http();
    for _ in 0..3 {
        let response = http
            .post(format!("{data}/chat/completions"))
            .bearer_auth(TAGMA_BEARER)
            .json(&json!({"model": "deepseek-chat", "messages": []}))
            .send()
            .await
            .expect("POST /chat/completions");
        assert!(response.status().is_success());
    }

    await_metric(
        &mgmt,
        r#"kallipai_model_gateway_forward_total{endpoint="chat",outcome="upstream_2xx"}"#,
        0.0,
        3.0,
    )
    .await;
    let after = fetch_metrics(&mgmt).await;
    let authority = upstream.strip_prefix("http://").unwrap_or(&upstream);
    let ttfb = metric_value(
        &after,
        &format!(
            "kallipai_model_gateway_upstream_time_to_first_header_seconds_count{{upstream=\"{authority}\"}}"
        ),
    )
    .expect("a ttfb sample for the upstream");
    assert!(ttfb >= 1.0, "each forwarded request observes once: {ttfb}");
    assert_eq!(
        metric_value(
            &after,
            &format!(
                "kallipai_model_gateway_upstream_responses_total{{api_family=\"deepseek\",status_class=\"2xx\",upstream=\"{authority}\"}}"
            ),
        )
        .expect("an attribution sample for the upstream"),
        3.0
    );
    assert_eq!(
        metric_value(&after, "kallipai_model_gateway_forward_in_flight")
            .expect("the in-flight gauge"),
        0.0
    );
}

/// An upstream that answers 5xx: the forwards land in the 5xx bucket,
/// never in the 2xx one, and the gauge drains all the same.
#[tokio::test]
async fn upstream_5xx_lands_in_the_5xx_bucket() {
    let db = fresh_db_url().await;
    let archeion = spawn_mock_archeion().await;
    let upstream = spawn_canned_upstream(500, "{\"canned\": false}").await;
    let (_child, data, mgmt) = boot_gateway(&db, &archeion).await;
    seed(&mgmt, &upstream).await;

    let http = http();
    for _ in 0..2 {
        let response = http
            .post(format!("{data}/chat/completions"))
            .bearer_auth(TAGMA_BEARER)
            .json(&json!({"model": "deepseek-chat", "messages": []}))
            .send()
            .await
            .expect("POST /chat/completions");
        assert_eq!(response.status().as_u16(), 500);
    }

    await_metric(
        &mgmt,
        r#"kallipai_model_gateway_forward_total{endpoint="chat",outcome="upstream_5xx"}"#,
        0.0,
        2.0,
    )
    .await;
    let authority = upstream.strip_prefix("http://").unwrap_or(&upstream);
    await_metric(
        &mgmt,
        &format!(
            "kallipai_model_gateway_upstream_responses_total{{api_family=\"deepseek\",status_class=\"5xx\",upstream=\"{authority}\"}}"
        ),
        0.0,
        2.0,
    )
    .await;
    let after = fetch_metrics(&mgmt).await;
    assert_eq!(
        metric_value(
            &after,
            r#"kallipai_model_gateway_forward_total{endpoint="chat",outcome="upstream_2xx"}"#
        ),
        None,
        "a 5xx answer must not touch the 2xx series"
    );
    assert_eq!(
        metric_value(&after, "kallipai_model_gateway_forward_in_flight")
            .expect("the in-flight gauge"),
        0.0
    );
}

/// A provider whose upstream is a closed port: every forward dies at
/// the dial, the request-level outcome lands in the connect bucket,
/// and the attempt-level failure counter records the refused class
/// (as a floor, not an exact count: retries may dial again).
#[tokio::test]
async fn connect_refusal_lands_in_the_connect_bucket() {
    let db = fresh_db_url().await;
    let archeion = spawn_mock_archeion().await;
    let closed = closed_port_base().await;
    let (_child, data, mgmt) = boot_gateway(&db, &archeion).await;
    seed(&mgmt, &closed).await;

    let http = http();
    for _ in 0..2 {
        let response = http
            .post(format!("{data}/chat/completions"))
            .bearer_auth(TAGMA_BEARER)
            .json(&json!({"model": "deepseek-chat", "messages": []}))
            .send()
            .await
            .expect("POST /chat/completions");
        assert!(
            !response.status().is_success(),
            "the gateway answers for its dead upstream"
        );
    }

    await_metric(
        &mgmt,
        r#"kallipai_model_gateway_forward_total{endpoint="chat",outcome="connect_error"}"#,
        0.0,
        2.0,
    )
    .await;
    let authority = closed.strip_prefix("http://").unwrap_or(&closed);
    await_metric(
        &mgmt,
        &format!(
            "kallipai_model_gateway_upstream_connect_failures_total{{error_class=\"refused\",upstream=\"{authority}\"}}"
        ),
        0.0,
        2.0,
    )
    .await;
    let after = fetch_metrics(&mgmt).await;
    assert_eq!(
        metric_value(&after, "kallipai_model_gateway_forward_in_flight")
            .expect("the in-flight gauge"),
        0.0
    );
}
