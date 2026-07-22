//! `AppState`: the registry's durable handle + boot config.
//!
//! The registry owns identity / credentials / provisioning (users, passkeys,
//! invite codes, enrollment tokens, tagmata, tagma tokens, sessions) in the
//! durable Postgres store, plus the `ControlPlane` impl exposed to the
//! data-plane relay (`kallip-lesche`) over the `/internal/*` HTTP API. The
//! data-plane soft state (presence, conversations, app streams, KEX
//! correlation) lives in the lesche, not here.

use std::sync::Arc;
use std::time::Duration;

use crate::db::Db;
use crate::ratelimit::IpRateLimiter;
use crate::session::SessionCfg;
use kallip_common::authtoken::TokenHash;
use tokio_util::sync::CancellationToken;
use webauthn_rs::Webauthn;

pub type SharedState = Arc<AppState>;

/// The registry's boot configuration. Relay-only knobs (`proof_skew_secs`,
/// `key_exchange_timeout`) live on the relay's `ConversationsState`, not here.
pub struct AppState {
    pub shutdown: CancellationToken,
    pub limits: Limits,
    /// SHA-256 of the admin token; the single provisioning authority.
    pub admin_token_hash: TokenHash,
    /// Durable store handle (sea-orm `DatabaseConnection`, cheap to clone).
    pub db: Db,
    /// Configured WebAuthn relying party (register/login ceremonies).
    pub webauthn: Arc<Webauthn>,
    /// Session-cookie attrs + TTL.
    pub session_cfg: SessionCfg,
    /// Per-IP token bucket guarding `/v1/auth/*`.
    pub auth_rate_limiter: IpRateLimiter,
    /// CIDRs whose direct connections are trusted to have set
    /// `X-Forwarded-For`. The rate limiter honors XFF only for a peer in one of
    /// these nets (see [`crate::clientip::real_client_ip`]). Empty means XFF is
    /// never trusted.
    pub trusted_proxies: Vec<ipnet::IpNet>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_body_size_bytes: usize,
    /// How long a minted enrollment token remains redeemable.
    pub enrollment_code_ttl: Duration,
    /// Default lifetime for an admin-minted invite code when none is given.
    pub invite_default_ttl_secs: u64,
}

impl AppState {
    pub fn new(
        admin_token_hash: TokenHash,
        limits: Limits,
        db: Db,
        webauthn: Arc<Webauthn>,
        session_cfg: SessionCfg,
        auth_rate_limiter: IpRateLimiter,
        trusted_proxies: Vec<ipnet::IpNet>,
    ) -> Self {
        Self {
            shutdown: CancellationToken::new(),
            limits,
            admin_token_hash,
            db,
            webauthn,
            session_cfg,
            auth_rate_limiter,
            trusted_proxies,
        }
    }
}
