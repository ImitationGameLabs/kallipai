use clap::Parser;
use std::path::PathBuf;

/// Default trusted-proxy CIDRs: loopback only, matching the default same-box
/// reverse-proxy deploy. Kept as a const so boot logic can tell "operator left
/// the default" from "operator set this explicitly".
pub const DEFAULT_TRUSTED_PROXIES: &str = "127.0.0.0/8, ::1/128";

/// CLI arguments for `kallip-archeion`.
///
/// The archeion is intended to sit behind a TLS-terminating reverse proxy; it
/// serves plain HTTP and binds localhost by default.
#[derive(Parser)]
#[command(
    name = "kallip-archeion",
    version,
    about = "Public-internet relay control plane for kallip agent tagmata"
)]
pub struct Args {
    /// Address to listen on (behind a TLS-terminating reverse proxy).
    #[arg(long, env = "KALLIP_ARCHEION_ADDR", default_value = "127.0.0.1:7100")]
    pub listen_addr: String,
    /// Postgres URL for the durable control-plane store
    /// (`postgres://user:pass@host/db`). The archeion connects (retrying with a
    /// capped backoff) and runs migrations at boot.
    #[arg(long, env = "KALLIP_ARCHEION_DATABASE_URL")]
    pub database_url: String,
    /// WebAuthn relying-party id: the registrable domain passkeys are bound to
    /// (e.g. `archeion.example.com`). CANNOT change without invalidating every
    /// bound passkey. Defaults to the prod domain; for local passkey dev use
    /// `localhost` explicitly.
    #[arg(
        long,
        env = "KALLIP_ARCHEION_WEBAUTHN_RP_ID",
        default_value = "kallipai.com"
    )]
    pub webauthn_rp_id: String,
    /// WebAuthn relying-party origin: the exact origin the web app is served
    /// from (e.g. `https://archeion.example.com`). Must have `rp_id` as its
    /// effective domain. Defaults to the prod web origin.
    #[arg(
        long,
        env = "KALLIP_ARCHEION_WEBAUTHN_RP_ORIGIN",
        default_value = "https://web.kallipai.com"
    )]
    pub webauthn_rp_origin: String,
    /// Human-readable relying-party name shown in the browser's WebAuthn prompt
    /// (e.g. on Touch ID / Windows Hello).
    #[arg(
        long,
        env = "KALLIP_ARCHEION_WEBAUTHN_RP_NAME",
        default_value = "kallipai"
    )]
    pub webauthn_rp_name: String,
    /// Whether to allow mismatched/non-standard ports on the WebAuthn origin.
    /// Enable for local HTTP dev (`http://localhost:5173`); leave disabled in
    /// production where the origin is a clean `https://<rp_id>`.
    #[arg(
        long,
        env = "KALLIP_ARCHEION_WEBAUTHN_ALLOW_ANY_PORT",
        default_value_t = false
    )]
    pub webauthn_allow_any_port: bool,
    /// Session cookie lifetime in seconds (the cookie's Max-Age and the
    /// `sessions.expires_at`).
    #[arg(
        long,
        env = "KALLIP_ARCHEION_SESSION_TTL_SECS",
        default_value = "2592000"
    )]
    pub session_ttl_secs: u64,
    /// Mark the session cookie `Secure` (recommended; only disable for local
    /// plain-HTTP dev where TLS is terminated elsewhere / absent).
    #[arg(long, env = "KALLIP_ARCHEION_COOKIE_SECURE", default_value_t = true)]
    pub cookie_secure: bool,
    /// Cookie `Domain` attribute. Set to the parent domain in a per-subdomain
    /// deploy so the session cookie is shared across `archeion.<d>` and
    /// `lesche.<d>` (e.g. `kallipai.com` prod, `localhost` dev). Unset =
    /// host-only (single-origin deploy).
    #[arg(long, env = "KALLIP_ARCHEION_SESSION_COOKIE_DOMAIN")]
    pub cookie_domain: Option<String>,
    /// Capacity of the per-IP token bucket guarding `/auth/*` (max burst).
    #[arg(long, env = "KALLIP_ARCHEION_AUTH_RATE_CAPACITY", default_value_t = 10)]
    pub auth_rate_capacity: u32,
    /// Refill rate of the per-IP auth rate bucket, in requests per second.
    #[arg(
        long,
        env = "KALLIP_ARCHEION_AUTH_RATE_REFILL_PER_SEC",
        default_value_t = 1
    )]
    pub auth_rate_refill_per_sec: u32,
    /// Capacity (max burst) of the SHARED token bucket capping aggregate
    /// throughput on the device-pairing begin endpoint. This is the real
    /// distributed brute-force bound on the short pairing code (per-IP limiting
    /// is bypassable by source-IP diversity). Pairing is rare (a legit user makes
    /// ~3 requests to mint + redeem across two devices), so this is tuned tight;
    /// raise it for larger deployments via this env var. Pair with
    /// `pair_rate_refill_per_sec`. With a 2^40 code space, even 1 req/s makes
    /// brute-forcing infeasible -- this is a CPU/storage-amplification cap, not
    /// the entropy backstop.
    #[arg(long, env = "KALLIP_ARCHEION_PAIR_RATE_CAPACITY", default_value_t = 10)]
    pub pair_rate_capacity: u32,
    /// Refill rate of the shared device-pairing bucket, in requests per second.
    #[arg(
        long,
        env = "KALLIP_ARCHEION_PAIR_RATE_REFILL_PER_SEC",
        default_value_t = 2
    )]
    pub pair_rate_refill_per_sec: u32,
    /// Comma-separated CIDRs whose direct connections are trusted to have set
    /// `X-Forwarded-For` (e.g. your reverse proxy). When the connecting peer is
    /// in one of these nets, the rate limiter buckets on the real client IP
    /// taken from XFF; otherwise XFF is ignored and the peer IP is used. The
    /// default trusts loopback, which is correct for the default same-box
    /// reverse-proxy deploy. A remote proxy must be added here explicitly.
    #[arg(
        long,
        env = "KALLIP_ARCHEION_TRUSTED_PROXIES",
        default_value = "127.0.0.0/8, ::1/128"
    )]
    pub trusted_proxies: String,
    /// Admin token (provisioning authority). Unset = generate a fresh
    /// `sk-admin-...` into the runtime file named by
    /// KALLIP_ARCHEION_ADMIN_TOKEN_OUT_FILE. The value is never logged.
    #[arg(long, env = "KALLIP_ARCHEION_ADMIN_TOKEN")]
    pub admin_token: Option<String>,
    /// Where a generated admin token is written (0600, KEY=value),
    /// required when --admin-token is unset. Runtime state: rewritten on
    /// every start and valid until the next restart, never logged.
    #[arg(long, env = "KALLIP_ARCHEION_ADMIN_TOKEN_OUT_FILE")]
    pub admin_token_out_file: Option<PathBuf>,
    /// Max HTTP request body size in kilobytes. 0 = axum default (2 MB).
    #[arg(long, env = "KALLIP_ARCHEION_MAX_BODY_SIZE_KB", default_value = "256")]
    pub max_body_size_kb: usize,
    /// Comma-separated CORS allowed origins (the app's origin(s)). Empty = no
    /// cross-origin allowed. Never use a wildcard on a public-facing deploy.
    #[arg(long, env = "KALLIP_ARCHEION_CORS_ORIGINS", default_value = "")]
    pub cors_origins: String,
    /// Enrollment-code lifetime in seconds (single-use, short-TTL).
    #[arg(
        long,
        env = "KALLIP_ARCHEION_ENROLLMENT_CODE_TTL_SECS",
        default_value = "600"
    )]
    pub enrollment_code_ttl_secs: u64,
    /// Whether open signup is allowed (both passkey-only signup and OAuth
    /// signup that creates a new account). The invite gate is gone; this is its
    /// replacement for closing signups during an incident. Existing-account
    /// login (passkey or OAuth) and account linking are unaffected.
    #[arg(long, env = "KALLIP_ARCHEION_SIGNUP_ENABLED", default_value_t = true)]
    pub signup_enabled: bool,
    /// Web app origin the OAuth flow redirects back into, e.g.
    /// `https://web.kallipai.com`. The canonical redirect_uri is this plus
    /// `/auth/callback`; the server is the single source of truth for it (the
    /// SPA never constructs it). Required only when an OAuth provider is
    /// configured.
    #[arg(long, env = "KALLIP_ARCHEION_OAUTH_REDIRECT_BASE")]
    pub oauth_redirect_base: Option<String>,
    /// GitHub OAuth app credentials. The provider is enabled iff BOTH
    /// client id and secret are non-empty.
    #[arg(long, env = "KALLIP_ARCHEION_OAUTH_GITHUB_CLIENT_ID")]
    pub oauth_github_client_id: Option<String>,
    #[arg(long, env = "KALLIP_ARCHEION_OAUTH_GITHUB_CLIENT_SECRET")]
    pub oauth_github_client_secret: Option<String>,
    /// Google OAuth client credentials. Enabled iff BOTH are non-empty.
    #[arg(long, env = "KALLIP_ARCHEION_OAUTH_GOOGLE_CLIENT_ID")]
    pub oauth_google_client_id: Option<String>,
    #[arg(long, env = "KALLIP_ARCHEION_OAUTH_GOOGLE_CLIENT_SECRET")]
    pub oauth_google_client_secret: Option<String>,
    /// File holding the platform-internal secret shared with the lesche,
    /// files, and instances services. Unset = the `/internal` nest is not
    /// mounted (archeion runs standalone). Set = first boot generates the
    /// secret into the file (0640, group-readable); later boots read the
    /// existing value and never overwrite it, so the four services keep
    /// agreeing while they restart around it. Machine-internal alignment
    /// material — regenerable, host-local — which is why the archeion owns
    /// it as state instead of asking the operator to ship it as config.
    #[arg(long, env = "KALLIP_ARCHEION_INTERNAL_TOKEN_FILE")]
    pub internal_token_file: Option<PathBuf>,
    /// Mount POST /auth/admin-login: exchange the admin token for a normal
    /// User session on a fixed local account (the local-platform login; see
    /// docs/reference/auth.md). Default off: the route is not mounted at
    /// all. When on, an operator-set KALLIP_ARCHEION_ADMIN_TOKEN shorter than
    /// 32 chars fails at boot (the generated 256-bit token is exempt).
    #[arg(
        long,
        env = "KALLIP_ARCHEION_ADMIN_USER_LOGIN",
        default_value_t = false
    )]
    pub admin_user_login: bool,
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    /// The default RP pair must satisfy the boot invariant (rp_id is an
    /// effective domain of rp_origin) so an unconfigured archeion still boots.
    #[test]
    fn default_webauthn_rp_pair_passes_effective_domain() {
        let args = Args::parse_from(["kallip-archeion", "--database-url", "postgres://x"]);
        assert_eq!(args.webauthn_rp_id, "kallipai.com");
        let origin = url::Url::parse(&args.webauthn_rp_origin).unwrap();
        // Assert via the real boot path: the builder checks rp_id is an
        // effective domain of rp_origin (host_str() alone diverges on IPs).
        crate::state::build_webauthn_pair(
            &args.webauthn_rp_name,
            &args.webauthn_rp_id,
            &origin,
            false,
            false,
        )
        .expect("default RP pair must satisfy the boot invariant");
        assert_eq!(args.webauthn_rp_name, "kallipai");
    }
}
