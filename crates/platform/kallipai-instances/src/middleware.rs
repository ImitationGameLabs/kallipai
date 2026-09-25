//! Cross-cutting middleware: the CSRF custom-header guard.
//!
//! Mirrors the archeion's and the lesche's `csrf_guard` (same two-pillar defense:
//! a `SameSite=Strict` session cookie plus a custom `X-Requested-With` header
//! the browser cannot synthesize cross-origin without a preflight). The
//! cookie auth channel (`token_guard`'s Platform branch) is what opens the
//! CSRF surface here; stateless requests (GET/HEAD/OPTIONS) and any request
//! that carries no session cookie (the bearer-authenticated app/CLI routes)
//! pass through untouched. A cookie-bearing mutating request MUST also carry
//! `X-Requested-With: kallipai`. A request carrying BOTH a session cookie and
//! a valid bearer is exempt: the bearer header is itself proof of intent.

use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// The custom-header CSRF marker name. Lowercase: HTTP headers are
/// case-insensitive, and axum canonicalises on read.
pub const CSRF_HEADER: &str = "x-requested-with";

/// The custom-header CSRF marker value.
pub const CSRF_HEADER_VALUE: &str = "kallipai";

/// Session cookie name. Mirrors the archeion's `kallipai_session`; a stable wire
/// literal kept here (duplicated, as in the lesche, so this crate does not
/// pull the registry's session module).
const SESSION_COOKIE_NAME: &str = "kallipai_session";

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

/// Read the session cookie value from a request's `Cookie` header, if present.
/// Mirrors the archeion/lesche helper: multiple `Cookie` headers and multiple
/// `name=value` pairs within one are both tolerated; first match wins.
pub(crate) fn read_session_cookie(headers: &HeaderMap) -> Option<String> {
    for header in headers.get_all(axum::http::header::COOKIE) {
        let Ok(s) = header.to_str() else {
            continue;
        };
        for cookie in cookie::Cookie::split_parse(s) {
            let Ok(cookie) = cookie else {
                continue;
            };
            if cookie.name() == SESSION_COOKIE_NAME {
                return Some(cookie.value().to_string());
            }
        }
    }
    None
}
