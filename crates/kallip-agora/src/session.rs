//! Session-cookie + CSRF constants and cookie read/write helpers.
//!
//! The session is an opaque `sk-sess-...` token whose SHA-256 hash is the
//! `sessions` primary key; the plaintext rides the `kallip_session` cookie.
//! Cookie attrs: `HttpOnly`, `Secure` (configurable), `SameSite=Strict`,
//! `Path=/`, `Max-Age=SESSION_TTL`. CSRF is two-pillar: `SameSite=Strict`
//! blocks the cookie on cross-site requests, and a custom `X-Requested-With:
//! kallip` header (checked by [`crate::middleware::csrf_guard`]) on every
//! cookie-bearing state-changing request — browsers forbid custom headers on
//! cross-origin fetches without preflight.

use std::time::Duration;

use axum::http::HeaderMap;

/// Session cookie name.
pub const SESSION_COOKIE_NAME: &str = "kallip_session";

/// The custom-header CSRF marker name. Lowercase: HTTP headers are
/// case-insensitive, and axum canonicalises on read.
pub const CSRF_HEADER: &str = "x-requested-with";

/// The custom-header CSRF marker value.
pub const CSRF_HEADER_VALUE: &str = "kallip";

/// Cookie-attribute + `sessions.expires_at` configuration, captured at boot.
/// Not `Copy`: `cookie_domain` is an `Option<String>` and `String` is not
/// `Copy`. The builders take it by reference, so `Clone` is paid only at the
/// few call sites that need an owned copy.
#[derive(Clone, Debug)]
pub struct SessionCfg {
    /// Cookie `Max-Age` and the `sessions.expires_at` deadline.
    pub ttl: Duration,
    /// Whether to emit `Secure` on the cookie. False only for local HTTP dev.
    pub cookie_secure: bool,
    /// Cookie `Domain` attribute. Set to the parent domain in a per-subdomain
    /// deploy so the session cookie is shared across `agora.<d>` and
    /// `lesche.<d>` (e.g. `kallipai.com` prod, `localhost` dev). `None` = no
    /// `Domain` attribute, i.e. a host-only cookie (single-origin deploy). The
    /// clear path MUST apply the same value or logout leaves the cookie behind.
    pub cookie_domain: Option<String>,
}

/// Read the session cookie value from a request's `Cookie` header, if present.
/// Multiple `Cookie` headers and multiple `name=value` pairs within one are both
/// tolerated (`cookie::Cookie::split_parse` handles both forms).
pub fn read_session_cookie(headers: &HeaderMap) -> Option<String> {
    // Take the FIRST matching cookie. A request with duplicate `kallip_session`
    // pairs is malformed (browsers don't emit that); first-wins is defensive and
    // deterministic rather than letting a trailing value shadow a leading one.
    for header in headers.get_all(axum::http::header::COOKIE) {
        let Ok(s) = header.to_str() else {
            continue;
        };
        for cookie in cookie::Cookie::split_parse(s) {
            let Ok(cookie) = cookie else { continue };
            if cookie.name() == SESSION_COOKIE_NAME {
                return Some(cookie.value().to_string());
            }
        }
    }
    None
}

/// Build a `Set-Cookie` header value that persists `plaintext` for `ttl`.
pub fn build_set_cookie(session_cfg: &SessionCfg, plaintext: &str) -> String {
    let mut cookie = cookie::Cookie::build((SESSION_COOKIE_NAME, plaintext))
        .http_only(true)
        .same_site(cookie::SameSite::Strict)
        .path("/")
        .max_age(cookie::time::Duration::seconds(
            session_cfg.ttl.as_secs() as i64
        ));
    if session_cfg.cookie_secure {
        cookie = cookie.secure(true);
    }
    if let Some(domain) = session_cfg.cookie_domain.as_deref() {
        cookie = cookie.domain(domain);
    }
    cookie.build().to_string()
}

/// Build a `Set-Cookie` header value that expires the session cookie
/// immediately (used at logout). Same attrs as the setter so it overwrites it
/// — including `Domain`, or the browser matches the clear against the host-only
/// variant and the parent-domain cookie survives logout.
pub fn build_clear_cookie(session_cfg: &SessionCfg) -> String {
    let mut cookie = cookie::Cookie::build((SESSION_COOKIE_NAME, ""))
        .http_only(true)
        .same_site(cookie::SameSite::Strict)
        .path("/")
        .max_age(cookie::time::Duration::seconds(0));
    if session_cfg.cookie_secure {
        cookie = cookie.secure(true);
    }
    if let Some(domain) = session_cfg.cookie_domain.as_deref() {
        cookie = cookie.domain(domain);
    }
    cookie.build().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(domain: Option<&str>) -> SessionCfg {
        SessionCfg {
            ttl: Duration::from_secs(3600),
            cookie_secure: true,
            cookie_domain: domain.map(str::to_string),
        }
    }

    /// With a `Domain` configured, both the set and clear headers carry it, so
    /// the cookie is shared across `agora.<d>` / `lesche.<d>` and logout deletes
    /// the same (parent-domain) cookie. Asserted structurally via `Cookie::parse`
    /// (not a substring match) so an attribute emitted in the wrong position or
    /// duplicated still fails.
    #[test]
    fn domain_is_emitted_on_set_and_clear_when_configured() {
        let set = cookie::Cookie::parse(build_set_cookie(&cfg(Some("localhost")), "secret"))
            .expect("set header parses");
        assert_eq!(set.domain(), Some("localhost"));
        let clear = cookie::Cookie::parse(build_clear_cookie(&cfg(Some("localhost"))))
            .expect("clear header parses");
        assert_eq!(
            clear.domain(),
            Some("localhost"),
            "clear must carry the same Domain or logout leaves the cookie"
        );
    }

    /// Unset `Domain` = host-only cookie (the single-origin default). The parsed
    /// cookie's `domain()` is `None`.
    #[test]
    fn domain_is_absent_when_unset() {
        let set = cookie::Cookie::parse(build_set_cookie(&cfg(None), "secret"))
            .expect("set header parses");
        assert!(
            set.domain().is_none(),
            "host-only set must not carry Domain"
        );
        let clear =
            cookie::Cookie::parse(build_clear_cookie(&cfg(None))).expect("clear header parses");
        assert!(
            clear.domain().is_none(),
            "host-only clear must not carry Domain"
        );
    }
}
