//! `AppState`: the registry's durable handle + boot config.
//!
//! The registry owns identity / credentials / provisioning (users, passkeys,
//! enrollment tokens, tagmata, tagma tokens, sessions) in the
//! durable Postgres store, plus the `ControlPlane` impl exposed to the
//! data-plane relay (`kallip-lesche`) over the `/internal/*` HTTP API. The
//! data-plane soft state (presence, conversations, app streams, KEX
//! correlation) lives in the lesche, not here.

use std::sync::Arc;
use std::time::Duration;

use crate::db::Db;
use crate::notify::{LoggingTransport, MailTransport};
use crate::ratelimit::{GlobalRateLimiter, IpRateLimiter};
use crate::session::SessionCfg;
use kallip_common::authtoken::TokenHash;
use tokio_util::sync::CancellationToken;
use url::Url;
use webauthn_rs::Webauthn;
use webauthn_rs::prelude::{WebauthnBuilder, WebauthnError};
use webauthn_rs_core::WebauthnCore;

pub type SharedState = Arc<AppState>;

/// Build the high-level `Webauthn` wrapper AND a bare `WebauthnCore` from the
/// SAME relying-party config, so a credential registered through the core
/// (discoverable registration requires `require_resident_key(true)`, which the
/// wrapper hardcodes to false and hides behind a private `core` field) is
/// verifiable by the wrapper at login. Both instances must agree byte-for-byte
/// on rp_id, rp_name, allowed origins, timeout, and the subdomain/any-port
/// flags -- this helper is the single point that keeps them in sync.
pub fn build_webauthn_pair(
    rp_name: &str,
    rp_id: &str,
    rp_origin: &Url,
    allow_any_port: bool,
    allow_subdomains: bool,
) -> Result<(Arc<Webauthn>, Arc<WebauthnCore>), WebauthnError> {
    let webauthn = WebauthnBuilder::new(rp_id, rp_origin)?
        .allow_any_port(allow_any_port)
        .allow_subdomains(allow_subdomains)
        .rp_name(rp_name)
        .timeout(Duration::from_secs(60))
        .build()?;
    // Mirror `WebauthnBuilder::build`: it forwards these six values into
    // `WebauthnCore::new_unsafe_experts_only`, applying `rp_name.unwrap_or(rp_id)`
    // to its OWN Option field -- but we always call `.rp_name(rp_name)` above so
    // the field is `Some`, and we pass the same `rp_name` string to the core, so
    // both instances agree byte-for-byte.
    let core = WebauthnCore::new_unsafe_experts_only(
        rp_name,
        rp_id,
        vec![rp_origin.clone()],
        Duration::from_secs(60),
        Some(allow_subdomains),
        Some(allow_any_port),
    );
    Ok((Arc::new(webauthn), Arc::new(core)))
}

/// The registry's boot configuration. Relay-only knobs (`proof_skew_secs`,
/// `key_exchange_timeout`) live on the relay's `ConversationsState`, not here.
pub struct AppState {
    pub shutdown: CancellationToken,
    pub limits: Limits,
    /// SHA-256 of the admin token; the single provisioning authority.
    pub admin_token_hash: TokenHash,
    /// Durable store handle (sea-orm `DatabaseConnection`, cheap to clone).
    pub db: Db,
    /// Configured WebAuthn relying party (register/login ceremonies, incl.
    /// discoverable login).
    pub webauthn: Arc<Webauthn>,
    /// Bare core used ONLY for discoverable-credential registration
    /// (`require_resident_key(true)`), built from the same RP config as
    /// [`AppState::webauthn`] via [`build_webauthn_pair`].
    pub webauthn_core: Arc<WebauthnCore>,
    /// Session-cookie attrs + TTL.
    pub session_cfg: SessionCfg,
    /// Per-IP token bucket guarding `/v1/auth/*`.
    pub auth_rate_limiter: IpRateLimiter,
    /// Single shared token bucket capping aggregate throughput on the
    /// device-pairing begin endpoint (the real distributed brute-force bound on
    /// the short pairing code; per-IP alone is bypassable by IP diversity).
    pub pair_rate_limiter: GlobalRateLimiter,
    /// CIDRs whose direct connections are trusted to have set
    /// `X-Forwarded-For`. The rate limiter honors XFF only for a peer in one of
    /// these nets (see [`crate::clientip::real_client_ip`]). Empty means XFF is
    /// never trusted.
    pub trusted_proxies: Vec<ipnet::IpNet>,
    /// Outbound email transport (verification links). Defaults to the logging
    /// transport until a real provider is wired.
    pub mail: Arc<dyn MailTransport>,
    /// Shared HTTP client for outbound OAuth round-trips (token exchange +
    /// userinfo). rustls, no cookies, request-bounded timeout.
    pub http: reqwest::Client,
    /// Enabled OAuth providers (empty = OAuth disabled). Built at boot from
    /// config; a provider is present iff its client credentials are set.
    pub oauth_providers: crate::oauth::ProviderRegistry,
    /// Whether open signup (passkey-only or OAuth account creation) is allowed.
    /// The runtime kill switch now that the invite gate is gone. Login and
    /// linking are unaffected.
    pub signup_enabled: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_body_size_bytes: usize,
    /// How long a minted enrollment token remains redeemable.
    pub enrollment_code_ttl: Duration,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        admin_token_hash: TokenHash,
        limits: Limits,
        db: Db,
        webauthn: Arc<Webauthn>,
        webauthn_core: Arc<WebauthnCore>,
        session_cfg: SessionCfg,
        auth_rate_limiter: IpRateLimiter,
        pair_rate_limiter: GlobalRateLimiter,
        trusted_proxies: Vec<ipnet::IpNet>,
        http: reqwest::Client,
        oauth_providers: crate::oauth::ProviderRegistry,
        signup_enabled: bool,
    ) -> Self {
        Self {
            shutdown: CancellationToken::new(),
            limits,
            admin_token_hash,
            db,
            webauthn,
            webauthn_core,
            session_cfg,
            auth_rate_limiter,
            pair_rate_limiter,
            trusted_proxies,
            mail: Arc::new(LoggingTransport) as Arc<dyn MailTransport>,
            http,
            oauth_providers,
            signup_enabled,
        }
    }
}
