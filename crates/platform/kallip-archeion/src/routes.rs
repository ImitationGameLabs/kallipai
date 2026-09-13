//! HTTP routes. `routes.rs` is the module root; submodules live under `routes/`.
//!
//! Only the control plane lives here (auth ceremonies, session/me, admin,
//! tagmata) plus the service-to-service `/internal/*` surface that the
//! data-plane relay (`kallip-lesche`, a separate process) consumes. The data
//! plane (conversations, envelopes, KEX, tagma tunnel, app events) lives in
//! `kallip-lesche`.

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
mod user_providers;

use axum::Router;
use axum::extract::State;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use kallip_common::authtoken::TokenHash;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::state::SharedState;

/// Build the public router. `internal_token_hash`, when `Some`, mounts the
/// `/internal/*` ControlPlane surface for the lesche (guarded by that hash);
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
    // plane requests. The data plane (lesche) runs its own CSRF guard.
    let mut v1 = Router::new()
        .merge(ceremony_begin)
        .merge(pair_begin)
        .merge(auth::finish_router())
        .merge(oauth::finish_router())
        .merge(device_pairing::finish_router())
        .merge(session::router())
        .merge(oauth::session_router())
        .merge(device_pairing::session_router())
        .merge(user_providers::session_router())
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

/// Build a CORS layer from a comma-separated allowlist. An empty configured
/// list denies all cross-origin requests; the operator sets the real allowlist
/// (the app's origin) via `KALLIP_ARCHEION_CORS_ORIGINS`. Never wildcard the
/// origin on a public deploy.
///
/// Methods and headers are explicit lists, not `Any` (the Fetch spec and
/// `tower-http` rationale lives at the `allow_methods` call below); the
/// origin allowlist is the real gate, and leaving either unset would
/// default them to denied and reject every preflight.
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
        // they're combined. Listed are exactly the methods the archeion routes use.
        // PUT is the provider-vault replace (`PUT /me/providers/{id}`, the
        // web app's rename/key-rotation write); without it the browser
        // rejects the preflight and the write never lands.
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        // Allow credentialed (cookie-bearing) cross-origin requests so the web
        // app -- served from a different origin than the archeion (e.g. the app at
        // http://localhost:5173 calling the archeion at http://localhost:7100 in
        // dev, or app vs. archeion hosts in prod) -- can send/receive the
        // `kallip_session` cookie with `credentials: "include"`. Safe because
        // every wildcard-forbidden field is concrete: the origin allowlist is
        // `AllowOrigin::list` (never `Any`) and the methods are enumerated above.
        // A misconfigured `KALLIP_ARCHEION_CORS_ORIGINS=*` therefore yields an empty
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
        assert_eq!(advertised, ["DELETE", "GET", "PATCH", "POST", "PUT"]);
    }
}
