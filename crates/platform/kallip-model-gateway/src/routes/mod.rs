//! HTTP surface of the gateway, assembled as two physically separate
//! planes on two listeners of one process:
//!
//! - the data plane: distribution GETs (secret-free registry reads), the
//!   LLM-compatible forwarding POST, and the process health probe -- the
//!   surface LLM clients and tagmas talk to;
//! - the management plane: the /admin family behind its own credential.
//!
//! The split keeps the LLM-client-compatible surface and the house
//! namespace from constraining each other's paths or versions, and lets
//! a deployment keep the admin face off the public interface entirely
//! (the Kong proxy/admin precedent). Role modules are declared one level
//! up; this module assembles the route tables and the shared layers.

use axum::Router;
use axum::http::{HeaderValue, Method};
use axum::routing::{get, post};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::state::AppState;

pub use kallip_common::protocol::ApiError;

/// Build the data-plane router: the distribution face's secret-free
/// registry reads, the LLM-compatible forwarding endpoint, and the
/// process health probe. Served on the public address; the management
/// face shares no route and no listener with it.
pub fn data_plane_router(state: AppState, cors_origins: &str) -> Router {
    Router::new()
        .route(
            "/profiles/{profile_id}",
            get(crate::distribution::get_profile),
        )
        .route("/sets/{name}", get(crate::distribution::get_set))
        .route("/sets", get(crate::distribution::get_sets))
        .route("/parking", get(crate::distribution::get_parking))
        .route("/default", get(crate::distribution::get_default))
        .route(
            "/v1/chat/completions",
            post(crate::forward::chat_completions),
        )
        .route("/health", get(health))
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(
            crate::forward::MAX_BODY_BYTES,
        ))
        .layer(cors_layer(cors_origins))
}

/// Build the management-plane router: the /admin family behind its own
/// credential (AdminToken), served on its own listener. Health is
/// mounted here too: it is a process probe, not a member of either
/// route family, so an orchestration watching either address sees the
/// same liveness answer. No explicit body limit is installed here: the
/// admin payloads are small JSON, and axum's default 2 MiB cap is
/// more than enough (the 8 MiB forwarding cap is data-plane only).
pub fn management_plane_router(state: AppState, cors_origins: &str) -> Router {
    Router::new()
        .nest("/admin", crate::management::routes::router())
        .route("/health", get(health))
        .with_state(state)
        .layer(cors_layer(cors_origins))
}

/// GET /health: no authentication, on purpose -- compose healthcheck, the
/// files service's health route is the precedent.
async fn health() -> &'static str {
    "ok"
}

/// `cors_layer` (credentials-aware, explicit method list, never a wildcard
/// origin) -- the lesche variant, NOT the tagma permissive one. The method
/// list is hand-maintained: `GET` for the distribution surface, `POST`
/// for the forwarding surface, `PUT`/`DELETE` for the management face.
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
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        // The forward client and the browser app both send Authorization;
        // the Fetch spec excludes it from the `*` wildcard, so list the
        // request headers we actually accept explicitly (lesche twin).
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
        ])
        .allow_credentials(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::raw_test_db;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// The health endpoint answers 200 unauthenticated (compose healthcheck
    /// has no credentials to present), through the real router builder.
    #[tokio::test]
    async fn health_answers_ok_without_auth() {
        let db = raw_test_db().await;
        let app = data_plane_router(
            AppState {
                db,
                public_base_url: "http://127.0.0.1:7501".to_string(),
                quota: std::sync::Arc::new(crate::quota::QuotaLedger::new()),
                management: crate::test_support::test_management(),
                key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
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

    /// The two planes share no routes: an admin path is a plain 404 on
    /// the data plane and a data path is a plain 404 on the management
    /// plane. The physical separation starts at the route table -- no
    /// path is reachable on the wrong listener, whatever credentials a
    /// request carries.
    #[tokio::test]
    async fn the_planes_share_no_routes() {
        let db = raw_test_db().await;
        let state = AppState {
            db,
            public_base_url: "http://127.0.0.1:7501".to_string(),
            quota: std::sync::Arc::new(crate::quota::QuotaLedger::new()),
            management: crate::test_support::test_management(),
            key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
        };
        let data = data_plane_router(state.clone(), "");
        let management = management_plane_router(state, "");

        let response = data
            .oneshot(
                Request::builder()
                    .uri("/admin/keys")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible oneshot");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = management
            .oneshot(
                Request::builder()
                    .uri("/sets")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible oneshot");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
        assert_eq!(advertised, vec!["DELETE", "GET", "POST", "PUT"]);
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
