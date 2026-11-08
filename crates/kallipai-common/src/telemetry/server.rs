//! The axum half of the spans: one middleware that opens the server span every
//! router answers under.
//!
//! The whole module sits behind the `axum` feature, so a process that only makes
//! calls compiles none of this. The span it opens is [`super::spans::request_span`]'s;
//! this file only supplies what the framework knows.

use super::spans::{record_response, request_span};

/// The route name a request gets when it matched no route at all.
pub(crate) const UNMATCHED_ROUTE: &str = "unmatched";

/// The server span for one request, as axum middleware: mount it with
/// [`with_server_span`], and every axum router mounts this one layer, so a
/// trace reads the same whichever service answered.
///
/// It adds the routing: the matched template (`GET /v1/files/{id}`) becomes
/// the route part of the span's name, or `unmatched` when nothing matched.
/// The name and the attributes are [`request_span`]'s.
///
/// The status is recorded once the response exists, and a server-side
/// failure also carries an error type and the failed status, so a trace can
/// be filtered for them.
///
/// A 4xx response only records `http.response.status_code`: the semantic
/// conventions reserve the error status and `error.type` for a 5xx on a
/// server span. A client span reads the same code as its own failure, so
/// the halves differ.
///
/// Absent a handoff, the span ends when the response head exists. For a
/// streaming body (SSE, a download) the duration is time to the first byte
/// rather than the transfer, and a body that fails mid-stream is not
/// recorded as an error on the span.
///
/// A handler that hands the span's handle to a task outliving the response
/// keeps the span open until that task ends, so such a route's duration
/// covers that work as well.
pub async fn server_span(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use tracing::Instrument as _;

    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or(UNMATCHED_ROUTE, axum::extract::MatchedPath::as_str);
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let span = request_span(method.as_str(), route, &path, request.headers());
    let response = next.run(request).instrument(span.clone()).await;
    record_response(&span, response.status().as_u16());
    response
}

/// Put [`server_span`] on a finished router, outermost, so the span covers the
/// layers inside it (body limit, CORS, CSRF) as well as the handler.
///
/// Whoever finishes assembling the router mounts it, which for some services
/// is `main` and for others the module that builds and serves the router. A new
/// service gets its span from one call at its own assembly point.
pub fn with_server_span<S>(router: axum::Router<S>) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(axum::middleware::from_fn(server_span))
}
