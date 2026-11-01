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
use kallipai_common::authtoken::TokenHash;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
use time::OffsetDateTime;

const PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);

/// Drive a request through the router and read its status and body (draining
/// the body either way).
async fn run_with_body(app: axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let resp = app.oneshot(request).await.expect("router responds");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

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
    let app = routes::router(state, None, false);
    let mut request = req(
        Method::POST,
        "/auth/login/begin",
        r#"{"username":"someone"}"#,
    );
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_static("kallipai_session=fake-opaque-value"),
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
    let app = routes::router(state, None, false);
    let mut request = req(
        Method::POST,
        "/auth/login/begin",
        r#"{"username":"someone"}"#,
    );
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_static("kallipai_session=fake-opaque-value"),
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
    let app = routes::router(state, None, false);
    let mut request = req(
        Method::POST,
        "/auth/login/begin",
        r#"{"username":"someone"}"#,
    );
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_static("kallipai_session=fake-opaque-value"),
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
    seed_user_and_session(state).await.0
}

/// The same fixture, keeping the user id so a test can issue a credential for
/// the very account the cookie belongs to.
async fn seed_user_and_session(
    state: &crate::state::SharedState,
) -> (String, kallipai_archeion_common::ids::UserId) {
    use crate::db::entity::{sessions, users};
    use kallipai_common::authtoken::MintedToken;
    use sea_orm::ActiveValue::Set;
    let now = OffsetDateTime::now_utc();
    let user_id = kallipai_archeion_common::ids::UserId::random();
    users::ActiveModel {
        id: Set(user_id.to_string()),
        username: Set("alice".to_string()),
        display_name: Set(None),
        created_at: Set(now),
        disabled_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("seed user");
    let session = MintedToken::generate(crate::token::SESSION).expect("getrandom failed");
    sessions::ActiveModel {
        token_hash: Set(session.hash().as_bytes().to_vec()),
        user_id: Set(user_id.to_string()),
        created_at: Set(now),
        expires_at: Set(now + time::Duration::hours(1)),
        authed_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("seed session");
    (session.secret().to_string(), user_id)
}

/// Seed a personal access token holding every string in the vocabulary for
/// `user`, and return its plaintext.
async fn seed_full_pat(
    state: &crate::state::SharedState,
    user: &kallipai_archeion_common::ids::UserId,
) -> String {
    let perms = kallipai_archeion_common::perm::PermSet::from_known(
        kallipai_archeion_common::perm::VOCABULARY.iter().copied(),
    )
    .expect("the vocabulary is known");
    let minted = crate::credentials::create(
        &state.db,
        crate::token::PAT,
        crate::credentials::NewCredential {
            owner: crate::credentials::Owner::User,
            owner_id: user.as_ref(),
            name: "script",
            perms: &perms,
            created_by: user.as_ref(),
            actor: Some(user.as_ref()),
            expires_at: None,
        },
    )
    .await
    .expect("seed a personal token");
    minted.plaintext
}

/// Every route that may only be driven by a session: minting or resetting
/// credentials, the sign-in methods, and logout. A bearer with the whole
/// vocabulary must not reach one of them.
const SESSION_FACES: &[(Method, &str, &str)] = &[
    (Method::GET, "/me/emails", ""),
    (
        Method::POST,
        "/me/emails",
        r#"{"address":"alice@example.com"}"#,
    ),
    (
        Method::POST,
        "/me/emails/00000000-0000-0000-0000-000000000000",
        "{}",
    ),
    (
        Method::DELETE,
        "/me/emails/00000000-0000-0000-0000-000000000000",
        "",
    ),
    (Method::GET, "/me/passkeys", ""),
    (Method::POST, "/me/passkeys/register/begin", "{}"),
    (
        Method::PATCH,
        "/me/passkeys/00000000-0000-0000-0000-000000000000",
        r#"{"label":"x"}"#,
    ),
    (
        Method::DELETE,
        "/me/passkeys/00000000-0000-0000-0000-000000000000",
        "",
    ),
    (Method::POST, "/me/oauth/google/begin", "{}"),
    (
        Method::DELETE,
        "/me/external-identities/00000000-0000-0000-0000-000000000000",
        "",
    ),
    (Method::POST, "/me/device-pairing", ""),
    (Method::POST, "/auth/logout", ""),
    (Method::GET, "/me/tokens", ""),
    (
        Method::POST,
        "/me/tokens",
        r#"{"name":"x","perms":["files:record:read"]}"#,
    ),
    (
        Method::PATCH,
        "/me/tokens/00000000-0000-0000-0000-000000000000",
        r#"{"name":"y"}"#,
    ),
    (
        Method::DELETE,
        "/me/tokens/00000000-0000-0000-0000-000000000000",
        "",
    ),
    (
        Method::GET,
        "/tagmata/00000000-0000-0000-0000-000000000000/credential",
        "",
    ),
    (
        Method::POST,
        "/tagmata/00000000-0000-0000-0000-000000000000/credential/reset",
        "",
    ),
];

/// A personal access token holding every string is still the token origin, and
/// these routes ask for the cookie origin: a full set buys nothing here.
/// Minting a tagma is deliberately not on this list: `POST /tagmata`
/// intersects on `archeion:tagma:write`, so both origins reach it.
#[tokio::test]
async fn a_full_personal_token_is_refused_on_every_session_face() {
    let state = make_state_with(100, 100).await;
    let (cookie, user_id) = seed_user_and_session(&state).await;
    let secret = seed_full_pat(&state, &user_id).await;
    let app = routes::router(state, None, false);

    for (method, uri, body) in SESSION_FACES {
        let mut request = req(method.clone(), uri, body);
        request.headers_mut().append(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {secret}")).expect("bearer header"),
        );
        let (status, text) = run_with_body(app.clone(), request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert!(
            text.contains("personal access token"),
            "{method} {uri} refuses without naming the origin: {text}"
        );
    }

    // The cookie is still the way in: the same listing answers 200.
    let mut request = req(Method::GET, "/me/emails", "");
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_str(&format!("kallipai_session={cookie}")).expect("cookie header"),
    );
    assert_eq!(run(app, request).await, StatusCode::OK);
}

/// `POST /tagmata` (mint a pending tagma) is CSRF-gated: a cookie-bearing
/// mint without the marker is 403.
#[tokio::test]
async fn csrf_guard_blocks_tagma_mint_without_marker() {
    let state = make_state_with(10, 10).await;
    let cookie = seed_session(&state).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None, false);
    let mut request = req(Method::POST, "/tagmata", "{}");
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_str(&format!("kallipai_session={cookie}")).expect("cookie header"),
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
    let app = routes::router(state, None, false);
    let mut request = req(Method::POST, "/tagmata", "{}");
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_str(&format!("kallipai_session={cookie}")).expect("cookie header"),
    );
    request.headers_mut().append(
        HeaderName::from_static(CSRF_HEADER),
        HeaderValue::from_static(CSRF_HEADER_VALUE),
    );
    assert_eq!(run(app, request).await, StatusCode::OK);
}

