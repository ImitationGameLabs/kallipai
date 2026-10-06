//! HTTP surface of the gateway, assembled as two physically separate
//! planes on two listeners of one process:
//!
//! - the data plane: the pingora face on the public address, whose
//!   routes live in `data_plane::routes` (the dispatch table, the
//!   forwarding POSTs and the health probe);
//! - the management plane: the /admin family and the distribution
//!   reads, each behind its own credential family.
//!
//! The split keeps the LLM-client-compatible surface and the house
//! namespace from constraining each other's paths or versions, and lets
//! a deployment keep the admin face off the public interface entirely
//! (the Kong proxy/admin precedent). Role modules are declared one level
//! up; this module assembles the route tables and the shared layers.

use axum::Router;
use axum::http::{HeaderValue, Method};
use axum::routing::{get, put};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::state::AppState;

pub use kallipai_common::protocol::ApiError;

/// Build the management-plane router: the /admin family behind its own
/// credential (AdminToken), the distribution reads behind theirs (the
/// tagma bearer): two disjoint route families on one listener. Health
/// and metrics are mounted here too: process probes, not members of
/// either route family. No explicit body limit is installed: the
/// payloads are small JSON, and axum's default 2 MiB cap is more than
/// enough (the 8 MiB forwarding cap is data-plane only).
pub fn management_plane_router(state: AppState, cors_origins: &str) -> Router {
    Router::new()
        .nest("/admin", crate::management::router())
        .nest("/user", crate::management::user_routes::router())
        // The distribution reads: the tagma-bearer-authenticated, secret-free
        // profile API, mounted at the root of the management face (the
        // api edge routes the stripped /v1/model-gateway segment here).
        .route("/sets", get(crate::distribution::get_sets))
        .route("/sets/{name}", get(crate::distribution::get_set))
        .route("/parking", get(crate::distribution::get_parking))
        .route(
            "/profiles/{profile_id}",
            get(crate::distribution::get_profile),
        )
        .route(
            "/selection",
            put(crate::distribution::put_selection).delete(crate::distribution::delete_selection),
        )
        .route(
            "/selected-collection",
            get(crate::distribution::get_selected_collection),
        )
        .route("/health", get(health))
        .route("/metrics", get(render_metrics))
        .with_state(state)
        // The CSRF guard (the cookie channel's second pillar) sits inside
        // the CORS layer: preflights answer before it, and its own
        // GET/HEAD/OPTIONS exemption keeps the distribution reads untouched.
        .layer(axum::middleware::from_fn(crate::management::csrf_guard))
        .layer(cors_layer(cors_origins))
}

/// GET /health: no authentication, on purpose -- compose healthcheck, the
/// files service's health route is the precedent. Mounted on the
/// management router here; the pingora data plane answers its own copy
/// in `request_filter`.
async fn health() -> &'static str {
    "ok"
}

/// GET /metrics: the Prometheus text exposition of the observability
/// face. No authentication, on purpose -- the same probe family as
/// /health (a scrape must survive credential outages), safe because
/// the production faces bind loopback.
async fn render_metrics(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
) -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
}

