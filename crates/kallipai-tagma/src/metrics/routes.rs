//! The health endpoint and the middleware that feeds the metrics.
//!
//! The endpoint is an opt-out flag, it lives on the tagma's existing listener
//! (no second port, no second service), and it is **loopback only**: `/health`
//! carries no credential, so it must not be reachable from whatever network
//! the operator bound. The peer-gate middleware decides that from the
//! connection's peer address, which is why the serve call uses
//! `into_make_service_with_connect_info`. A missing peer address is a refusal,
//! not a pass: an in-process call that cannot prove loopback gets the same
//! answer as a remote one.
//!
//! `/health` answers 200 for `ok` and `degraded`, 503 for `not_ready` (see
//! [`super::health`] for why degradation is not a 5xx).

use std::time::Instant;

use axum::Router;
use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::metrics;
use crate::state::SharedState;

/// The health path, and the whole exempt set: the timing layer skips it, and
/// the router mounts it under the flag it belongs to.
const HEALTH_PATH: &str = "/health";

/// The one path this module serves, exempt from the HTTP timing middleware and
/// from the server span: a monitor's cadence says nothing about the process's
/// work, and an entry per probe would be noise in the traces.
pub(crate) const EXEMPT_ROUTES: [&str; 1] = [HEALTH_PATH];

/// The method names this listener answers. Any other token is counted as
/// `OTHER_METHOD`: the timing layer sits outside the token check, so a peer
/// that cannot authenticate must not be able to mint a series per invented
/// method name.
const KNOWN_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// The one label value a method outside `KNOWN_METHODS` is counted under.
const OTHER_METHOD: &str = "other";

/// The health route, present only when its flag is on. The peer gate wraps it,
/// so an enabled endpoint is never reachable off-host.
pub fn router(health_enabled: bool) -> Router<SharedState> {
    let mut router = Router::new();
    if health_enabled {
        router = router.route(HEALTH_PATH, get(health));
    }
    router.layer(axum::middleware::from_fn(peer_gate))
}

/// The health answer: a status code plus the report of what was checked.
async fn health(State(state): State<SharedState>) -> Response {
    let report = super::health::evaluate(&state);
    (report.status().status_code(), axum::Json(report)).into_response()
}

/// Refuse anything that is not loopback.
async fn peer_gate(request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|connect| connect.0.ip());
    match peer {
        Some(address) if address.to_canonical().is_loopback() => next.run(request).await,
        other => {
            tracing::debug!(
                peer = ?other,
                "refused an observability request from a non-loopback peer"
            );
            (
                StatusCode::FORBIDDEN,
                "observability endpoints are loopback-only\n",
            )
                .into_response()
        }
    }
}

