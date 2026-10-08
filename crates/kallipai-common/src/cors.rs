//! Shared CORS layer builders for the polis services.
//!
//! Two shapes exist in the fleet. The browser-facing edge services
//! (archeion, lesche, keryx, files, instances, model-gateway) share
//! [`cors_layer`]: credentials-aware, an explicit method list, and an origin
//! allowlist parsed from config -- never a wildcard. The local agent-side
//! services (tagma, cron-daemon) use [`permissive_cors_layer`] instead:
//! nothing browser-facing depends on them as a gate.
//!
//! The builders live here so the origin parse chain and the Fetch-spec
//! constraints are argued once; a call site states only its verb list.

use axum::http::{HeaderValue, Method, header};
use tower_http::cors::{AllowHeaders, AllowOrigin, Any, CorsLayer};

/// Credentials-aware CORS layer for the browser-facing edge services.
///
/// `origins` is the service's comma-separated `*_CORS_ORIGINS` config value.
/// The parse chain trims each entry, drops empties (a trailing comma is
/// normal config style, not a misconfiguration), drops a literal `*` (a
/// wildcarded value degrades to the empty allowlist the docs promise, never
/// an open hole), and drops entries that are not origin-shaped or not
/// parseable as a header value. Every misconfiguration drop warns at boot:
/// a silent drop would turn a typo into a mysteriously rejected preflight.
///
/// `methods` is the explicit verb list the service's routes use. It must be
/// a concrete list, NOT `Any`: the Fetch spec forbids
/// `Access-Control-Allow-Credentials: true` together with a wildcard
/// (`Allow-Methods: *`), and tower-http panics at layer construction when
/// they are combined.
///
/// The header list is the set the web client actually sends: `Authorization`
/// and `Content-Type` plus `Accept` for content negotiation, and the
/// `X-Requested-With` CSRF marker (a custom header the browser only sends
/// same-origin or after a passing preflight, so it must be allowed for the
/// preflight to succeed). `Authorization` is excluded from the `*` wildcard
/// by the Fetch spec, which is why the list exists at all.
pub fn cors_layer(origins: &str, methods: &[Method]) -> CorsLayer {
    let allowed = origin_allowlist(origins);
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed))
        .allow_methods(methods.to_vec())
        .allow_credentials(true)
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::HeaderName::from_static("x-requested-with"),
        ])
}

/// Permissive CORS layer for the local agent-side services (the tagma
/// shape). These services sit on loopback next to their callers and
/// authenticate every request themselves, so CORS is not a security
/// boundary for them. `mirror_request()` reflects whatever the browser
/// requests in preflight, which is the wildcard semantics wanted here:
/// unlike `Any` (the `*` value) it actually covers `Authorization`, which
/// the Fetch spec excludes from `*`.
pub fn permissive_cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(AllowHeaders::mirror_request())
}

/// Parse the comma-separated origins config value into an allowlist.
fn origin_allowlist(origins: &str) -> Vec<HeaderValue> {
    origins
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| {
            if *s == "*" {
                tracing::warn!(
                    entry = *s,
                    "dropping wildcard CORS origin entry; a wildcarded config \
                     degrades to the empty allowlist"
                );
                false
            } else {
                true
            }
        })
        .filter(|s| {
            if !origin_shaped(s) {
                tracing::warn!(
                    entry = *s,
                    "dropping non-origin-shaped CORS entry; an origin is \
                     scheme://host[:port] with no path"
                );
                false
            } else {
                true
            }
        })
        .filter_map(|s| match HeaderValue::from_str(s) {
            Ok(value) => Some(value),
            Err(_) => {
                tracing::warn!(entry = s, "dropping unparseable CORS origin entry");
                None
            }
        })
        .collect()
}

/// True when the entry has the origin shape: an http(s) scheme, a non-empty
/// host, and no path, query, or fragment. HeaderValue parsing alone accepts
/// any visible ASCII, so a misconfigured `localhost:5173` would otherwise
/// sit in the allowlist never matching a browser Origin (which always
/// carries its scheme).
fn origin_shaped(s: &str) -> bool {
    match s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    {
        Some(rest) => !rest.is_empty() && !rest.contains(['/', '?', '#']),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::http::header::{
        ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_REQUEST_METHOD,
        ORIGIN,
    };
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    fn app(origins: &str) -> Router {
        Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .layer(cors_layer(origins, &[Method::GET, Method::DELETE]))
    }

    async fn preflight(app: Router, origin: &str) -> axum::response::Response {
        app.oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .header(ORIGIN, origin)
                .header(ACCESS_CONTROL_REQUEST_METHOD, "DELETE")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn preflight_from_a_listed_origin_echoes_it_and_the_methods() {
        let response = preflight(
            app("https://a.example, https://b.example"),
            "https://b.example",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://b.example"
        );
        let mut advertised: Vec<&str> = response.headers()[ACCESS_CONTROL_ALLOW_METHODS]
            .to_str()
            .unwrap()
            .split(',')
            .map(str::trim)
            .collect();
        advertised.sort_unstable();
        assert_eq!(advertised, ["DELETE", "GET"]);
    }

    #[tokio::test]
    async fn preflight_from_an_unlisted_origin_gets_no_echo() {
        let response = preflight(app("https://a.example"), "https://evil.example").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }

    /// A wildcarded, empty, or whitespace-only config degrades to the empty
    /// allowlist: no origin is echoed, so the browser rejects everything.
    /// `AllowOrigin::list` rejects a literal `*` at construction.
    #[tokio::test]
    async fn wildcard_or_empty_config_yields_the_empty_allowlist() {
        for origins in ["*", "", "  "] {
            let response = preflight(app(origins), "https://app.example").await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response
                    .headers()
                    .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                    .is_none(),
                "origins {origins:?} must not echo any origin"
            );
        }
    }

    /// Misconfigured entries drop with a warn while the valid ones keep
    /// working: a trailing comma is silent, a missing scheme or a parse
    /// failure is surfaced at boot instead of eating the whole allowlist.
    #[tokio::test]
    async fn malformed_entries_drop_and_valid_entries_keep_working() {
        let origins = "https://a.example,, *, localhost:5173, https://b.example/x, ??";
        let allowed = preflight(app(origins), "https://a.example").await;
        assert_eq!(
            allowed.headers()[ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://a.example"
        );
        let dropped = preflight(app(origins), "localhost:5173").await;
        assert!(dropped.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }

    #[tokio::test]
    async fn permissive_layer_echoes_any_origin() {
        let app = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .layer(permissive_cors_layer());
        let response = preflight(app, "https://whatever.example").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_some()
        );
    }
}
