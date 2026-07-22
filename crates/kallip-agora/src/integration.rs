//! Router-level integration tests for the cross-cutting middleware and the
//! challenge GC sweep. These exercise the real `routes::router` wiring (CSRF,
//! per-client rate limiting) end-to-end, which the handler-level unit tests
//! cannot reach.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::db::entity::webauthn_challenges;
use crate::routes;
use crate::session::{CSRF_HEADER, CSRF_HEADER_VALUE};
use crate::test_helpers::make_state_with;
use kallip_common::authtoken::TokenHash;
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
    let state = make_state_with(10, 10).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None);
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
    let state = make_state_with(10, 10).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None);
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
    let state = make_state_with(10, 10).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None);
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

/// Seed a live session for a fresh user and return the cookie value to send.
async fn seed_session(state: &crate::state::SharedState) -> String {
    use crate::db::entity::{sessions, users};
    use kallip_common::authtoken::MintedToken;
    use sea_orm::ActiveValue::Set;
    let now = OffsetDateTime::now_utc();
    let user_id = kallip_agora_common::ids::UserId::random();
    users::ActiveModel {
        id: Set(user_id.to_string()),
        username: Set("alice".to_string()),
        email: Set("alice@example.test".to_string()),
        display_name: Set(None),
        created_at: Set(now),
        disabled_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("seed user");
    let session = MintedToken::generate(crate::token::SESSION);
    sessions::ActiveModel {
        token_hash: Set(session.hash().as_bytes().to_vec()),
        user_id: Set(user_id.to_string()),
        created_at: Set(now),
        expires_at: Set(now + time::Duration::hours(1)),
    }
    .insert(&state.db)
    .await
    .expect("seed session");
    session.secret().to_string()
}

/// `POST /v1/tagmata` (mint a pending tagma) is CSRF-gated: a cookie-bearing
/// mint without the marker is 403.
#[tokio::test]
async fn csrf_guard_blocks_tagma_mint_without_marker() {
    let state = make_state_with(10, 10).await;
    let cookie = seed_session(&state).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None);
    let mut request = req(Method::POST, "/v1/tagmata", "{}");
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_str(&format!("kallip_session={cookie}")).expect("cookie header"),
    );
    assert_eq!(run(app, request).await, StatusCode::FORBIDDEN);
}

/// With the marker, the same mint passes the guard and returns 200 (it actually
/// mints a code, since the seeded session authenticates).
#[tokio::test]
async fn tagma_mint_with_marker_returns_200() {
    let state = make_state_with(10, 10).await;
    let cookie = seed_session(&state).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None);
    let mut request = req(Method::POST, "/v1/tagmata", "{}");
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_str(&format!("kallip_session={cookie}")).expect("cookie header"),
    );
    request.headers_mut().append(
        HeaderName::from_static(CSRF_HEADER),
        HeaderValue::from_static(CSRF_HEADER_VALUE),
    );
    assert_eq!(run(app, request).await, StatusCode::OK);
}

/// `DELETE /v1/tagmata/{id}` (revoke) passes the CSRF guard with the marker and
/// reaches the handler (404 for an unknown id -- not 403).
#[tokio::test]
async fn tagma_revoke_with_marker_reaches_handler() {
    let state = make_state_with(10, 10).await;
    let cookie = seed_session(&state).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None);
    let mut request = req(
        Method::DELETE,
        "/v1/tagmata/00000000-0000-0000-0000-000000000000",
        "",
    );
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_str(&format!("kallip_session={cookie}")).expect("cookie header"),
    );
    request.headers_mut().append(
        HeaderName::from_static(CSRF_HEADER),
        HeaderValue::from_static(CSRF_HEADER_VALUE),
    );
    assert_eq!(run(app, request).await, StatusCode::NOT_FOUND);
}

/// The begin endpoints share one per-client bucket; the finish endpoints do
/// NOT (a ceremony id is unguessable + single-use, transitively bounded by
/// begin's limiter). With a capacity-2 bucket, the 3rd begin is 429 while any
/// number of finishes never are.
#[tokio::test]
async fn rate_limit_begins_but_not_finishes() {
    let state = make_state_with(2, 0).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None);

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

/// `POST /v1/tagmata/enroll` shares the begin bucket; the 3rd call is 429.
#[tokio::test]
async fn rate_limit_enroll() {
    let state = make_state_with(2, 0).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None);
    for _ in 0..2 {
        let request = req(Method::POST, "/v1/tagmata/enroll", r#"{"code":"x"}"#);
        assert_ne!(
            run(app.clone(), request).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    let request = req(Method::POST, "/v1/tagmata/enroll", r#"{"code":"x"}"#);
    assert_eq!(
        run(app.clone(), request).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

/// The challenge GC sweep deletes only expired rows.
#[tokio::test]
async fn gc_sweep_deletes_expired_only() {
    let state = make_state_with(10, 10).await;
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
            email: Set(None),
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

/// `/internal/*` is NOT mounted when no shared secret is configured: the whole
/// surface is absent (404), so there is nothing to reach even with a guess.
#[tokio::test]
async fn internal_surface_absent_without_token() {
    let state = make_state_with(10, 10).await;
    let app = routes::router(state, None);
    let mut request = req(
        Method::POST,
        "/internal/verify-session",
        r#"{"cookie":"x"}"#,
    );
    request.headers_mut().append(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer internal-secret"),
    );
    assert_eq!(run(app, request).await, StatusCode::NOT_FOUND);
}

/// A request to `/internal/*` with no bearer is rejected 401.
#[tokio::test]
async fn internal_guard_rejects_missing_bearer() {
    let state = make_state_with(10, 10).await;
    let app = routes::router(state, Some(TokenHash::of("internal-secret")));
    let request = req(
        Method::POST,
        "/internal/verify-session",
        r#"{"cookie":"x"}"#,
    );
    assert_eq!(run(app, request).await, StatusCode::UNAUTHORIZED);
}

/// A request with the wrong bearer is rejected 401.
#[tokio::test]
async fn internal_guard_rejects_wrong_bearer() {
    let state = make_state_with(10, 10).await;
    let app = routes::router(state, Some(TokenHash::of("internal-secret")));
    let mut request = req(
        Method::POST,
        "/internal/verify-session",
        r#"{"cookie":"x"}"#,
    );
    request.headers_mut().append(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer sk-wrong"),
    );
    assert_eq!(run(app, request).await, StatusCode::UNAUTHORIZED);
}

/// The correct bearer passes the guard and reaches the handler (an unknown
/// session maps to 404, NOT 401, proving the guard let it through).
#[tokio::test]
async fn internal_guard_passes_correct_bearer() {
    let state = make_state_with(10, 10).await;
    let app = routes::router(state, Some(TokenHash::of("internal-secret")));
    let mut request = req(
        Method::POST,
        "/internal/verify-session",
        r#"{"cookie":"x"}"#,
    );
    request.headers_mut().append(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer internal-secret"),
    );
    assert_eq!(
        run(app, request).await,
        StatusCode::NOT_FOUND,
        "correct bearer reaches the handler (404 for unknown session)"
    );
}
