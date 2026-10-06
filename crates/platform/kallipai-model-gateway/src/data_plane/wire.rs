//! Bridges between the axum-shaped business pieces and the pingora
//! downstream session.
//!
//! The business layer (dialect error envelopes, the distribution
//! handlers, `ApiError`) keeps speaking axum types; the data plane
//! buffers those (always small: JSON documents and error bodies) and
//! writes them through the session. Forwarded traffic never passes
//! through here -- the framework proxies it verbatim.

use axum::http::{HeaderName, HeaderValue, header};
use axum::response::IntoResponse;
use bytes::Bytes;
use pingora::Result;
use pingora::http::ResponseHeader;
use pingora::prelude::Session;

/// The CORS policy, parsed once at spawn: the configured origin
/// allowlist (a wildcard entry, or any origin the header grammar
/// rejects, is dropped -- both leave the empty allowlist, the same
/// promise the `cors_layer` doc makes). The header semantics mirror
/// the tower-http layer the axum face installed: a request whose
/// `Origin` is listed gets that origin echoed plus the advertised
/// method/header lists with credentials on; anything else gets no
/// CORS header at all.
#[derive(Clone, Debug)]
pub struct CorsPolicy {
    origins: Vec<HeaderValue>,
}

impl CorsPolicy {
    pub fn parse(origins: &str) -> Self {
        Self {
            origins: origins
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "*")
                .filter_map(|s| HeaderValue::from_str(s).ok())
                .collect(),
        }
    }

    /// The CORS response headers for this request's origin, if it is
    /// allowed. The method and header lists are the layer's full
    /// advertisements (the route table's real surface), and credentials
    /// stay on.
    pub fn headers(&self, request_origin: Option<&str>) -> Vec<(HeaderName, HeaderValue)> {
        let Some(origin) = request_origin.and_then(|o| {
            self.origins
                .iter()
                .find(|allowed| allowed.to_str().is_ok_and(|a| a == o))
        }) else {
            return Vec::new();
        };
        vec![
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone()),
            (
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, POST, PUT, DELETE"),
            ),
            (
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("authorization, content-type"),
            ),
            (
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            ),
        ]
    }
}

/// The buffering bound for bridge-written responses. Every response that
/// takes this path is a distribution JSON document or an error envelope;
/// a generation body is proxied by the framework and never lands here.
const BRIDGE_BUFFER: usize = 32 * 1024 * 1024;

/// Write an axum-shaped response to the downstream session.
pub async fn write_axum_response(
    session: &mut Session,
    response: axum::response::Response,
) -> Result<()> {
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, BRIDGE_BUFFER)
        .await
        .unwrap_or_else(|_| Bytes::from_static(b"{}"));
    let mut header = ResponseHeader::build(parts.status.as_u16(), None)?;
    for (name, value) in parts.headers.iter() {
        header.insert_header(name, value)?;
    }
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session.write_response_body(Some(bytes), true).await
}

/// Write an `IntoResponse` value (an `ApiError`, a `Json<T>`...) with the
/// CORS headers applied for the request's origin, then tell the phase to
/// stop.
pub async fn respond_axum(
    session: &mut Session,
    response: impl IntoResponse,
    cors: &CorsPolicy,
    origin: Option<&str>,
) -> Result<bool> {
    let mut response = response.into_response();
    for (name, value) in cors.headers(origin) {
        response.headers_mut().insert(name, value);
    }
    write_axum_response(session, response).await?;
    Ok(true)
}

/// The CORS preflight test: an OPTIONS carrying both the browser's
/// `Origin` and the `Access-Control-Request-Method` probe. A preflight
/// is answered here, never forwarded.
pub fn is_cors_preflight(session: &Session) -> bool {
    let req = session.req_header();
    req.method == axum::http::Method::OPTIONS
        && req.headers.contains_key(axum::http::header::ORIGIN)
        && req
            .headers
            .contains_key(axum::http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_policy_drops_wildcard_and_invalid_entries() {
        let policy = CorsPolicy::parse(" https://ok.example , * , bad\u{7f} ");
        assert_eq!(policy.headers(Some("https://ok.example")).len(), 4);
        assert!(policy.headers(Some("https://other.example")).is_empty());
        assert!(policy.headers(None).is_empty());
    }
}
