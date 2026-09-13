//! Local instance management service for the kallip daemon.
//!
//! Proxies the four management verbs from the bare resource paths to the
//! daemon's UDS socket. The daemon itself never grows an HTTP or token
//! surface; this crate is the only networked door: a Host-header check
//! on everything, plus one of three auth modes for the API —
//! archeion-verified admin access, a configured standalone token, or open
//! on bare loopback.

pub mod api;
pub mod backend;
pub mod config;
pub mod control_plane;
pub mod error;
pub mod guard;
pub mod middleware;
pub mod wire;

use axum::Router;
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};

pub use config::Config;
pub use guard::AppState;

/// Assemble the full router: the API under token auth, a 404 fallback
pub fn build_router(state: AppState) -> Router {
    let api = api::api_routes()
        .layer(from_fn_with_state(state.clone(), guard::token_guard))
        .layer(axum::middleware::from_fn(middleware::csrf_guard))
        .layer(api::cors_layer(&state.cors_origins));
    let app = Router::new().merge(api).fallback(not_found);

    // Layered last so the guard also wraps the fallback: a Router layer
    // only covers routes registered before it, and unknown paths must
    // meet the same Host check as the API.
    app.layer(from_fn_with_state(state.clone(), guard::host_guard))
        .with_state(state)
}

/// Anything off the API is a plain 404.
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(error::ApiFault {
            code: "not_found",
            message: "no such path; the API lives at the root".to_string(),
        }),
    )
        .into_response()
}

/// Resolve the auth mode from the configuration and bind address.
///
/// Platform credentials take precedence when both modes are configured
/// (the platform is the intended shape; a leftover standalone token does
/// not silently downgrade the deployment). Half of the archeion pair alone
/// likewise refuses to start rather than fall through to a weaker mode.
/// Bare loopback with nothing configured is the open mode; anything else
/// refuses to start.
pub fn resolve_auth(config: &Config, addr: &str) -> anyhow::Result<guard::AuthMode> {
    use guard::AuthMode;
    match (
        &config.archeion_internal_url,
        &config.archeion_internal_token,
    ) {
        (Some(url), Some(token)) => {
            return Ok(AuthMode::Platform(std::sync::Arc::new(
                control_plane::ArcheionVerifier::new(url.clone(), token.clone()),
            )));
        }
        // A half-configured pair must refuse to start: falling through
        // would silently serve the API under a weaker mode than intended.
        (Some(_), None) => anyhow::bail!(
            "refusing to start: KALLIP_INSTANCES_ARCHEION_URL is set but \
             KALLIP_POLIS_INTERNAL_TOKEN_FILE is missing"
        ),
        (None, Some(_)) => anyhow::bail!(
            "refusing to start: KALLIP_POLIS_INTERNAL_TOKEN_FILE is set \
             but KALLIP_INSTANCES_ARCHEION_URL is missing"
        ),
        (None, None) => {}
    }
    if let Some(token) = &config.token {
        return Ok(AuthMode::Token(token.clone()));
    }
    if is_loopback_bind(addr)? {
        return Ok(AuthMode::Open);
    }
    anyhow::bail!(
        "refusing to start: {addr} is not loopback and no auth is configured \
         (set KALLIP_INSTANCES_TOKEN, or archeion internal URL + token for platform mode)"
    )
}

