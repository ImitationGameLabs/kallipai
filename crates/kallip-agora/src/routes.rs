//! HTTP routes. `routes.rs` is the module root; submodules live under `routes/`.

mod admin;
mod auth;
mod conversations;
mod events;
mod herald;
mod me_enrollment_codes;
mod tagmata;

use axum::Router;
use axum::extract::State;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::state::SharedState;

pub fn router(state: SharedState) -> Router<()> {
    // The unauthenticated, crypto-heavy entry surfaces are rate-limited per
    // client IP: the ceremony begins (invite-enumeration / ceremony-spam) and
    // tagma enroll (CPU + DB + token mint). Ceremony finishes are NOT
    // rate-limited: each needs a real, single-use ceremony id issued by a
    // (rate-limited) begin, so they are transitively bounded. The
    // cookie-authenticated `/me` + `/logout` and the bearer-authenticated
    // `GET /tagmata/{id}` are not rate-limited, so users behind a shared IP
    // cannot lock each other out.
    let rate_limit =
        axum::middleware::from_fn_with_state(state.clone(), crate::middleware::auth_rate_limit);
    let ceremony_begin = auth::begin_router().layer(rate_limit.clone());
    let enroll = tagmata::enroll_router().layer(rate_limit);

    let v1 = Router::new()
        .merge(ceremony_begin)
        .merge(auth::finish_router())
        .merge(auth::session_router())
        .merge(me_enrollment_codes::me_enrollment_codes_router())
        .nest("/admin", admin::router())
        .merge(enroll)
        .merge(tagmata::protected_router())
        .merge(conversations::router())
        .merge(events::router())
        .nest("/herald", herald::router())
        // CSRF custom-header guard on the whole v1 surface. It is a no-op for
        // non-cookie requests (machine / bearer), so herald + enroll paths are
        // unaffected; it only gates cookie-bearing mutating requests. No router
        // state needed, so plain `from_fn` suffices.
        .layer(axum::middleware::from_fn(crate::middleware::csrf_guard));
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .nest("/v1", v1)
        .with_state(state)
}

/// Liveness: the process is up.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Readiness: up and not shutting down.
async fn readyz(State(state): State<SharedState>) -> impl IntoResponse {
    if state.shutdown.is_cancelled() {
        (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
    } else {
        (StatusCode::OK, "ready")
    }
}

/// Build a CORS layer from a comma-separated allowlist. An empty configured
/// list denies all cross-origin requests; the operator sets the real allowlist
/// (the app's origin) via `KALLIP_AGORA_CORS_ORIGINS`. Never wildcard the
/// origin on a public deploy.
///
/// Methods and headers stay permissive (`Any`): the origin allowlist is the
/// real gate, and `tower-http`'s `CorsLayer::new()` defaults *everything* to
/// denied, so leaving these unset would make the browser reject every preflight
/// (`Authorization` + `application/json` always trigger one) even when the
/// origin matches.
pub(crate) fn cors_layer(origins: &str) -> CorsLayer {
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
        // they're combined. Listed are exactly the methods the agora routes use.
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        // Allow credentialed (cookie-bearing) cross-origin requests so the web
        // app -- served from a different origin than the agora (e.g. the app at
        // http://localhost:5173 calling the agora at http://localhost:7100 in
        // dev, or app vs. agora hosts in prod) -- can send/receive the
        // `kallip_session` cookie with `credentials: "include"`. Safe because
        // every wildcard-forbidden field is concrete: the origin allowlist is
        // `AllowOrigin::list` (never `Any`) and the methods are enumerated above.
        // A misconfigured `KALLIP_AGORA_CORS_ORIGINS=*` therefore yields an empty
        // allowlist (no cross-origin allowed) rather than an open hole.
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
            // The CSRF marker is a custom header; the browser only sends it
            // same-origin / after a passing preflight, so allow it explicitly.
            HeaderName::from_static("x-requested-with"),
        ])
}
