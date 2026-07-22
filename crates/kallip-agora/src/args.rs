use clap::Parser;

/// Default trusted-proxy CIDRs: loopback only, matching the default same-box
/// reverse-proxy deploy. Kept as a const so boot logic can tell "operator left
/// the default" from "operator set this explicitly".
pub const DEFAULT_TRUSTED_PROXIES: &str = "127.0.0.0/8, ::1/128";

/// CLI arguments for `kallip-agora`.
///
/// The agora is intended to sit behind a TLS-terminating reverse proxy; it
/// serves plain HTTP and binds localhost by default.
#[derive(Parser)]
#[command(
    name = "kallip-agora",
    about = "Public-internet relay control plane for kallip agent tagmata"
)]
pub struct Args {
    /// Address to listen on (behind a TLS-terminating reverse proxy).
    #[arg(long, env = "KALLIP_AGORA_ADDR", default_value = "127.0.0.1:7100")]
    pub listen_addr: String,
    /// Postgres URL for the durable control-plane store
    /// (`postgres://user:pass@host/db`). The agora connects (retrying with a
    /// capped backoff) and runs migrations at boot.
    #[arg(long, env = "KALLIP_AGORA_DATABASE_URL")]
    pub database_url: String,
    /// WebAuthn relying-party id: the registrable domain passkeys are bound to
    /// (e.g. `agora.example.com`). CANNOT change without invalidating every
    /// bound passkey. For local dev use `localhost`.
    #[arg(long, env = "KALLIP_AGORA_WEBAUTHN_RP_ID")]
    pub webauthn_rp_id: String,
    /// WebAuthn relying-party origin: the exact origin the web app is served
    /// from (e.g. `https://agora.example.com`). Must have `rp_id` as its
    /// effective domain.
    #[arg(long, env = "KALLIP_AGORA_WEBAUTHN_RP_ORIGIN")]
    pub webauthn_rp_origin: String,
    /// Human-readable relying-party name shown in the browser's WebAuthn prompt
    /// (e.g. on Touch ID / Windows Hello).
    #[arg(long, env = "KALLIP_AGORA_WEBAUTHN_RP_NAME", default_value = "kallip")]
    pub webauthn_rp_name: String,
    /// Whether to allow mismatched/non-standard ports on the WebAuthn origin.
    /// Enable for local HTTP dev (`http://localhost:5173`); leave disabled in
    /// production where the origin is a clean `https://<rp_id>`.
    #[arg(
        long,
        env = "KALLIP_AGORA_WEBAUTHN_ALLOW_ANY_PORT",
        default_value_t = false
    )]
    pub webauthn_allow_any_port: bool,
    /// Session cookie lifetime in seconds (the cookie's Max-Age and the
    /// `sessions.expires_at`).
    #[arg(long, env = "KALLIP_AGORA_SESSION_TTL_SECS", default_value = "2592000")]
    pub session_ttl_secs: u64,
    /// Mark the session cookie `Secure` (recommended; only disable for local
    /// plain-HTTP dev where TLS is terminated elsewhere / absent).
    #[arg(long, env = "KALLIP_AGORA_COOKIE_SECURE", default_value_t = true)]
    pub cookie_secure: bool,
    /// Cookie `Domain` attribute. Set to the parent domain in a per-subdomain
    /// deploy so the session cookie is shared across `agora.<d>` and
    /// `lesche.<d>` (e.g. `kallipai.com` prod, `localhost` dev). Unset =
    /// host-only (single-origin deploy).
    #[arg(long, env = "KALLIP_AGORA_SESSION_COOKIE_DOMAIN")]
    pub cookie_domain: Option<String>,
    /// Default invite-code lifetime in seconds when the admin does not pass one
    /// (minted via `POST /v1/admin/invite-codes`).
    #[arg(
        long,
        env = "KALLIP_AGORA_INVITE_DEFAULT_TTL_SECS",
        default_value = "604800"
    )]
    pub invite_default_ttl_secs: u64,
    /// Capacity of the per-IP token bucket guarding `/v1/auth/*` (max burst).
    #[arg(long, env = "KALLIP_AGORA_AUTH_RATE_CAPACITY", default_value_t = 10)]
    pub auth_rate_capacity: u32,
    /// Refill rate of the per-IP auth rate bucket, in requests per second.
    #[arg(
        long,
        env = "KALLIP_AGORA_AUTH_RATE_REFILL_PER_SEC",
        default_value_t = 1
    )]
    pub auth_rate_refill_per_sec: u32,
    /// Comma-separated CIDRs whose direct connections are trusted to have set
    /// `X-Forwarded-For` (e.g. your reverse proxy). When the connecting peer is
    /// in one of these nets, the rate limiter buckets on the real client IP
    /// taken from XFF; otherwise XFF is ignored and the peer IP is used. The
    /// default trusts loopback, which is correct for the default same-box
    /// reverse-proxy deploy. A remote proxy must be added here explicitly.
    #[arg(
        long,
        env = "KALLIP_AGORA_TRUSTED_PROXIES",
        default_value = "127.0.0.0/8, ::1/128"
    )]
    pub trusted_proxies: String,
    /// Admin token (provisioning authority). Unset = generate a fresh
    /// `sk-admin-...` printed once at startup.
    #[arg(long, env = "KALLIP_AGORA_ADMIN_TOKEN")]
    pub admin_token: Option<String>,
    /// Max HTTP request body size in kilobytes. 0 = axum default (2 MB).
    #[arg(long, env = "KALLIP_AGORA_MAX_BODY_SIZE_KB", default_value = "256")]
    pub max_body_size_kb: usize,
    /// Comma-separated CORS allowed origins (the app's origin(s)). Empty = no
    /// cross-origin allowed. Never use a wildcard on a public-facing deploy.
    #[arg(long, env = "KALLIP_AGORA_CORS_ORIGINS", default_value = "")]
    pub cors_origins: String,
    /// Enrollment-code lifetime in seconds (single-use, short-TTL).
    #[arg(
        long,
        env = "KALLIP_AGORA_ENROLLMENT_CODE_TTL_SECS",
        default_value = "600"
    )]
    pub enrollment_code_ttl_secs: u64,
    /// Shared secret that `kallip-lesche` presents as `Authorization: Bearer`
    /// to the `/internal/*` ControlPlane API. Unset = the `/internal` nest is
    /// not mounted (agora runs standalone, no relay connected). Must equal the
    /// lesche's `KALLIP_LESCHE_AGORA_TOKEN`.
    #[arg(long, env = "KALLIP_AGORA_INTERNAL_TOKEN")]
    pub internal_token: Option<String>,
}
