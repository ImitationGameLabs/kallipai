//! The two observability endpoints and the middleware that feeds one of them.
//!
//! Both endpoints are opt-out flags, both live on the tagma's existing
//! listener (no second port, no second service), and both are **loopback
//! only**: a scrape is unauthenticated by design, so it must not be reachable
//! from whatever network the operator bound. The peer-gate middleware decides
//! that from the connection's peer address, which is why the serve call uses
//! `into_make_service_with_connect_info`. A missing peer address is a refusal,
//! not a pass: an in-process call that cannot prove loopback gets the same
//! answer as a remote one.
//!
//! `/metrics` renders the text exposition format (Prometheus 0.0.4). A render
//! failure answers 503 with no body of counters: half a snapshot would read as
//! a sudden drop to zero, and `rate()` over a torn sample is worse than none.
//!
//! `/health` answers 200 for `ok` and `degraded`, 503 for `not_ready` (see
//! [`super::health`] for why degradation is not a 5xx).
//!
//! The timing middleware is deliberately thin: it records the matched route
//! (never the raw path -- a path carries ids, and a label with ids in it grows
//! without bound), the method, and the status class. Both endpoints are exempt
//! from their own accounting: a scrape's duration says something about the
//! scraper, and self-observation would make the histogram's tail depend on the
//! scrape interval.

use std::time::Instant;

use axum::Router;
use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};

use super::metrics;
use crate::state::SharedState;

/// One row per endpoint: the flag that mounts it, its path, and its handler.
struct Row {
    switch: Switch,
    path: &'static str,
    handler: fn() -> MethodRouter<SharedState>,
}

/// The endpoints this module serves: exempt from the HTTP timing middleware
/// and from the server span, so a scrape leaves neither a series nor a trace
/// entry.
///
/// One table holds the whole set: [`router`] mounts from it, and
/// [`EXEMPT_ROUTES`] is its path column, widened at compile time, so adding an
/// endpoint is one row plus one [`Switch`] variant. axum does not report what
/// a router has mounted, so a test walks the rows to see each path answer.
const ROUTES: [Row; 2] = [
    Row {
        switch: Switch::Metrics,
        path: "/metrics",
        handler: || get(render_metrics),
    },
    Row {
        switch: Switch::Health,
        path: "/health",
        handler: || get(health),
    },
];

/// Which flag mounts a row of [`ROUTES`].
#[derive(Clone, Copy)]
enum Switch {
    Metrics,
    Health,
}

impl Switch {
    fn is_on(self, metrics_enabled: bool, health_enabled: bool) -> bool {
        match self {
            Switch::Metrics => metrics_enabled,
            Switch::Health => health_enabled,
        }
    }
}

/// The path column of [`ROUTES`], filled at compile time so a row added to the
/// table joins the exemption without a second edit here.
const fn exempt_routes() -> [&'static str; ROUTES.len()] {
    let mut paths = [""; ROUTES.len()];
    let mut row = 0;
    while row < ROUTES.len() {
        paths[row] = ROUTES[row].path;
        row += 1;
    }
    paths
}

/// Every path in [`ROUTES`], for the layers that answer outside the app.
pub(crate) const EXEMPT_ROUTES: [&str; ROUTES.len()] = exempt_routes();

/// The method names this listener answers. Any other token is counted as
/// `OTHER_METHOD`: the timing layer sits outside the token check, so a peer
/// that cannot authenticate must not be able to mint a series per invented
/// method name.
const KNOWN_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// The one label value a method outside `KNOWN_METHODS` is counted under.
const OTHER_METHOD: &str = "other";

/// The observability routes, each present only when its flag is on. The
/// peer gate wraps the whole set, so an enabled endpoint is never reachable
/// off-host.
pub fn router(metrics_enabled: bool, health_enabled: bool) -> Router<SharedState> {
    let mut router = Router::new();
    for row in ROUTES {
        if row.switch.is_on(metrics_enabled, health_enabled) {
            router = router.route(row.path, (row.handler)());
        }
    }
    router.layer(axum::middleware::from_fn(peer_gate))
}

