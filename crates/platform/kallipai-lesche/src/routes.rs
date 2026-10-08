//! Data-plane route mounting.

use axum::Router;
use axum::http::Method;
use tower_http::cors::CorsLayer;

use crate::state::SharedConvState;

/// Data-plane routes, state-injected (`Router<()>`): the chat and link
/// domains plus the shared events face.
pub fn router(state: SharedConvState) -> Router<()> {
    Router::new()
        .merge(crate::chat::router().with_state(state.clone()))
        .merge(crate::events::router().with_state(state.clone()))
        .merge(crate::link::router().with_state(state))
}

/// The complete external route table: (verb, path) pairs the merged
/// router serves, exactly as registered above -- the registration
/// registry's mirror. Adding a route means adding
/// a row here: the shape tests drive every row through the app and
/// the CORS layer derives its preflight set from this table, so a stale
/// /v1 prefix in a registration, or an unadvertised verb fails
/// here instead of in production. Keep rows path-sorted.
pub(crate) const ROUTE_TABLE: &[(&str, &str)] = &[
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
    ("GET", "/tagmata/{id}/events"),
    ("GET", "/tagmata/{tagma_id}/rooms"),
];
/// Build the credentials-aware CORS layer: the shared builder with the
/// verb list derived from ROUTE_TABLE, so a route row
/// and its preflight advertisement cannot drift apart.
pub fn cors_layer(origins: &str) -> CorsLayer {
    let mut methods: Vec<Method> = Vec::new();
    for (verb, _) in ROUTE_TABLE {
        let m = Method::from_bytes(verb.as_bytes()).expect("table verb is concrete");
        if !methods.contains(&m) {
            methods.push(m);
        }
    }
    kallipai_common::cors::cors_layer(origins, &methods)
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
    /// route mounted here, 405 = route exists, verb not served.
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
        let (state, _control) = crate::test_support::make_state();
        let app = super::router(state);
        for (verb, path) in super::ROUTE_TABLE {
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
        let (state, _control) = crate::test_support::make_state();
        let app = super::router(state);
        let wrong = [
            ("DELETE", "/rooms"),
            ("PUT", "/direct-sessions"),
            ("GET", "/rooms/r-1/envelopes"),
            ("POST", "/me/events"),
            ("DELETE", "/tagmata/t-a/events"),
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
        let fixture = include_str!("route-shapes.json");
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
    /// session. The two sources move together: ROUTE_TABLE and the CORS
    /// methods (one change re-advertises all).
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
            if expected.contains(verb) {
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
        let (state, _control) = crate::test_support::make_state();
        // Single mount state: the internal surface lives on the keryx.
        let _ = super::router(state);
    }
}
