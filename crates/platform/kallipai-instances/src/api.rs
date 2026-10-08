//! The five management routes. Each is a thin translation: HTTP request →
//! one UDS exchange via `DaemonClient` → the unwrapped payload as plain
//! JSON (the wire's `v`/`kind`/`status` tags stay behind the proxy).

use crate::error::{daemon_err, proxy_err};
use crate::guard::AppState;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use kallipai_daemon_common::wire::InstanceInfo;
use serde::Deserialize;
use tower_http::cors::CorsLayer;

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

/// Build the credentials-aware CORS layer: the shared builder with the
/// verbs the management API speaks.
pub fn cors_layer(origins: &str) -> CorsLayer {
    kallipai_common::cors::cors_layer(origins, &[Method::GET, Method::POST])
}

/// The instances resource router. The token guard and CORS layer are
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

#[cfg(test)]
mod tests {
    use super::cors_layer;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::header::{ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN};
    use tower::ServiceExt;

    /// The verb list is this service's local call into the shared
    /// builder, invisible to the shared suite: pin the advertised
    /// set so adding or removing a verb must update this test
    /// consciously.
    #[tokio::test]
    async fn preflight_advertises_exactly_the_route_methods() {
        let app = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .layer(cors_layer("https://app.example"));
        let request = Request::builder()
            .method(axum::http::Method::OPTIONS)
            .header(ORIGIN, "https://app.example")
            .header(ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let advertised = response
            .headers()
            .get(ACCESS_CONTROL_ALLOW_METHODS)
            .expect("allowed preflight advertises the method list")
            .to_str()
            .unwrap();
        let mut advertised: Vec<&str> = advertised.split(',').map(str::trim).collect();
        advertised.sort_unstable();
        assert_eq!(advertised, ["GET", "POST"]);
    }
}