/// `DELETE /tagmata/{id}` (revoke) passes the CSRF guard with the marker and
/// reaches the handler (404 for an unknown id -- not 403).
#[tokio::test]
async fn tagma_revoke_with_marker_reaches_handler() {
    let state = make_state_with(10, 10).await;
    let cookie = seed_session(&state).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None, false);
    let mut request = req(
        Method::DELETE,
        "/tagmata/00000000-0000-0000-0000-000000000000",
        "",
    );
    request.headers_mut().append(
        axum::http::header::COOKIE,
        HeaderValue::from_str(&format!("kallipai_session={cookie}")).expect("cookie header"),
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
    let app = routes::router(state, None, false);

    // Two begins exhaust the bucket (handler may 400 on the username; we only
    // care it is not yet 429).
    for _ in 0..2 {
        let request = req(
            Method::POST,
            "/auth/login/begin",
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
        "/auth/login/begin",
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
            "/auth/login/finish",
            r#"{"ceremony_id":"00000000-0000-0000-0000-000000000000"}"#,
        );
        assert_ne!(
            run(app.clone(), request).await,
            StatusCode::TOO_MANY_REQUESTS,
            "finish is gated by ceremony id, not the rate limiter"
        );
    }
}

/// `POST /tagmata/enroll` shares the begin bucket; the 3rd call is 429.
#[tokio::test]
async fn rate_limit_enroll() {
    let state = make_state_with(2, 0).await;
    // No `/internal` surface is needed for these control-plane middleware
    // tests.
    let app = routes::router(state, None, false);
    for _ in 0..2 {
        let request = req(Method::POST, "/tagmata/enroll", r#"{"code":"x"}"#);
        assert_ne!(
            run(app.clone(), request).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    let request = req(Method::POST, "/tagmata/enroll", r#"{"code":"x"}"#);
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
            pairing_code_hash: Set(None),
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

/// `/internal/*` is NOT mounted when no shared secret is configured: the whole
/// surface is absent (404), so there is nothing to reach even with a guess.
#[tokio::test]
async fn internal_surface_absent_without_token() {
    let state = make_state_with(10, 10).await;
    let app = routes::router(state, None, false);
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
    let app = routes::router(state, Some(TokenHash::of("internal-secret")), false);
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
    let app = routes::router(state, Some(TokenHash::of("internal-secret")), false);
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
    let app = routes::router(state, Some(TokenHash::of("internal-secret")), false);
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

/// `/internal/enrollment-lookup` sits inside the guarded nest: no bearer is
/// 401, and a correct bearer reaches the handler (404 for an unknown tagma).
/// Pins the new route to the shared-secret surface rather than the public
/// one -- a registration outside the nest would fail these.
#[tokio::test]
async fn enrollment_lookup_guard_rejects_missing_bearer() {
    let state = make_state_with(10, 10).await;
    let app = routes::router(state, Some(TokenHash::of("internal-secret")), false);
    let request = req(
        Method::POST,
        "/internal/enrollment-lookup",
        r#"{"tagma_id":"tagma-abc"}"#,
    );
    assert_eq!(run(app, request).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn enrollment_lookup_guard_passes_correct_bearer() {
    let state = make_state_with(10, 10).await;
    let app = routes::router(state, Some(TokenHash::of("internal-secret")), false);
    let mut request = req(
        Method::POST,
        "/internal/enrollment-lookup",
        r#"{"tagma_id":"tagma-abc"}"#,
    );
    request.headers_mut().append(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer internal-secret"),
    );
    assert_eq!(
        run(app, request).await,
        StatusCode::NOT_FOUND,
        "correct bearer reaches the handler (404 for unknown tagma)"
    );
}
/// The admin-root probe is mounted at the no-trailing-slash path `/admin`:
/// axum 0.8 serves a `nest` + inner `"/"` route without the trailing slash, and
/// there is no normalization middleware. Pin both sides of that contract so the
/// client (`admin_verify_token`) and the route table cannot drift apart -- the
/// bug this guards against once made `kallipai-admin ping` always 404.
#[tokio::test]
async fn admin_probe_mounted_at_no_slash_path() {
    let state = make_state_with(10, 10).await;
    let app = routes::router(state, None, false);

    // Valid admin bearer: `make_state` stores `TokenHash::of("test-admin")`.
    let mut ok = Request::builder()
        .method(Method::GET)
        .uri("/admin")
        .header(axum::http::header::AUTHORIZATION, "Bearer test-admin")
        .body(Body::empty())
        .expect("build request");
    ok.extensions_mut().insert(MockConnectInfo(PEER));
    assert_eq!(
        run(app.clone(), ok).await,
        StatusCode::OK,
        "no-slash path with a valid admin bearer must reach the probe"
    );

    // The trailing-slash form is NOT registered under axum 0.8 nest.
    let mut slash = Request::builder()
        .method(Method::GET)
        .uri("/admin/")
        .header(axum::http::header::AUTHORIZATION, "Bearer test-admin")
        .body(Body::empty())
        .expect("build request");
    slash.extensions_mut().insert(MockConnectInfo(PEER));
    assert_eq!(
        run(app, slash).await,
        StatusCode::NOT_FOUND,
        "trailing-slash form is unregistered under axum 0.8 nest"
    );
}

/// A personal token holding the tagma-write string mints an enrollment code at
/// `POST /tagmata`: the write string is the one that renames and revokes a
/// tagma, and the code it mints belongs to the token's owner. A well-formed but
/// unknown bearer on the same request is a 401, so a rejected token and a
/// refused one stay distinguishable.
#[tokio::test]
async fn a_write_scoped_personal_token_can_mint_an_enrollment_code() {
    use kallipai_archeion_common::perm::{ARCHEION_TAGMA_READ, ARCHEION_TAGMA_WRITE, PermSet};

    use crate::credentials;
    use crate::test_helpers::seed_user;
    use crate::token::PAT;

    let state = make_state_with(10, 10).await;
    let user = seed_user(&state, "alice").await;
    let perms = PermSet::from_known([ARCHEION_TAGMA_READ, ARCHEION_TAGMA_WRITE]).expect("known");
    let minted = credentials::create(
        &state.db,
        PAT,
        credentials::NewCredential {
            owner: credentials::Owner::User,
            owner_id: user.as_ref(),
            name: "ci",
            perms: &perms,
            created_by: user.as_ref(),
            actor: Some(user.as_ref()),
            expires_at: None,
        },
    )
    .await
    .expect("seed a personal token");
    let app = routes::router(state, None, false);

    let authorize = |request: &mut Request<Body>, bearer: &str| {
        request.headers_mut().append(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {bearer}")).expect("bearer header"),
        );
    };

    let mut request = req(Method::POST, "/tagmata", "{}");
    authorize(&mut request, &minted.plaintext);
    let (status, body) = run_with_body(app.clone(), request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let response: serde_json::Value = serde_json::from_str(&body).expect("mint body");
    let code = response["code"].as_str().expect("code field");
    assert!(code.starts_with("sk-enroll-"), "{code}");
    assert!(
        response["id"].as_str().is_some_and(|id| !id.is_empty()),
        "{body}"
    );

    let mut request = req(Method::POST, "/tagmata", "{}");
    authorize(&mut request, "sk-pat-not-a-real-token");
    assert_eq!(run(app, request).await, StatusCode::UNAUTHORIZED);
}
