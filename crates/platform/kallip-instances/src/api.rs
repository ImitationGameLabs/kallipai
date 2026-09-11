//! The five management routes. Each is a thin translation: HTTP request →
//! one UDS exchange via `DaemonClient` → the unwrapped payload as plain
//! JSON (the wire's `v`/`kind`/`status` tags stay behind the proxy).

use crate::error::{daemon_err, proxy_err};
use crate::guard::AppState;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use kallip_daemon_common::wire::InstanceInfo;
use serde::Deserialize;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[derive(Debug, Deserialize)]
pub struct SpawnRequest {
    pub slug: String,
    pub workspace: String,
    #[serde(default)]
    pub env: Vec<String>,
    /// Provisioning method from the capability vocabulary; omitted =
    /// the backend default. A value the backend does not support is
    /// rejected before the backend is touched.
    pub method: Option<String>,
    /// Launch identity; omitted = the daemon's implicit-launch
    /// rules (self-launch in place, or a drop to the peer's uid).
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StopRequest {
    pub slug: String,
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub slug: String,
}

#[derive(Debug, Deserialize)]
pub struct HealthQuery {
    pub slug: Option<String>,
}

/// Build a CORS layer from a comma-separated allowlist. Mirrors the
/// archeion/lesche `cors_layer` (credentials-aware, explicit method list,
/// never a wildcard origin). The tagma has a separate permissive variant
/// -- do NOT copy that one; this is the credentials-aware variant the
/// browser app needs.
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
        // Methods must be an explicit list, NOT `Any`: the Fetch spec
        // forbids `Access-Control-Allow-Credentials: true` together with
        // a wildcard (`Allow-Methods: *`), and tower-http panics at
        // layer construction if they're combined. The management API
        // speaks exactly these two verbs.
        .allow_methods([Method::GET, Method::POST])
        // Allow credentialed cross-origin requests so the web app --
        // served from a different origin than this service -- can send
        // its bearer header after a passing preflight. Safe because
        // every wildcard-forbidden field is concrete: the origin
        // allowlist is `AllowOrigin::list` (never `Any`) and the methods
        // are enumerated above. A misconfigured
        // `KALLIP_INSTANCES_CORS_ORIGINS=*` therefore yields an empty
        // allowlist (no cross-origin allowed) rather than an open hole.
        .allow_credentials(true)
        // `Authorization` is excluded from the `*` wildcard by the Fetch
        // spec, so list the request headers we actually send explicitly.
        .allow_headers([
            AUTHORIZATION,
            CONTENT_TYPE,
            ACCEPT,
            HeaderName::from_static("x-requested-with"),
        ])
}

/// The `/api/instances` sub-router. The token guard and CORS layer are
/// applied by the caller (`build_router`), not here, so tests can hit
/// the handlers directly.
pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/spawn", post(spawn))
        .route("/stop", post(stop))
        .route("/start", post(start))
        .route("/list", get(list))
        .route("/health", get(health))
        .route("/capabilities", get(capabilities))
}

async fn spawn(
    State(state): State<AppState>,
    payload: Result<Json<SpawnRequest>, JsonRejection>,
) -> Response {
    let Json(SpawnRequest {
        slug,
        workspace,
        env,
        method,
        user,
    }) = match payload {
        Ok(Json(body)) => Json(body),
        Err(rejection) => return bad_body(rejection),
    };
    // Validate the provisioning method against the backend's advertised
    // set before touching the backend: an unsupported value is a client
    // error, and omitted keeps the backend default (zero change for
    // existing callers).
    if let Some(method) = &method
        && !state.backend.capabilities().iter().any(|m| m == method)
    {
        return crate::error::fault(
            StatusCode::BAD_REQUEST,
            "unsupported_method",
            format!("provisioning method not supported: {method}"),
        );
    }
    // user: the launch identity field, carried verbatim -- the daemon
    // owns the implicit-vs-explicit identity rules.
    let outcome = state.backend.spawn(slug, workspace, env, user).await;
    respond(outcome)
}

async fn stop(
    State(state): State<AppState>,
    payload: Result<Json<StopRequest>, JsonRejection>,
) -> Response {
    let Json(StopRequest { slug }) = match payload {
        Ok(Json(body)) => Json(body),
        Err(rejection) => return bad_body(rejection),
    };
    let outcome = state.backend.stop(slug).await;
    respond(outcome)
}

async fn start(
    State(state): State<AppState>,
    payload: Result<Json<StartRequest>, JsonRejection>,
) -> Response {
    let Json(StartRequest { slug }) = match payload {
        Ok(Json(body)) => Json(body),
        Err(rejection) => return bad_body(rejection),
    };
    let outcome = state.backend.start(slug).await;
    respond(outcome)
}

async fn list(State(state): State<AppState>) -> Response {
    let outcome = state
        .backend
        .list()
        .await
        .map(|instances| InstanceList { instances });
    respond(outcome)
}

async fn capabilities(State(state): State<AppState>) -> Response {
    let supported = state.backend.capabilities();
    Json(Capabilities { methods: supported }).into_response()
}
async fn health(
    State(state): State<AppState>,
    query: Result<Query<HealthQuery>, QueryRejection>,
) -> Response {
    let Query(HealthQuery { slug }) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_body(rejection),
    };
    let outcome = state.backend.health(slug).await;
    respond(outcome)
}

/// Render one backend outcome as the HTTP response: Ok → the plain JSON
/// value; Err → the mapped status + `{code, message}`.
fn respond<T: serde::Serialize>(outcome: Result<T, crate::wire::BackendError>) -> Response {
    match outcome {
        Ok(value) => Json(value).into_response(),
        Err(crate::wire::BackendError::Fault { code, message }) => daemon_err(code, message),
        Err(crate::wire::BackendError::Transport(error)) => proxy_err(error),
    }
}
/// A JSON body that failed to parse becomes 400 `bad_request` (not axum's
/// default 415/422/500 text): the daemon's own grammar for a malformed
/// request, so clients see one error shape.
fn bad_body(rejection: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(crate::error::ApiFault {
            code: "bad_request",
            message: format!("request body rejected: {rejection}"),
        }),
    )
        .into_response()
}

#[derive(Debug, serde::Serialize)]
pub struct Spawned {
    pub slug: String,
    pub pid: u32,
    pub port: u16,
}

#[derive(Debug, serde::Serialize)]
pub struct Stopped {
    pub slug: String,
}

#[derive(Debug, serde::Serialize)]
pub struct Capabilities {
    pub methods: Vec<String>,
}
#[derive(Debug, serde::Serialize)]
pub struct InstanceList {
    pub instances: Vec<InstanceInfo>,
}