/// The `text/plain` exposition Prometheus scrapes.
async fn render_metrics() -> Response {
    match metrics().render() {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "rendering the metric snapshot failed");
            (StatusCode::SERVICE_UNAVAILABLE, "metrics unavailable\n").into_response()
        }
    }
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

    async fn call(
        metrics_enabled: bool,
        health_enabled: bool,
        path: &str,
        peer: Option<SocketAddr>,
    ) -> (StatusCode, String) {
        let mut request = Request::builder()
            .uri(path)
            .body(Body::empty())
            .expect("a well-formed request");
        if let Some(peer) = peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        let response = router(metrics_enabled, health_enabled)
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

    #[tokio::test]
    async fn a_loopback_peer_reads_the_snapshot() {
        let (status, body) = call(true, false, "/metrics", Some(loopback())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("kallipai_tagma_relays_configured"), "{body}");
    }

    /// A dual-stack listener reports a loopback peer as `::ffff:127.0.0.1`.
    /// The gate canonicalises the address, so such a peer is served rather
    /// than refused for looking like a stranger.
    #[tokio::test]
    async fn a_v4_mapped_loopback_peer_reads_the_snapshot() {
        let mapped = std::net::SocketAddr::from(([0, 0, 0, 0, 0, 0xffff, 0x7f00, 1], 40000));
        let (status, _) = call(true, false, "/metrics", Some(mapped)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_remote_peer_is_refused_at_both_endpoints() {
        assert_eq!(
            call(true, true, "/metrics", Some(remote())).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(true, true, "/health", Some(remote())).await.0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn a_peer_that_cannot_be_placed_is_refused() {
        assert_eq!(
            call(true, true, "/metrics", None).await.0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn a_switched_off_endpoint_is_not_routed_at_all() {
        assert_eq!(
            call(false, false, "/metrics", Some(loopback())).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(false, false, "/health", Some(loopback())).await.0,
            StatusCode::NOT_FOUND
        );
    }
    /// One table holds the set: every path the router mounts is exempt from the
    /// accounting it would otherwise dominate, and every exempt path is one the
    /// router mounts. The exemption column follows the table at compile time;
    /// the mounting half is answered for each row here.
    #[tokio::test]
    async fn one_table_routes_and_exempts_the_same_paths() {
        assert!(Switch::Metrics.is_on(true, false));
        assert!(!Switch::Metrics.is_on(false, true));
        assert!(Switch::Health.is_on(false, true));
        assert!(!Switch::Health.is_on(true, false));

        for row in ROUTES {
            let (status, _) = call(true, true, row.path, Some(loopback())).await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{} is mounted when its flag is on",
                row.path
            );
        }
    }

    #[tokio::test]
    async fn health_answers_a_report_a_monitor_can_read() {
        let (status, body) = call(false, true, "/health", Some(loopback())).await;
        // A test state has no history store, so the honest answer is 503 with
        // the failing check named -- and that is exactly what a monitor needs.
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("\"status\":\"not_ready\""), "{body}");
        assert!(body.contains("\"store\":{\"ok\":false}"), "{body}");
    }
    /// The timing layer's own contract: a routed request is counted once, an
    /// unmatched path is counted under the placeholder, and neither
    /// observability endpoint leaves a series -- the exemption is by path, so a
    /// scrape's cadence never becomes a measurement of the process.
    #[tokio::test]
    async fn the_timing_layer_counts_requests_and_exempts_the_observability_endpoints() {
        fn app() -> axum::Router {
            router(true, true)
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
            let text = metrics().render().expect("the registry renders");
            // The encoder sorts labels, so the route is not the first one.
            let series = format!("route=\"{route}\"");
            text.lines()
                .filter(|line| line.starts_with("kallipai_tagma_http_requests_total{"))
                .filter(|line| line.contains(&series))
                .map(|line| {
                    line.rsplit(' ')
                        .next()
                        .expect("a value")
                        .parse::<u64>()
                        .expect("a count")
                })
                .sum()
        }

        fn counted_method(method: &str) -> u64 {
            let text = metrics().render().expect("the registry renders");
            let series = format!("method=\"{method}\"");
            text.lines()
                .filter(|line| line.starts_with("kallipai_tagma_http_requests_total{"))
                .filter(|line| line.contains(&series))
                .map(|line| {
                    line.rsplit(' ')
                        .next()
                        .expect("a value")
                        .parse::<u64>()
                        .expect("a count")
                })
                .sum()
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

        // The exemption walks the table rather than naming its paths, so a row
        // added later is covered here without a new line, and a path that
        // starts counting fails with its own name in the message.
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
