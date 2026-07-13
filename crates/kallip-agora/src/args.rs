use clap::Parser;

/// CLI arguments for `kallip-agora`.
///
/// The agora is intended to sit behind a TLS-terminating reverse proxy; it
/// serves plain HTTP and binds localhost by default.
#[derive(Parser)]
#[command(
    name = "kallip-agora",
    about = "Public-internet relay control plane for kallip agent teams"
)]
pub struct Args {
    /// Address to listen on (behind a TLS-terminating reverse proxy).
    #[arg(long, env = "KALLIP_AGORA_ADDR", default_value = "127.0.0.1:7100")]
    pub listen_addr: String,
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
    /// Acceptable clock skew (both directions) on a herald tunnel reconnect
    /// proof's timestamp, in seconds.
    #[arg(long, env = "KALLIP_AGORA_PROOF_SKEW_SECS", default_value = "60")]
    pub proof_skew_secs: i64,
    /// Per-user cap on live conversations, bounding memory growth against
    /// unbounded conversation creation.
    #[arg(
        long,
        env = "KALLIP_AGORA_MAX_CONVERSATIONS_PER_USER",
        default_value = "64"
    )]
    pub max_conversations_per_user: usize,
    /// How long a synchronous key exchange waits for the herald's response
    /// before failing with 504, in seconds.
    #[arg(
        long,
        env = "KALLIP_AGORA_KEY_EXCHANGE_TIMEOUT_SECS",
        default_value = "10"
    )]
    pub key_exchange_timeout_secs: u64,
}
