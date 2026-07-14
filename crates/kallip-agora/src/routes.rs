//! HTTP routes. `routes.rs` is the module root; submodules live under `routes/`.

mod admin;
mod conversations;
mod events;
mod herald;
mod teams;

use axum::Router;
use axum::extract::State;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
    let v1 = Router::new()
        .nest("/admin", admin::router())
        .merge(teams::router())
        .merge(conversations::router())
        .merge(events::router())
        .nest("/herald", herald::router());
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .nest("/v1", v1)
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
        .allow_methods(Any)
        // `Authorization` is excluded from the `*` wildcard by the Fetch spec,
        // so list the request headers we actually send explicitly.
        .allow_headers([AUTHORIZATION, CONTENT_TYPE, ACCEPT])
}
