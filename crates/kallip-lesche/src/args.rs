use clap::Parser;

/// CLI arguments for `kallip-lesche`, the data-plane relay.
///
/// The lesche is stateless soft-state (presence, conversations, app streams are
/// rebuilt on restart from heralds reconnecting + conversations created on
/// demand); all durable identity / credential / tagma metadata stays in the
/// agora, reached through the `/internal/*` ControlPlane API.
#[derive(Parser)]
#[command(
    name = "kallip-lesche",
    about = "kallip data-plane relay: herald tunnels, app events, envelope routing"
)]
pub struct Args {
    /// Address to listen on (behind a TLS-terminating reverse proxy).
    #[arg(long, env = "KALLIP_LESCHE_ADDR", default_value = "127.0.0.1:7200")]
    pub listen_addr: String,
    /// Agora internal base URL for `/internal/*` ControlPlane calls (e.g.
    /// `http://127.0.0.1:7100`). Must NOT be publicly reachable.
    #[arg(long, env = "KALLIP_LESCHE_AGORA_INTERNAL_URL")]
    pub agora_internal_url: String,
    /// Shared secret bearer for the agora `/internal/*` API. Must equal the
    /// agora's `KALLIP_AGORA_INTERNAL_TOKEN`.
    #[arg(long, env = "KALLIP_LESCHE_AGORA_TOKEN")]
    pub agora_internal_token: String,
    /// Acceptable clock skew (both directions) on a herald tunnel reconnect
    /// proof's timestamp, in seconds.
    #[arg(long, env = "KALLIP_LESCHE_PROOF_SKEW_SECS", default_value = "60")]
    pub proof_skew_secs: i64,
    /// How long a synchronous key exchange waits for the herald's response
    /// before failing with 504, in seconds.
    #[arg(
        long,
        env = "KALLIP_LESCHE_KEY_EXCHANGE_TIMEOUT_SECS",
        default_value = "10"
    )]
    pub key_exchange_timeout_secs: u64,
    /// Max HTTP request body size in kilobytes. 0 = axum default (2 MB).
    #[arg(long, env = "KALLIP_LESCHE_MAX_BODY_SIZE_KB", default_value = "256")]
    pub max_body_size_kb: usize,
    /// Comma-separated CORS allowed origins (the app's origin(s)). Empty = no
    /// cross-origin allowed. Never use a wildcard on a public-facing deploy.
    #[arg(long, env = "KALLIP_LESCHE_CORS_ORIGINS", default_value = "")]
    pub cors_origins: String,
}
