//! Data-plane route mounting.

mod conversations;
mod events;
mod herald;

use axum::Router;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::state::SharedConvState;

/// All six data-plane routes, state-injected (`Router<()>`):
/// `/conversations*`, `/me/events`, and `/herald/tunnel` (under `/herald`).
pub fn router(state: SharedConvState) -> Router<()> {
    Router::new()
        .merge(conversations::router().with_state(state.clone()))
        .merge(events::router().with_state(state.clone()))
        .nest("/herald", herald::router().with_state(state))
}

/// Build a CORS layer from a comma-separated allowlist. Mirrors the agora's
/// `cors_layer` (credentials-aware, explicit method list, never a wildcard
/// origin). The daemon has a separate permissive `cors_layer` -- do NOT copy
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
    CorsLayer::new()
        .allow_origin(origin)
        // Methods must be an explicit list, NOT `Any`: the Fetch spec forbids
        // `Access-Control-Allow-Credentials: true` together with a wildcard
        // (`Allow-Methods: *`), and tower-http panics at layer construction if
        // they're combined.
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
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
