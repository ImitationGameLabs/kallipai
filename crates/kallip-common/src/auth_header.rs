//! Bearer-token extraction from an HTTP `Authorization` header, shared by
//! `kallip-daemon` and `kallip-agora`. Available only with the `axum` feature
//! (the only axum-bound symbol here is `axum::http::HeaderMap`).
//!
//! Both components resolve an incoming bearer token the same way: parse
//! `Authorization: Bearer <token>`, rejecting a missing header, a non-Bearer
//! scheme, or an empty token. Centralizing it keeps the parsing rule in one
//! place rather than drifting between the two crates' auth extractors.

use crate::protocol::ApiError;

/// Extract the bearer credential from an `Authorization: Bearer <token>` header.
///
/// Returns the token slice borrowed from `headers`. Errors map to 401 at the
/// route boundary: a missing/unreadable header, a non-`Bearer ` scheme, or an
/// empty token.
pub fn extract_bearer_token(headers: &axum::http::HeaderMap) -> Result<&str, ApiError> {
    let value = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("authentication required"))?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::unauthorized("invalid Authorization scheme, expected Bearer"))?;
    if token.is_empty() {
        return Err(ApiError::unauthorized("empty bearer token"));
    }
    Ok(token)
}
