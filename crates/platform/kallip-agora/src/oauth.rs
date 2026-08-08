//! OAuth (GitHub, Google) as a first-class sign-in method, symmetric with
//! WebAuthn passkeys.
//!
//! The agora is the Authorization-Code **confidential client**: it holds the
//! client secrets, performs the code->token exchange server-side, and fetches
//! the user identity. The browser only ever sees the `authorize_url` and posts
//! the returned `code`+`state` back to the SPA callback, which forwards them to
//! the agora's finish handler.
//!
//! [`OAuthProvider`] is the seam: production wires GitHub + Google (built only
//! when their client credentials are configured); tests inject a mock impl so
//! the full begin/finish/login/create/link logic is exercised without any real
//! provider round-trip. WeChat is a future drop-in impl (its ICP-filed-domain
//! requirement blocks local testing today).
//!
//! Security posture (enforced by the handlers in `routes/oauth`, restated here
//! as the contract the impls must honor):
//! - `state` is a random CSRF token; only its SHA-256 is stored, it is
//!   single-use, and it is bound to `provider` so a github state cannot redeem
//!   at the google finish path.
//! - OAuth signup NEVER auto-merges by email (account takeover) and never
//!   auto-inserts into `emails`; the provider email is display-only.
//! - `exchange`/`fetch_identity` use the server-held secret + the PKCE verifier
//!   stored on the ceremony row; the secret is never logged.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

// ---------------------------------------------------------------------------
// provider identity + error
// ---------------------------------------------------------------------------

/// Stable provider discriminator persisted on `external_identities.provider`
/// and carried in URL path segments. One stable lowercase string per provider;
/// never rename (renaming orphans every linked identity).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Github,
    Google,
}

impl ProviderId {
    /// The URL path segment and stored `provider` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Google => "google",
        }
    }

    /// Parse a path segment back into a `ProviderId`. Unknown -> `None` (the
    /// handler maps that to 404 before touching any provider).
    pub fn from_path(s: &str) -> Option<Self> {
        match s {
            "github" => Some(Self::Github),
            "google" => Some(Self::Google),
            _ => None,
        }
    }

    /// Human label for the login/settings UI ("Continue with GitHub").
    pub fn label(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::Google => "Google",
        }
    }
}

/// What a provider reports about the authenticated user.
///
/// `subject` is the STABLE account id at the provider (GitHub numeric `id`,
/// Google `sub`) and is the login resolution key. The provider-reported email
/// is intentionally NOT carried here: it is never used to merge accounts and
/// never inserted into `emails` (account-takeover avoidance); only
/// `display_name` is persisted (display-only).
#[derive(Debug, Clone)]
pub struct ProviderIdentity {
    pub subject: String,
    pub display_name: Option<String>,
}

/// The token set returned by the exchange. `access_token` is a bearer credential
/// for the provider API; it is never persisted, but to keep an inadvertent
/// `tracing::debug!(?tokens)` (or an `unwrap` path) from leaking it to server
/// logs, the `Debug` impl redacts it (mirroring how `OAuthConfig` carries no
/// `Debug`). Provider responses carry more fields (e.g. `token_type`); serde
/// ignores them, and none are read by any impl today.
#[derive(Clone, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .finish()
    }
}

/// An opaque OAuth round-trip failure. Carries detail for the server log only;
/// handlers surface a generic error to the client (OAuth failures must not leak
/// provider specifics or which step failed).
#[derive(Debug)]
pub struct OAuthError(pub(crate) String);

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "oauth error: {}", self.0)
    }
}

impl std::error::Error for OAuthError {}

impl From<reqwest::Error> for OAuthError {
    fn from(e: reqwest::Error) -> Self {
        Self(format!("transport: {e}"))
    }
}

// ---------------------------------------------------------------------------
// config + trait
// ---------------------------------------------------------------------------

/// One provider's credentials + redirect base. The secret is server-side only;
/// it is never serialized or logged.
#[derive(Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    /// The web app origin, e.g. `https://web.kallipai.com`. The canonical
    /// `redirect_uri` is `redirect_base + "/auth/callback"` and is the single
    /// source of truth (the SPA never constructs it).
    pub redirect_base: Url,
}

impl OAuthConfig {
    /// The exact redirect_uri registered with the provider and reused in the
    /// token exchange. Derived via [`Url::join`] so it is canonical regardless
    /// of whether `redirect_base` was supplied with a trailing slash (the
    /// `url` crate's `Display` always serializes an origin WITH a trailing
    /// slash, so a naive `format!("{base}{path}")` would double-slash it and
    /// providers would reject the mismatch).
    fn redirect_uri(&self) -> String {
        self.redirect_base
            .join(OAUTH_CALLBACK_PATH.trim_start_matches('/'))
            .expect("OAUTH_CALLBACK_PATH is a valid relative path")
            .to_string()
    }
}

