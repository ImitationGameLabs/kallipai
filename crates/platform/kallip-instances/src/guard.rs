//! Request guards: API authentication (three modes) and the Host check.
//!
//! Auth follows the deployment modes (operator decision): the platform mode
//! is dual-channel, mirroring the lesche — a bearer token verifies with the
//! archeion (only Admin may pass) and, absent a bearer, the archeion session
//! cookie authenticates the fixed local-admin account (instance control is
//! an operator surface, so any other user session is 403); the standalone
//! mode compares a locally configured token; bare loopback with nothing
//! configured trusts the local process, mirroring the daemon's own
//! "the socket's file mode is the auth" stance.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use kallip_archeion_common::principal::Principal;
use subtle::ConstantTimeEq;

use crate::error::fault;

/// How instances API requests are authenticated.
#[derive(Clone)]
pub enum AuthMode {
    /// Platform mode: every request's bearer is verified with the archeion;
    /// only `Principal::Admin` passes. Without a bearer, the archeion session
    /// cookie is verified instead and passes only for the local-admin
    /// account's session (a valid non-admin identity — Tagma, User, or a
    /// plain user session — still gets 403; instance life cycles are the
    /// operator's surface).
    Platform(Arc<dyn crate::control_plane::AuthVerifier>),
    /// Standalone mode: compare against a locally configured token.
    Token(String),
    /// Bare loopback, nothing configured: trust the local process.
    Open,
}

impl std::fmt::Debug for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The verifier is a trait object; name the mode, never a credential.
        match self {
            AuthMode::Platform(_) => f.write_str("Platform(..)"),
            AuthMode::Token(_) => f.write_str("Token(..)"),
            AuthMode::Open => f.write_str("Open"),
        }
    }
}

/// Shared handler state.
#[derive(Clone)]
pub struct AppState {
    pub backend: std::sync::Arc<dyn crate::backend::InstanceBackend>,
    pub auth: AuthMode,
    /// Extra Host values allowed through the host guard (platform mode
    /// fronts this proxy with a reverse proxy, so the platform's domain
    /// must be nameable).
    pub allowed_hosts: Vec<String>,
    /// Comma-separated CORS allowed origins (empty = no cross-origin).
    pub cors_origins: String,
}

/// Credential guard for the instances resource paths. Static assets sit outside this
/// layer: the page loads first, then its API calls carry the credential.
pub async fn token_guard(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let presented = kallip_common::auth_header::extract_bearer_token(&headers);
    match &state.auth {
        AuthMode::Open => next.run(request).await,
        AuthMode::Token(expected) => {
            // Both "missing" and "malformed" mean no usable credential;
            // standalone mode has no cookie channel.
            let Ok(presented) = presented else {
                return fault(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "a bearer token is required (Authorization: Bearer <token>)",
                );
            };
            // Constant-time compare, length-checked first: ct_eq only
            // guarantees timing safety for equal lengths, and the length
            // itself is not secret (the token format is fixed).
            let expected = expected.as_bytes();
            let presented = presented.as_bytes();
            let same = expected.len() == presented.len() && expected.ct_eq(presented).into();
            if same {
                next.run(request).await
            } else {
                fault(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "invalid bearer token",
                )
            }
        }
        AuthMode::Platform(control) => match presented {
            Ok(token) => match control.verify_bearer(token).await {
                Ok(Some(Principal::Admin)) => next.run(request).await,
                // A valid non-admin identity: authenticated but not allowed.
                Ok(Some(_)) => fault(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    "instance management is restricted to platform administrators",
                ),
                // Unknown/invalid token.
                Ok(None) => fault(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "invalid bearer token",
                ),
                // Archeion unreachable or off-contract: fail closed.
                Err(error) => {
                    tracing::warn!(%error, "archeion verify_bearer failed");
                    fault(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "auth_backend_unavailable",
                        "the auth backend could not be reached; access is denied",
                    )
                }
            },
            // No bearer: the browser channel. A session cookie belonging to
            // the fixed local-admin account carries instance rights (the
            // admin login IS the instances credential); any other user
            // session stays 403, an absent one 401.
            Err(_) => {
                let Some(cookie) = crate::middleware::read_session_cookie(&headers) else {
                    return fault(
                        StatusCode::UNAUTHORIZED,
                        "unauthorized",
                        "a bearer token or administrator session is required",
                    );
                };
                match control.verify_session(&cookie).await {
                    Ok(Some(session)) if session.local_admin => next.run(request).await,
                    Ok(Some(_)) => fault(
                        StatusCode::FORBIDDEN,
                        "forbidden",
                        "instance management is restricted to platform administrators",
                    ),
                    // Absent / expired / disabled session.
                    Ok(None) => fault(
                        StatusCode::UNAUTHORIZED,
                        "admin_session_required",
                        "sign in as the platform administrator to manage instances",
                    ),
                    // Archeion unreachable or off-contract: fail closed.
                    Err(error) => {
                        tracing::warn!(%error, "archeion verify_session failed");
                        fault(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "auth_backend_unavailable",
                            "the auth backend could not be reached; access is denied",
                        )
                    }
                }
            }
        },
    }
}

/// Host-header guard, applied to the whole router (API and static). A Host
/// that is neither an IP literal, localhost, nor explicitly configured gets
/// 403 — DNS-rebinding attacks necessarily carry an attacker hostname.
pub async fn host_guard(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    let ok = host.is_some_and(|h| host_is_allowed(h, &state.allowed_hosts));
    if !ok {
        return fault(
            StatusCode::FORBIDDEN,
            "host_forbidden",
            "the Host header must be an IP address, localhost, or configured",
        );
    }
    next.run(request).await
}

/// One Host value (possibly `host:port` or `[v6]:port`) is allowed when it
/// names an IP literal, localhost, or an allowlisted name (case-blind).
fn host_is_allowed(host: &str, allowed: &[String]) -> bool {
    // Split off one port; bare IPv6 literals keep their colons (the port
    // side must be all digits and non-empty, which a hex IPv6 tail is not).
    let authority = match host.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    };
    if allowed
        .iter()
        .any(|a| a.eq_ignore_ascii_case(host) || a.eq_ignore_ascii_case(authority))
    {
        return true;
    }
    if authority.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let authority = authority.trim_start_matches('[').trim_end_matches(']');
    authority.parse::<std::net::IpAddr>().is_ok()
        // A bare unbracketed IPv6 ("::1") mis-splits above (the tail is
        // digits); parsing the original value covers that rare form.
        || host.parse::<std::net::IpAddr>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_ip_literals_localhost_and_allowlist() {
        let allowed = vec!["platform.internal".to_string()];
        for good in [
            "127.0.0.1",
            "127.0.0.1:7300",
            "192.168.1.4:7300",
            "[::1]:7300",
            "[2001:db8::1]",
            "::1",
            "localhost",
            "LOCALHOST:7300",
            "platform.internal",
            "PLATFORM.INTERNAL:7300",
        ] {
            assert!(host_is_allowed(good, &allowed), "{good} should be allowed");
        }
    }

    #[test]
    fn rejects_domains_and_garbage() {
        for bad in [
            "evil.example",
            "evil.example:80",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "platform.internal.evil.example",
            "",
            "not a host",
        ] {
            assert!(!host_is_allowed(bad, &[]), "{bad} should be rejected");
        }
    }
}
