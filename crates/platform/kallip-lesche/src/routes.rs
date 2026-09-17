//! Data-plane route mounting.

mod conversations;
mod direct;
mod events;
mod internal;
mod manage_proxy;
mod room_management;
mod rooms;
mod signal;
mod state;
mod status;
mod tunnel;
mod upstream;

#[cfg(test)]
pub(crate) mod test_support;

use axum::Router;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::state::SharedConvState;

/// Data-plane routes, state-injected (`Router<()>`): `/conversations*`,
/// `/me/events`, and `/tunnel`.
pub fn router(
    state: SharedConvState,
    internal_token_hash: Option<kallip_common::authtoken::TokenHash>,
) -> Router<()> {
    let mut app = Router::new()
        .merge(conversations::router().with_state(state.clone()))
        .merge(rooms::router().with_state(state.clone()))
        .merge(direct::router().with_state(state.clone()))
        .merge(room_management::router().with_state(state.clone()))
        .merge(events::router().with_state(state.clone()))
        .merge(tunnel::router().with_state(state.clone()))
        .merge(manage_proxy::router().with_state(state.clone()))
        .merge(state::router().with_state(state.clone()))
        .merge(upstream::router().with_state(state.clone()));
    // The service-to-service `/internal/*` surface: mounted only when the
    // shared secret is configured (same discipline as the archeion's internal
    // nest; the files service pushes FileDelivered events here).
    if let Some(hash) = internal_token_hash {
        let internal = internal::router(state.clone()).layer(axum::middleware::from_fn_with_state(
            hash,
            crate::middleware::internal_guard,
        ));
        app = app.nest("/internal", internal);
    }
    app
}

