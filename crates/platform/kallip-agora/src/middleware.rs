//! Cross-cutting middleware: the CSRF custom-header guard and the per-IP
//! auth rate limiter.
//!
//! Both are `axum::middleware::from_fn` functions (extractors as leading
//! params) so they compose as layers on the routers in [`crate::routes`]
//! without needing state bound at layer-construction time.

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;

use kallip_common::authtoken::TokenHash;

use crate::session::{CSRF_HEADER, CSRF_HEADER_VALUE, read_session_cookie};
use crate::state::SharedState;

/// CSRF guard. Stateless-changing requests (GET/HEAD/OPTIONS) and any request
/// that carries no session cookie (machine / bearer — herald, enroll, admin
/// token) pass through untouched. A cookie-bearing mutating request MUST also
/// carry `X-Requested-With: kallip`; browsers block custom headers on
/// cross-origin fetches without preflight, so a CSRF-forged form/fetch cannot
/// synthesize it. Combined with `SameSite=Strict`, this is the two-pillar
/// defense. A request that carries BOTH a session cookie and a valid bearer
/// token is exempt: the bearer header is itself proof of intent (browsers do
/// not auto-attach it cross-origin), so it authenticates without the marker.
/// Returns 403 on a missing marker for a cookie-only mutating request.
pub async fn csrf_guard(
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let is_state_changing = !matches!(
        method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if is_state_changing
        && read_session_cookie(&headers).is_some()
        && kallip_common::auth_header::extract_bearer_token(&headers).is_err()
    {
        let has_marker = headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case(CSRF_HEADER_VALUE))
            .unwrap_or(false);
        if !has_marker {
            return (StatusCode::FORBIDDEN, "missing CSRF marker").into_response();
        }
    }
    next.run(request).await
}

/// Guard for the `/internal/*` ControlPlane surface (consumed by the lesche).
/// Runs as `from_fn_with_state(expected_hash, internal_guard)` so the expected
/// hash is baked into the layer, independent of the handlers'
/// `State<SharedState>`. Rejects any request whose `Authorization: Bearer`
/// token does not hash to the expected shared secret. The comparison is
/// constant-time: this is a high-value service-to-service secret. A missing or
/// non-matching bearer is `401`; the `/internal` nest is mounted only when the
/// secret is configured, so reaching this guard at all means one is expected.
pub async fn internal_guard(
    State(expected): State<TokenHash>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let token = match kallip_common::auth_header::extract_bearer_token(&headers) {
        Ok(t) => t,
        Err(_) => return (StatusCode::UNAUTHORIZED, "missing bearer").into_response(),
    };
    if !expected.ct_eq(&TokenHash::of(token)) {
        return (StatusCode::UNAUTHORIZED, "invalid internal token").into_response();
    }
    next.run(request).await
}

/// Per-client rate limit for the unauthenticated auth surface. Charges one
/// token from the caller's bucket and returns 429 when empty. The bucket key is
/// the real client IP: when the direct peer is a configured trusted proxy, the
/// client IP is taken from `X-Forwarded-For` (rightmost untrusted hop);
/// otherwise the peer IP is used directly. Requires the server to be served
/// with `into_make_service_with_connect_info::<SocketAddr>()`.
pub async fn auth_rate_limit(
    State(state): State<SharedState>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let ip = crate::clientip::real_client_ip(&headers, addr.ip(), &state.trusted_proxies);
    if !state.auth_rate_limiter.check(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }
    next.run(request).await
}