/// Count and time every request, routed or not. Applied to the whole app: the
/// route is the matched pattern, so parameterised paths collapse into one
/// series, and a request that matched nothing is counted under
/// [`UNMATCHED_ROUTE`](super::UNMATCHED_ROUTE)
/// so a 404 sweep is visible instead of silent. The method is one of
/// `KNOWN_METHODS` or `OTHER_METHOD`, because a label has to stay bounded:
/// the layer runs outside the token check, so the method name is peer-controlled.
pub async fn http_timing(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or(super::UNMATCHED_ROUTE.to_owned(), |matched| {
            matched.as_str().to_owned()
        });
    let method = request.method().as_str().to_owned();
    let method = if KNOWN_METHODS.contains(&method.as_str()) {
        method
    } else {
        OTHER_METHOD.to_owned()
    };
    let started = Instant::now();
    let response = next.run(request).await;
    if !EXEMPT_ROUTES.contains(&route.as_str()) {
        metrics().record_http(
            &route,
            method.as_str(),
            response.status().as_u16(),
            started.elapsed(),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;

    fn loopback() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 40000))
    }

    fn remote() -> SocketAddr {
        SocketAddr::from(([10, 1, 2, 3], 40000))
    }

    async fn call(health_enabled: bool, peer: Option<SocketAddr>) -> (StatusCode, String) {
        let mut request = Request::builder()
            .uri(HEALTH_PATH)
            .body(Body::empty())
            .expect("a well-formed request");
        if let Some(peer) = peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        let response = router(health_enabled)
            .with_state(crate::test_helpers::make_state())
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("the body reads");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// A dual-stack listener reports a loopback peer as `::ffff:127.0.0.1`.
    /// The gate canonicalises the address, so such a peer is served rather
    /// than refused for looking like a stranger.
    #[tokio::test]
    async fn a_loopback_peer_reads_the_report() {
        assert_eq!(
            call(true, Some(loopback())).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let mapped = SocketAddr::from(([0, 0, 0, 0, 0, 0xffff, 0x7f00, 1], 40000));
        assert_eq!(
            call(true, Some(mapped)).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn a_remote_peer_is_refused() {
        assert_eq!(call(true, Some(remote())).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_peer_that_cannot_be_placed_is_refused() {
        assert_eq!(call(true, None).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_switched_off_endpoint_is_not_routed_at_all() {
        assert_eq!(call(false, Some(loopback())).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn health_answers_a_report_a_monitor_can_read() {
        let (status, body) = call(true, Some(loopback())).await;
        // A test state has no history store, so the honest answer is 503 with
        // the failing check named -- and that is exactly what a monitor needs.
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("\"status\":\"not_ready\""), "{body}");
        assert!(body.contains("\"history_store\":{\"ok\":false}"), "{body}");
    }

    /// The timing layer's own contract: a routed request is counted once, an
    /// unmatched path is counted under the placeholder, the health endpoint
    /// leaves no series, and an invented method lands on the bounded label.
    #[tokio::test]
    async fn the_timing_layer_counts_requests_and_exempts_the_health_endpoint() {
        fn app() -> axum::Router {
            router(true)
                .route("/probe_timing", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(http_timing))
                .with_state(crate::test_helpers::make_state())
        }

        fn request(path: &str) -> Request<Body> {
            let mut request = Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a well-formed request");
            request.extensions_mut().insert(ConnectInfo(loopback()));
            request
        }

        fn request_with(method: &str, path: &str) -> Request<Body> {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .expect("a well-formed request");
            request.extensions_mut().insert(ConnectInfo(loopback()));
            request
        }

        fn counted(route: &str) -> u64 {
            crate::metrics::test_meter::counter_matching(
                "kallipai_tagma.http_requests",
                ("route", route),
            )
        }

        fn counted_method(method: &str) -> u64 {
            crate::metrics::test_meter::counter_matching(
                "kallipai_tagma.http_requests",
                ("method", method),
            )
        }

        let routed = counted("/probe_timing");
        let unmatched = counted(super::super::UNMATCHED_ROUTE);
        let exempt: Vec<u64> = EXEMPT_ROUTES.iter().map(|path| counted(path)).collect();

        app()
            .oneshot(request("/probe_timing"))
            .await
            .expect("the router answers");
        assert_eq!(
            counted("/probe_timing"),
            routed + 1,
            "a matched request is counted once, under its route template"
        );

        app()
            .oneshot(request("/nothing_matches_this"))
            .await
            .expect("the router answers");
        assert_eq!(
            counted(super::super::UNMATCHED_ROUTE),
            unmatched + 1,
            "a request that matched no route is counted under the placeholder"
        );

        // The method is a label and this layer runs outside the token check:
        // an invented method must not mint a series of its own.
        let other = counted_method(OTHER_METHOD);
        assert_eq!(counted_method("FOO"), 0, "no series for an invented method");
        app()
            .oneshot(request_with("FOO", "/probe_timing"))
            .await
            .expect("the router answers");
        assert_eq!(
            counted_method(OTHER_METHOD),
            other + 1,
            "an invented method is counted under the bounded label"
        );
        assert_eq!(counted_method("FOO"), 0, "and never under its own name");

        // The exempt path is mounted and leaves no series behind: the
        // exemption is by path, so a probe's cadence never becomes a
        // measurement of the process.
        for (path, before) in EXEMPT_ROUTES.iter().zip(exempt) {
            let status = app()
                .oneshot(request(path))
                .await
                .expect("the router answers")
                .status();
            assert_ne!(status, StatusCode::NOT_FOUND, "{path} is mounted");
            assert_eq!(counted(path), before, "{path} leaves no series");
        }
    }
}
