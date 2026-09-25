//! Cross-cutting middleware: the CSRF custom-header guard.
//!
//! Mirrors the archeion's `csrf_guard` (same two-pillar defense: a `SameSite=Strict`
//! session cookie plus a custom `X-Requested-With` header the browser cannot
//! synthesize cross-origin without a preflight). Stateless-changing requests
//! (GET/HEAD/OPTIONS) and any request that carries no session cookie (the
//! bearer-authenticated machine routes this relay serves -- tagma tunnel,
//! envelope POST, key-exchange) pass through untouched. A cookie-bearing
//! mutating request MUST also carry `X-Requested-With: kallipai`. A request
//! carrying BOTH a session cookie and a valid bearer is exempt: the bearer
//! header is itself proof of intent.

use crate::auth::read_session_cookie;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use kallipai_common::authtoken::TokenHash;

/// The custom-header CSRF marker name. Lowercase: HTTP headers are
/// case-insensitive, and axum canonicalises on read.
pub const CSRF_HEADER: &str = "x-requested-with";

/// The custom-header CSRF marker value.
pub const CSRF_HEADER_VALUE: &str = "kallipai";

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
        && kallipai_common::auth_header::extract_bearer_token(&headers).is_err()
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

/// Guard for the `/internal/*` service-to-service surface (consumed by the
/// files service for file-delivery event pushes). Runs as
/// `from_fn_with_state(expected_hash, internal_guard)` so the expected hash
/// is baked into the layer. Rejects any request whose `Authorization:
/// Bearer` token does not hash to the expected shared secret; the
/// comparison is constant-time (high-value service-to-service secret).
/// A missing or non-matching bearer is 401; the internal nest is mounted
/// only when the token is configured. Same shape as the archeion's twin
/// guard.
pub async fn internal_guard(
    State(expected): State<TokenHash>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let token = match kallipai_common::auth_header::extract_bearer_token(&headers) {
        Ok(t) => t,
        Err(_) => return (StatusCode::UNAUTHORIZED, "missing bearer").into_response(),
    };
    if !expected.ct_eq(&TokenHash::of(token)) {
        return (StatusCode::UNAUTHORIZED, "invalid internal token").into_response();
    }
    next.run(request).await
}