/// The complete external route table: (verb, path) pairs the merged
/// router serves, exactly as registered above -- the registration
/// registry's mirror. Adding a route means adding
/// a row here: the shape tests drive every row through the app and
/// the CORS layer derives its preflight set from this table, so a stale
/// /v1 prefix in a registration, or an unadvertised verb fails
/// here instead of in production. `ANY` stands for the manage-proxy
/// pass-through, which forwards every verb. Keep rows path-sorted.
pub(crate) const ROUTE_TABLE: &[(&str, &str)] = &[
    ("POST", "/conversations"),
    ("POST", "/conversations/{id}/envelopes"),
    ("POST", "/conversations/{id}/key-exchange/init"),
    ("POST", "/conversations/{id}/key-exchange/response"),
    ("GET", "/direct-sessions"),
    ("POST", "/direct-sessions"),
    ("GET", "/direct-sessions/{session_id}/messages"),
    ("POST", "/direct-sessions/{session_id}/messages"),
    ("PUT", "/direct-sessions/{session_id}/read-cursor"),
    ("GET", "/me/events"),
    ("GET", "/me/tagmata/{tagma_id}/rooms"),
    ("GET", "/rooms"),
    ("POST", "/rooms"),
    ("GET", "/rooms/invites"),
    ("GET", "/rooms/public"),
    ("GET", "/rooms/{room_id}"),
    ("POST", "/rooms/{room_id}/envelopes"),
    ("POST", "/rooms/{room_id}/invites"),
    ("POST", "/rooms/{room_id}/invites/{invite_id}/accept"),
    ("POST", "/rooms/{room_id}/join"),
    ("DELETE", "/rooms/{room_id}/members/{member_id}"),
    ("GET", "/rooms/{room_id}/messages"),
    ("POST", "/rooms/{room_id}/tagmata"),
    ("PUT", "/rooms/{room_id}/read-cursor"),
    ("GET", "/tagmata/{id}/agents"),
    ("GET", "/tagmata/{id}/budget"),
    ("GET", "/tagmata/{id}/status"),
    ("ANY", "/tagmata/{id}/manage/{*path}"),
    ("GET", "/tagmata/{id}/state"),
    ("GET", "/tagmata/{id}/work-schedule"),
    ("GET", "/tagmata/{tagma_id}/rooms"),
    ("POST", "/tagmata/{tagma_id}/upstream"),
    ("GET", "/tunnel"),
    ("POST", "/tunnel/manage-reply"),
];
/// Build a CORS layer from a comma-separated allowlist. Mirrors the archeion's
/// `cors_layer` (credentials-aware, explicit method list, never a wildcard
/// origin). The tagma has a separate permissive `cors_layer` -- do NOT copy
/// that one; this is the credentials-aware variant the browser app needs.
pub fn cors_layer(origins: &str) -> CorsLayer {
    let allowed: Vec<HeaderValue> = origins
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    let origin = if allowed.is_empty() {
        AllowOrigin::list(Vec::new())
    } else {
        AllowOrigin::list(allowed)
    };
    // Methods must be an explicit list, NOT `Any`: the Fetch spec forbids
    // `Access-Control-Allow-Credentials: true` together with a wildcard
    // (`Allow-Methods: *`), and tower-http panics at layer construction if
    // they're combined. The list is exactly the ROUTE_TABLE-derived set
    // (ANY rows skipped): a route row and its preflight advertisement
    // cannot drift apart.
    let mut methods: Vec<Method> = Vec::new();
    for (verb, _) in ROUTE_TABLE {
        if *verb == "ANY" {
            continue;
        }
        let m = Method::from_bytes(verb.as_bytes()).expect("table verb is concrete");
        if !methods.contains(&m) {
            methods.push(m);
        }
    }
    CorsLayer::new()
        .allow_origin(origin)
        .allow_methods(methods)
        // Allow credentialed (cookie-bearing) cross-origin requests so the web
        // app -- served from a different origin than the lesche -- can send the
        // `kallip_session` cookie with `credentials: "include"`. Safe because
        // every wildcard-forbidden field is concrete: the origin allowlist is
        // `AllowOrigin::list` (never `Any`) and the methods are enumerated
        // above. A misconfigured `KALLIP_LESCHE_CORS_ORIGINS=*` therefore yields
        // an empty allowlist (no cross-origin allowed) rather than an open hole.
        .allow_credentials(true)
        // `Authorization` is excluded from the `*` wildcard by the Fetch spec,
        // so list the request headers we actually send explicitly. The CSRF
        // marker (`X-Requested-With`) is a custom header the browser only sends
        // same-origin / after a passing preflight, so it must be allowed here
        // for the preflight to succeed.
        .allow_headers([
            AUTHORIZATION,
            CONTENT_TYPE,
            ACCEPT,
            HeaderName::from_static("x-requested-with"),
        ])
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::header::{ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN};
    use axum::http::{Method, Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    use super::cors_layer;
    use axum::Router;

    /// Route-shape nails: every ROUTE_TABLE row
    /// must serve at its registered bare shape, and a
    /// wrong verb must answer 405. Status vocabulary: 401 = route reached
    /// (the auth extractor answers unauthenticated requests), 404 = no
    /// route mounted here, 405 = route exists, verb not served. The table
    /// rows with `ANY` are covered by the proxy tests; concrete
    /// verbs go through the positive pass.
    async fn shape_response(app: &axum::Router, verb: &str, path: &str) -> axum::http::StatusCode {
        let request = Request::builder()
            .method(verb)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn every_table_row_serves_at_its_bare_shape() {
        let (state, _control) =
            crate::test_support::make_state(60, std::time::Duration::from_secs(10));
        let app = super::router(state, None);
        for (verb, path) in super::ROUTE_TABLE {
            if *verb == "ANY" {
                continue; // covered by the manage-proxy tests
            }
            let status = shape_response(&app, verb, path).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "route missing or mis-shaped: {verb} {path}",
            );
        }
    }

    /// A mounted path with the wrong verb answers 405 (route exists, verb
    /// not served) -- distinct from 404, so verb/shape regressions are
    /// tellable apart in the assertion message.
    #[tokio::test]
    async fn wrong_verbs_answer_405() {
        let (state, _control) =
            crate::test_support::make_state(60, std::time::Duration::from_secs(10));
        let app = super::router(state, None);
        let wrong = [
            ("POST", "/tagmata/t-a/agents"),
            ("DELETE", "/tagmata/t-a/state"),
            ("PUT", "/tagmata/t-a/state"),
            ("GET", "/tunnel/manage-reply"),
            ("DELETE", "/conversations"),
        ];
        for (verb, path) in wrong {
            let status = shape_response(&app, verb, path).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{verb} {path}");
        }
    }

    /// Two-end reconciliation: the rust table and
    /// the TS clients must assert against one shared fixture, so a
    /// one-sided URL change fails a test on the other side too (the root
    /// cause of the original double-prefix bugs was each end proving
    /// itself in isolation).
    #[test]
    fn route_table_matches_the_shared_fixture() {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/kallip-lesche-client/src/route-shapes.json",
        ));
        let fixture: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let mut fixture_routes: Vec<(String, String)> = fixture["routes"]
            .as_array()
            .expect("fixture has a routes array")
            .iter()
            .map(|r| {
                let pair = r.as_array().expect("route is a [verb, path] pair");
                (
                    pair[0].as_str().unwrap().to_owned(),
                    pair[1].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        fixture_routes.sort();
        let mut table: Vec<(String, String)> = super::ROUTE_TABLE
            .iter()
            .map(|(verb, path)| (verb.to_string(), path.to_string()))
            .collect();
        table.sort();
        assert_eq!(
            table, fixture_routes,
            "ROUTE_TABLE and the shared TS fixture drifted"
        );
    }
    /// Same pin as the archeion's: the advertised preflight set must equal
    /// the derived set exactly, so dropping a method (the omission that
    /// broke the archeion's provider vault) fails here instead of in a live
    /// session. The manage surface's verbs are GET/POST/PUT per the tagma
    /// frame allowlist; the three sources move together: frame allowlist,
    /// ROUTE_TABLE, and the CORS methods (one change re-advertises all).
    #[tokio::test]
    async fn preflight_advertises_exactly_the_route_methods() {
        let app = Router::new()
            .route("/ping", get(|| async { "ok" }))
            .layer(cors_layer("https://app.example"));
        let request = Request::builder()
            .method(Method::OPTIONS)
            .header(ORIGIN, "https://app.example")
            .header(ACCESS_CONTROL_REQUEST_METHOD, "PUT")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let advertised = response
            .headers()
            .get(ACCESS_CONTROL_ALLOW_METHODS)
            .expect("allowed preflight advertises the method list")
            .to_str()
            .unwrap();
        let mut advertised: Vec<&str> = advertised.split(',').map(str::trim).collect();
        advertised.sort_unstable();
        let mut expected: Vec<&str> = Vec::new();
        for (verb, _) in super::ROUTE_TABLE {
            if *verb == "ANY" || expected.contains(verb) {
                continue;
            }
            expected.push(*verb);
        }
        expected.sort_unstable();
        assert_eq!(advertised, expected);
    }

    /// Assembly smoke test: the full router build (every sub-router
    /// merged, the internal nest conditionally mounted) must not panic.
    /// axum 0.8 panics on a same-path merge conflict at construction,
    /// and only the binary's startup path builds the whole router -- the
    /// per-sub-router tests never exercise this: a duplicated merge line once
    /// crashed startup while the test suite stayed green.
    #[test]
    fn full_router_assembly_does_not_panic() {
        let (state, _control) =
            crate::test_support::make_state(60, std::time::Duration::from_secs(10));
        // Both mount states: no internal surface, and with it.
        let _ = super::router(state.clone(), None);
        let _ = super::router(
            state,
            Some(kallip_common::authtoken::TokenHash::of("secret")),
        );
    }
}