/// Whether the bind address names a loopback interface (the unspecified
/// address is broader still — treated as non-loopback, i.e. unsafe).
fn is_loopback_bind(addr: &str) -> anyhow::Result<bool> {
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("bind address must be ip:port, got {addr}"))?;
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("bind address must be ip:port, got {addr}");
    }
    let ip: std::net::IpAddr = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| anyhow::anyhow!("bind address host is not an IP literal: {host}"))?;
    Ok(ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use kallip_daemon_client::DaemonClient;
    use tower::ServiceExt;

    fn test_state(auth: crate::guard::AuthMode) -> AppState {
        AppState {
            // A socket that never exists: API calls resolve to 503
            // daemon_unreachable, proving the backend layer is wired.
            backend: crate::backend::UdsBackend::arc(DaemonClient::new(
                "/nonexistent-kallip-test.sock",
            )),
            auth,
            allowed_hosts: vec![],
            cors_origins: String::new(),
        }
    }

    async fn body_string(response: axum::response::Response) -> String {
        let bytes = response.into_body().collect().await.expect("read body");
        String::from_utf8(bytes.to_bytes().to_vec()).expect("utf8")
    }

    #[tokio::test]
    async fn api_requires_a_token() {
        let app = build_router(test_state(crate::guard::AuthMode::Token(
            "test-token".into(),
        )));
        let response = app
            .oneshot(
                Request::get("/list")
                    .header("host", "127.0.0.1:7300")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_string(response).await;
        assert!(body.contains("\"unauthorized\""), "{body}");
    }

    #[tokio::test]
    async fn token_mode_accepts_the_configured_token() {
        let app = build_router(test_state(crate::guard::AuthMode::Token(
            "test-token".into(),
        )));
        let response = app
            .oneshot(
                Request::get("/list")
                    .header("host", "127.0.0.1:7300")
                    .header("authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The daemon socket never exists, so reaching the proxy layer at
        // all (503, not 401) proves the token was accepted.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn api_rejects_a_wrong_token() {
        let app = build_router(test_state(crate::guard::AuthMode::Token(
            "test-token".into(),
        )));
        let response = app
            .oneshot(
                Request::get("/list")
                    .header("host", "127.0.0.1:7300")
                    .header("authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn open_mode_lets_the_api_through_without_a_token() {
        let app = build_router(test_state(crate::guard::AuthMode::Open));
        let response = app
            .oneshot(
                Request::get("/list")
                    .header("host", "127.0.0.1:7300")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // No credential at all: reaching the proxy layer (503, not 401)
        // proves the open mode waved the request through.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn unreachable_daemon_maps_to_503() {
        let app = build_router(test_state(crate::guard::AuthMode::Token(
            "test-token".into(),
        )));
        let response = app
            .oneshot(
                Request::get("/list")
                    .header("host", "127.0.0.1:7300")
                    .header("authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_string(response).await;
        assert!(body.contains("\"daemon_unreachable\""), "{body}");
    }

    #[tokio::test]
    async fn foreign_host_is_forbidden_even_with_token() {
        let app = build_router(test_state(crate::guard::AuthMode::Token(
            "test-token".into(),
        )));
        let response = app
            .oneshot(
                Request::get("/list")
                    .header("host", "evil.example")
                    .header("authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_string(response).await;
        assert!(body.contains("\"host_forbidden\""), "{body}");
    }

    #[tokio::test]
    async fn malformed_json_body_is_bad_request() {
        let app = build_router(test_state(crate::guard::AuthMode::Token(
            "test-token".into(),
        )));
        let response = app
            .oneshot(
                Request::post("/spawn")
                    .header("host", "127.0.0.1:7300")
                    .header("authorization", "Bearer test-token")
                    .header("content-type", "application/json")
                    .body(Body::from("{not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_string(response).await;
        assert!(body.contains("\"bad_request\""), "{body}");
    }

    #[tokio::test]
    async fn api_only_mode_404s_off_api_paths() {
        let app = build_router(test_state(crate::guard::AuthMode::Open));
        let response = app
            .oneshot(
                Request::get("/")
                    .header("host", "127.0.0.1:7300")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn foreign_host_is_forbidden_off_api_paths() {
        // The fallback sits outside the API routes, and a Router layer
        // only covers what was registered before it: this locks the
        // host guard's reach over the fallback itself.
        let app = build_router(test_state(crate::guard::AuthMode::Open));
        let response = app
            .oneshot(
                Request::get("/")
                    .header("host", "evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
