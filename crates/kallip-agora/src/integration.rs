//! Router-level integration tests for the cross-cutting middleware and the
//! challenge GC sweep. These exercise the real `routes::router` wiring (CSRF,
//! per-client rate limiting) end-to-end, which the handler-level unit tests
//! cannot reach.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::db::entity::webauthn_challenges;
use crate::routes;
use crate::session::{CSRF_HEADER, CSRF_HEADER_VALUE};
use crate::test_helpers::make_state_with;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
use time::OffsetDateTime;

const PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);

/// Build a JSON request with the `MockConnectInfo` extension the rate-limit
/// middleware reads (the `ConnectInfo` extractor falls back to it outside a
/// real socket).
fn req(method: Method, uri: &str, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    request.extensions_mut().insert(MockConnectInfo(PEER));
    request
}

/// Drive a request through the router and return its status (draining the body
/// so the response is fully consumed).
async fn run(app: axum::Router, request: Request<Body>) -> StatusCode {
    let resp = app.oneshot(request).await.expect("router responds");
    let status = resp.status();
    let _ = resp.into_body().collect().await;
    status
}

/// A cookie-bearing mutating request without the CSRF marker is rejected 403.
#[tokio::test]
async fn csrf_guard_blocks_cookie_post_without_marker() {
    let state = make_state_with(Duration::from_secs(2), 10, 10).await;
    let app = routes::router(state);
    let mut request = req(
        Method::POST,
        "/v1/auth/login/begin",
        r#"{"username":"someone"}"#,
    );
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_static("kallip_session=fake-opaque-value"),
    );
    assert_eq!(run(app, request).await, StatusCode::FORBIDDEN);
}

/// A cookie-bearing mutating request WITH the marker passes the CSRF guard
/// (status is whatever the handler returns, not 403).
#[tokio::test]
async fn csrf_guard_passes_cookie_post_with_marker() {
    let state = make_state_with(Duration::from_secs(2), 10, 10).await;
    let app = routes::router(state);
    let mut request = req(
        Method::POST,
        "/v1/auth/login/begin",
        r#"{"username":"someone"}"#,
    );
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_static("kallip_session=fake-opaque-value"),
    );
    request.headers_mut().append(
        HeaderName::from_static(CSRF_HEADER),
        HeaderValue::from_static(CSRF_HEADER_VALUE),
    );
    assert_ne!(
        run(app, request).await,
        StatusCode::FORBIDDEN,
        "marker present: must pass the CSRF guard"
    );
}

/// A request carrying a bearer is exempt from the CSRF marker requirement
/// (bearer auth is not browser-CSRFable). It must not be 403'd.
#[tokio::test]
async fn csrf_guard_exempts_bearer() {
    let state = make_state_with(Duration::from_secs(2), 10, 10).await;
    let app = routes::router(state);
    let mut request = req(
        Method::POST,
        "/v1/auth/login/begin",
        r#"{"username":"someone"}"#,
    );
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_static("kallip_session=fake-opaque-value"),
    );
    request.headers_mut().append(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer sk-tagma-fake"),
    );
    assert_ne!(
        run(app, request).await,
        StatusCode::FORBIDDEN,
        "bearer present: exempt from the CSRF marker"
    );
}

/// The begin endpoints share one per-client bucket; the finish endpoints do
/// NOT (a ceremony id is unguessable + single-use, transitively bounded by
/// begin's limiter). With a capacity-2 bucket, the 3rd begin is 429 while any
/// number of finishes never are.
#[tokio::test]
async fn rate_limit_begins_but_not_finishes() {
    let state = make_state_with(Duration::from_secs(2), 2, 0).await;
    let app = routes::router(state);

    // Two begins exhaust the bucket (handler may 400 on the username; we only
    // care it is not yet 429).
    for _ in 0..2 {
        let request = req(
            Method::POST,
            "/v1/auth/login/begin",
            r#"{"username":"someone"}"#,
        );
        assert_ne!(
            run(app.clone(), request).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    // The third begin trips the limiter.
    let request = req(
        Method::POST,
        "/v1/auth/login/begin",
        r#"{"username":"someone"}"#,
    );
    assert_eq!(
        run(app.clone(), request).await,
        StatusCode::TOO_MANY_REQUESTS
    );

    // Finish is NOT under the begin limiter; repeated calls never 429 (they
    // 400/404 on the bogus ceremony id instead).
    for _ in 0..5 {
        let request = req(
            Method::POST,
            "/v1/auth/login/finish",
            r#"{"ceremony_id":"00000000-0000-0000-0000-000000000000"}"#,
        );
        assert_ne!(
            run(app.clone(), request).await,
            StatusCode::TOO_MANY_REQUESTS,
            "finish is gated by ceremony id, not the rate limiter"
        );
    }
}

/// `POST /v1/tagmata` (enroll) shares the begin bucket; the 3rd call is 429.
#[tokio::test]
async fn rate_limit_enroll() {
    let state = make_state_with(Duration::from_secs(2), 2, 0).await;
    let app = routes::router(state);
    for _ in 0..2 {
        let request = req(Method::POST, "/v1/tagmata", r#"{"code":"x"}"#);
        assert_ne!(
            run(app.clone(), request).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    let request = req(Method::POST, "/v1/tagmata", r#"{"code":"x"}"#);
    assert_eq!(
        run(app.clone(), request).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

/// The challenge GC sweep deletes only expired rows.
#[tokio::test]
async fn gc_sweep_deletes_expired_only() {
    let state = make_state_with(Duration::from_secs(2), 10, 10).await;
    let now = OffsetDateTime::now_utc();
    // One expired, one live.
    for expires in [
        now - time::Duration::seconds(60),
        now + time::Duration::seconds(60),
    ] {
        webauthn_challenges::ActiveModel {
            id: Set(uuid::Uuid::new_v4()),
            kind: Set("login".to_string()),
            state: Set(serde_json::Value::Null),
            invite_code_hash: Set(None),
            user_id: Set(None),
            username: Set(None),
            expires_at: Set(expires),
            created_at: Set(now),
        }
        .insert(&state.db)
        .await
        .expect("seed challenge");
    }
    crate::db::gc_expired_challenges(&state.db).await;
    let remaining = webauthn_challenges::Entity::find()
        .all(&state.db)
        .await
        .expect("read challenges");
    assert_eq!(remaining.len(), 1, "only the live row remains");
    assert!(remaining[0].expires_at > now);
}
