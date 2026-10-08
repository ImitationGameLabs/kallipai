//! HTTP routes. `routes.rs` is the module root; submodules live under `routes/`.
//!
//! Only the control plane lives here (auth ceremonies, session/me, admin,
//! tagmata) plus the service-to-service `/internal/*` surface that the
//! data-plane services (separate processes) consume. The data plane lives
//! outside this crate: room envelopes, invites, and direct sessions in the
//! lesche; conversations, KEX, the tagma tunnel, and the agent app stream in
//! the keryx.

mod admin;
mod auth;
mod device_pairing;
mod emails;
mod internal;
mod oauth;
mod passkeys;
mod public_profiles;
mod session;
mod tagmata;

use axum::Router;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use kallipai_common::authtoken::TokenHash;
use tower_http::cors::CorsLayer;

use crate::state::SharedState;

/// Build the public router. `internal_token_hash`, when `Some`, mounts the
/// `/internal/*` ControlPlane surface for the data-plane services (guarded by that hash);
/// `None` runs the archeion standalone with no internal surface.
pub fn router(
    state: SharedState,
    internal_token_hash: Option<TokenHash>,
    admin_login: bool,
) -> Router<()> {
    // The unauthenticated, crypto-heavy entry surfaces are rate-limited per
    // client IP: the ceremony begins (ceremony-spam / username-enumeration) and
    // tagma enroll (CPU + DB + token mint). Ceremony finishes are NOT
    // rate-limited: each needs a real, single-use ceremony id issued by a
    // (rate-limited) begin, so they are transitively bounded. The
    // cookie-authenticated `/me` + `/logout` and the bearer-authenticated
    // `GET /tagmata/{id}` are not rate-limited, so users behind a shared IP
    // cannot lock each other out.
    let rate_limit =
        axum::middleware::from_fn_with_state(state.clone(), crate::middleware::auth_rate_limit);
    // The device-pairing begin is the brute-force target on a short code, so it
    // gets BOTH the per-IP limiter and a single shared global bucket (the real
    // distributed bound — per-IP is bypassable by source-IP diversity).
    let pair_rate_limit =
        axum::middleware::from_fn_with_state(state.clone(), crate::middleware::pair_rate_limit);
    let ceremony_begin = auth::begin_router()
        .merge(oauth::begin_router())
        .layer(rate_limit.clone());
    let pair_begin = device_pairing::begin_router()
        .layer(rate_limit.clone())
        .layer(pair_rate_limit);
    let enroll = tagmata::enroll_router().layer(rate_limit.clone());
    // The unauthenticated read surfaces share the per-IP limiter:
    // `GET /users/{username}` is otherwise a free username-enumeration
    // sweep, the signup availability probe
    // (`GET /auth/username-availability`) is an even more explicit one, and
    // the OAuth provider discovery endpoint (`GET /auth/oauth/providers`)
    // is unauthenticated too.
    let public_profiles = public_profiles::public_router()
        .merge(oauth::public_router())
        .merge(auth::availability_router())
        .layer(rate_limit.clone());
    // The email surfaces that can drive unbounded work are per-IP rate-limited:
    // `POST /me/emails` triggers an outbound verification mail (an amplifier
    // once SMTP is wired), and `POST /me/emails/verify` is unauthenticated
    // (click-from-inbox). The 256-bit single-use token is the real verify gate;
    // the limiter is defense-in-depth + outbound bound.
    let email_write = emails::write_router().layer(rate_limit.clone());
    let email_verify = emails::verify_router().layer(rate_limit.clone());

    // The control-plane v1 carries `SharedState`; resolve it to a stateless
    // `Router<()>`. The CSRF custom-header guard scopes to v1 (a no-op for
    // non-cookie / bearer requests); it gates cookie-bearing mutating control-
    // plane requests. The data planes (lesche/keryx) run their own CSRF guard.
    let mut v1 = Router::new()
        .merge(ceremony_begin)
        .merge(pair_begin)
        .merge(auth::finish_router())
        .merge(oauth::finish_router())
        .merge(device_pairing::finish_router())
        .merge(session::router())
        .merge(oauth::session_router())
        .merge(device_pairing::session_router())
        .merge(email_write)
        .merge(email_verify)
        .nest("/admin", admin::router())
        .merge(enroll)
        .merge(tagmata::protected_router())
        .merge(public_profiles);
    // The local-platform admin-login mounts only when the boot flag is set
    // (production default: the route does not exist). It is an
    // unauthenticated credential-check surface, so it shares the per-IP
    // limiter with the ceremony begins.
    if admin_login {
        v1 = v1.merge(auth::admin_login_router().layer(rate_limit));
    }
    let v1 = v1
        .with_state(state.clone())
        .layer(axum::middleware::from_fn(crate::middleware::csrf_guard));

    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state.clone())
        .merge(v1);

    // The service-to-service `/internal/*` surface: mounted only when the
    // shared secret is configured. The `internal_guard` middleware (layer state
    // = the expected hash) rejects any request whose bearer hash does not match
    // before it reaches the handlers; the handlers take `State<SharedState>` for
    // the DB.
    if let Some(hash) = internal_token_hash {
        let internal = internal::router()
            .layer(axum::middleware::from_fn_with_state(
                hash,
                crate::middleware::internal_guard,
            ))
            .with_state(state);
        app = app.nest("/internal", internal);
    }
    app
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

/// Build the credentials-aware CORS layer: the shared builder carrying
/// the verbs the archeion routes use. The origin allowlist comes from
/// `KALLIPAI_ARCHEION_CORS_ORIGINS`, parsed and validated in the shared
/// builder.
pub(crate) fn cors_layer(origins: &str) -> CorsLayer {
    kallipai_common::cors::cors_layer(
        origins,
        &[Method::GET, Method::POST, Method::PATCH, Method::DELETE],
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::header::{ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN};
    use tower::ServiceExt;

    use super::*;

    /// Pin the preflight method table. A missing method is invisible to the
    /// compiler -- the route works, the browser just rejects its preflight --
    /// so assert the exact advertised set: adding or removing a method must
    /// update this test consciously.
    #[tokio::test]
    async fn preflight_advertises_exactly_the_route_methods() {
        let app = Router::new()
            .route("/healthz", get(healthz))
            .layer(cors_layer("https://app.example"));
        let request = Request::builder()
            .method(Method::OPTIONS)
            .header(ORIGIN, "https://app.example")
            .header(ACCESS_CONTROL_REQUEST_METHOD, "DELETE")
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
        assert_eq!(advertised, ["DELETE", "GET", "PATCH", "POST"]);
    }
}