/// `cors_layer` (credentials-aware, explicit method list, never a wildcard
/// origin) -- the lesche variant, NOT the tagma permissive one. The method
/// list is hand-maintained: `GET` for the distribution reads, `POST`
/// for the forwarding surface, `PUT`/`PATCH`/`DELETE` for the admin face.
/// empty allowlist (no cross-origin allowed), never an open hole.
pub fn cors_layer(origins: &str) -> CorsLayer {
    let allowed: Vec<HeaderValue> = origins
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        // A literal "*" is not an origin: filter it so a misconfigured
        // wildcard yields exactly the empty allowlist the docs promise,
        // never a header entry that only exact-matches the string "*".
        .filter(|s| *s != "*")
        .filter_map(|s| s.parse().ok())
        .collect();
    let origin = if allowed.is_empty() {
        AllowOrigin::list(Vec::new())
    } else {
        AllowOrigin::list(allowed)
    };
    CorsLayer::new()
        .allow_origin(origin)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        // The forward client and the browser app both send Authorization;
        // the Fetch spec excludes it from the `*` wildcard, so list the
        // request headers we actually accept explicitly (lesche twin).
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            // The browser channel's CSRF marker (see csrf_guard).
            axum::http::header::HeaderName::from_static(crate::management::CSRF_HEADER),
        ])
        .allow_credentials(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TEST_ADMIN_BEARER, migrated_test_db, raw_test_db};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// The health endpoint answers 200 unauthenticated (compose healthcheck
    /// has no credentials to present), through the real router builder.
    #[tokio::test]
    async fn health_answers_ok_without_auth() {
        let db = raw_test_db().await;
        let app = management_plane_router(
            AppState {
                db,
                public_base_url: "http://127.0.0.1:7501".to_string(),
                management: crate::test_support::test_management(),
                identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
                metrics: crate::metrics::Metrics::private(),
            },
            "",
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible oneshot");

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The metrics exposition answers 200 unauthenticated with the
    /// Prometheus text content type (a scrape presents no credentials),
    /// through the real router builder.
    #[tokio::test]
    async fn metrics_answers_the_prometheus_text_unauthenticated() {
        let db = raw_test_db().await;
        let app = management_plane_router(
            AppState {
                db,
                public_base_url: "http://127.0.0.1:7501".to_string(),
                management: crate::test_support::test_management(),
                identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
                metrics: crate::metrics::Metrics::private(),
            },
            "",
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .expect("a content type");
        assert!(content_type.starts_with("text/plain"));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body");
        let text = String::from_utf8(body.to_vec()).expect("utf-8 exposition");
        assert!(text.contains("kallipai_model_gateway_build_info"));
    }

    /// The two route families stay disjoint: an admin path is a plain
    /// 404 on the data plane and a forwarding path is unknown on the
    /// management plane. The physical separation starts at the route
    /// table -- no path is reachable on the wrong listener, whatever
    /// credentials a request carries.
    #[tokio::test]
    async fn the_planes_share_no_routes() {
        let db = migrated_test_db().await;
        let state = AppState {
            db,
            public_base_url: "http://127.0.0.1:7501".to_string(),
            management: crate::test_support::test_management(),
            identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
            metrics: crate::metrics::Metrics::private(),
        };
        let management = management_plane_router(state, "");

        // The data plane's half is a dispatch pin now: the /admin
        // family (bare prefix included) routes to the Admin
        // short-circuit -- a 404 on the public port, never a proxy-through;
        // the distribution reads are gone from its dispatch table.
        assert!(matches!(
            crate::data_plane::routes::route_of(&Method::GET, "/sets"),
            crate::data_plane::Route::NotFound
        ));
        assert!(matches!(
            crate::data_plane::routes::route_of(&Method::GET, "/admin/keys"),
            crate::data_plane::Route::Admin
        ));
        assert!(matches!(
            crate::data_plane::routes::route_of(&Method::GET, "/admin"),
            crate::data_plane::Route::Admin
        ));

        let response = management
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/sets")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible oneshot");
        // The distribution reads live here: no platform bearer means
        // the extractor's 401, not a 404.
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        // Even the management credential is the wrong family for the
        // reads: they never consult the admin token (family, not port).
        let response = management
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/sets")
                    .header("authorization", format!("Bearer {TEST_ADMIN_BEARER}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible oneshot");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// A wildcard-origin config yields an empty allowlist: no request gets
    /// an allow-origin echo (the "*" is filtered, never passed through as
    /// a literal origin), while a listed origin still echoes -- the same
    /// layer machinery, so the pin cannot pass by stripping everything.
    #[tokio::test]
    async fn cors_wildcard_config_yields_empty_allowlist() {
        let wildcard = axum::Router::new()
            .route("/health", get(health))
            .layer(cors_layer("*"));
        let listed = axum::Router::new()
            .route("/health", get(health))
            .layer(cors_layer("https://good.example"));

        let denied = wildcard
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("origin", "https://evil.example")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible oneshot");
        assert!(
            denied
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );

        let echoed = listed
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("origin", "https://good.example")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible oneshot");
        assert_eq!(
            echoed
                .headers()
                .get("access-control-allow-origin")
                .expect("listed origin must echo"),
            "https://good.example"
        );
    }
}

/// A stateless stand-in route for the CORS-layer pins: the preflight is
/// answered by the shared [`cors_layer`] before any handler runs, so the
/// pin needs the layer, not the stateful router.
#[cfg(test)]
mod cors_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    fn preflight(origin: &str) -> Request<Body> {
        Request::builder()
            .method(Method::OPTIONS)
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .expect("request")
    }

    /// The preflight surface, files/archeion twin shape: an allowed origin
    /// gets the advertised method list back (the route table's full set).
    #[tokio::test]
    async fn preflight_from_an_allowed_origin_advertises_methods() {
        let app = Router::new()
            .route("/health", get(health))
            .layer(cors_layer("https://app.example"));
        let response = app.oneshot(preflight("https://app.example")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let advertised = response
            .headers()
            .get("access-control-allow-methods")
            .expect("allowed preflight advertises the method list")
            .to_str()
            .unwrap();
        let mut advertised: Vec<&str> = advertised.split(',').map(str::trim).collect();
        advertised.sort_unstable();
        assert_eq!(advertised, vec!["DELETE", "GET", "PATCH", "POST", "PUT"]);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .expect("allowed origin echoes")
                .to_str()
                .unwrap(),
            "https://app.example"
        );
    }

    /// A wildcard-origin config yields no ACAO at all (the "*" is filtered,
    /// so nothing echoes) -- the browser rejects the response rather than
    /// the proxy holding an open cross-origin hole.
    #[tokio::test]
    async fn preflight_under_wildcard_config_denies_every_origin() {
        let app = Router::new()
            .route("/health", get(health))
            .layer(cors_layer("*"));
        let response = app
            .oneshot(preflight("https://evil.example"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
    }
}