/// The web SPA callback path. Shared between the redirect_uri the provider sees
/// and the route the SPA mounts (see `gate.ts` + the callback page).
pub const OAUTH_CALLBACK_PATH: &str = "/auth/callback";

/// One OAuth provider. Implementations hold their [`OAuthConfig`] and the
/// provider-specific endpoint URLs.
#[async_trait::async_trait]
pub trait OAuthProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    /// Human label (delegates to [`ProviderId::label`] by default).
    fn label(&self) -> &'static str {
        self.id().label()
    }
    /// Whether this provider supports PKCE (GitHub + Google do; WeChat will
    /// override to `false`).
    fn supports_pkce(&self) -> bool {
        true
    }

    /// Build the authorize URL the SPA navigates to. `state` is the raw CSRF
    /// token (the caller hashes it before storing). `pkce_challenge` is the
    /// S256 challenge derived by the caller when PKCE is supported. The
    /// redirect_uri is derived from the provider's own [`OAuthConfig`] (single
    /// source of truth) so authorize + exchange cannot drift.
    fn authorize_url(&self, state: &str, pkce_challenge: Option<&str>) -> Url;

    /// Exchange the authorization code for tokens (server-side; client secret +
    /// PKCE verifier). The verifier is threaded back from the ceremony row; the
    /// redirect_uri is the provider's own (must byte-match the authorize call).
    async fn exchange(
        &self,
        code: &str,
        pkce_verifier: Option<&str>,
        http: &reqwest::Client,
    ) -> Result<TokenSet, OAuthError>;

    /// Fetch the authenticated user's identity from the provider.
    async fn fetch_identity(
        &self,
        tokens: &TokenSet,
        http: &reqwest::Client,
    ) -> Result<ProviderIdentity, OAuthError>;
}

// ---------------------------------------------------------------------------
// concrete providers (github.rs, google.rs)
// ---------------------------------------------------------------------------

mod github;
mod google;

pub use github::GitHubProvider;
pub use google::GoogleProvider;

// ---------------------------------------------------------------------------
// registry
// ---------------------------------------------------------------------------

/// A UI-facing provider entry (the `/auth/oauth/providers` response item).
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
}

/// The set of enabled providers. Built at boot from config; empty = OAuth
/// disabled. The test seam injects a mock provider via [`ProviderRegistry::new`].
/// Not `Clone`: it owns trait objects; `AppState` holds it once under its `Arc`.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Box<dyn OAuthProvider>>,
}

impl ProviderRegistry {
    pub fn new(providers: Vec<Box<dyn OAuthProvider>>) -> Self {
        Self { providers }
    }

    /// Resolve a provider by id.
    pub fn get(&self, id: ProviderId) -> Option<&dyn OAuthProvider> {
        self.providers
            .iter()
            .find(|p| p.id() == id)
            .map(|p| p.as_ref())
    }

    /// The enabled providers, for the `/auth/oauth/providers` UI discovery
    /// endpoint. Order is registration order (boot config order).
    pub fn list(&self) -> Vec<ProviderInfo> {
        self.providers
            .iter()
            .map(|p| ProviderInfo {
                id: p.id().as_str().to_string(),
                label: p.label().to_string(),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// PKCE + state + return-path sanitization + username synthesis (pure helpers)
// ---------------------------------------------------------------------------

/// A random PKCE code_verifier (43 URL-safe chars from 32 random bytes) and its
/// S256 code_challenge (base64url-no-pad of its SHA-256). The verifier rides
/// the ceremony row; the challenge goes in the authorize URL.
pub(crate) fn generate_pkce() -> Result<(String, String), OAuthError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| OAuthError(format!("pkce rng: {e}")))?;
    let verifier = b64url(&bytes);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

/// A random 32-byte `state` CSRF token, base64url-encoded for the URL.
pub(crate) fn generate_state() -> Result<String, OAuthError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| OAuthError(format!("state rng: {e}")))?;
    Ok(b64url(&bytes))
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Sanitize the SPA-supplied return path (the `?next=` query value) to a
/// relative path only, defeating open-redirect. Returns `None` for anything
/// missing, non-relative, or suspicious -- the caller then defaults to the
/// standard post-login page.
///
/// Rules: must start with a single `/`, must not start with `//` (protocol-
/// relative) or `/\`, must contain no backslash or control chars, capped at a
/// sane length. A leading `/` with none of those tricks is necessarily a
/// same-origin path (no scheme can precede it).
pub(crate) fn sanitize_return_path(next: Option<&str>) -> Option<String> {
    let s = next?.trim();
    if s.is_empty() || s.len() > 512 {
        return None;
    }
    if !s.starts_with('/') || s.starts_with("//") || s.starts_with("/\\") {
        return None;
    }
    if s.contains('\\') || s.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    //! Pure-helper tests: provider id round-trip, authorize_url shape + PKCE,
    //! and the return-path sanitizer (adversarial). Provider HTTP round-trips
    //! are exercised via a mock provider in `routes/oauth` handler tests.

    use super::*;

    #[test]
    fn provider_id_round_trips() {
        for id in [ProviderId::Github, ProviderId::Google] {
            assert_eq!(ProviderId::from_path(id.as_str()), Some(id));
        }
        assert_eq!(ProviderId::from_path("wechat"), None);
        assert_eq!(ProviderId::from_path("bogus"), None);
    }

    #[test]
    fn pkce_verifier_and_challenge_shape() {
        let (verifier, challenge) = generate_pkce().unwrap();
        // 32 bytes -> 43 base64url-no-pad chars.
        assert_eq!(verifier.len(), 43);
        // The challenge is the base64url-no-pad SHA-256 of the verifier.
        let expected = b64url(&Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
        // No padding chars.
        assert!(!verifier.contains('=') && !challenge.contains('='));
    }

    #[test]
    fn state_is_random_unpadded() {
        let a = generate_state().unwrap();
        let b = generate_state().unwrap();
        assert_ne!(a, b);
        // 32 bytes -> 43 base64url-no-pad chars.
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn github_authorize_url_has_pkce_and_scope() {
        let cfg = OAuthConfig {
            client_id: "cid".to_string(),
            client_secret: "sec".to_string(),
            redirect_base: Url::parse("https://web.example.com").unwrap(),
        };
        let url = GitHubProvider::new(cfg).authorize_url("thestate", Some("thechallenge"));
        let q: std::collections::HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(q.get("client_id"), Some(&"cid".to_string()));
        assert_eq!(q.get("state"), Some(&"thestate".to_string()));
        assert_eq!(q.get("code_challenge"), Some(&"thechallenge".to_string()));
        assert_eq!(q.get("code_challenge_method"), Some(&"S256".to_string()));
        assert!(q.get("scope").unwrap().contains("user:email"));
    }

    #[test]
    fn redirect_uri_is_canonical_without_double_slash() {
        // `Url` Display always adds a trailing slash to an origin, so a naive
        // format!("{}{path}", base) would yield `//auth/callback`. The provider
        // would reject the redirect_uri_mismatch. Verify both input spellings.
        for base in ["https://web.example.com", "https://web.example.com/"] {
            let cfg = OAuthConfig {
                client_id: "cid".to_string(),
                client_secret: "sec".to_string(),
                redirect_base: Url::parse(base).unwrap(),
            };
            assert_eq!(
                cfg.redirect_uri(),
                "https://web.example.com/auth/callback",
                "base `{base}` must not double-slash"
            );
        }
    }

    #[test]
    fn sanitize_return_path_accepts_relative_paths() {
        assert_eq!(
            sanitize_return_path(Some("/tagmata")),
            Some("/tagmata".to_string())
        );
        assert_eq!(
            sanitize_return_path(Some("/tagmata?x=1")),
            Some("/tagmata?x=1".to_string())
        );
        assert_eq!(sanitize_return_path(None), None);
        assert_eq!(sanitize_return_path(Some("")), None);
        assert_eq!(sanitize_return_path(Some("   ")), None);
    }

    #[test]
    fn sanitize_return_path_rejects_open_redirects() {
        // Protocol-relative.
        assert_eq!(sanitize_return_path(Some("//evil.com")), None);
        // Absolute scheme URL.
        assert_eq!(sanitize_return_path(Some("https://evil.com")), None);
        // Backslash tricks.
        assert_eq!(sanitize_return_path(Some("/\\evil.com")), None);
        assert_eq!(sanitize_return_path(Some("/foo\\bar")), None);
        // No leading slash.
        assert_eq!(sanitize_return_path(Some("tagmata")), None);
        // Control char.
        assert_eq!(sanitize_return_path(Some("/foo\nbar")), None);
        // Over-long.
        assert_eq!(
            sanitize_return_path(Some(&format!("/{}", "a".repeat(600)))),
            None
        );
    }
}
