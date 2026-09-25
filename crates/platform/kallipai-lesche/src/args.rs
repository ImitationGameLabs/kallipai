use clap::Parser;

/// CLI arguments for `kallipai-lesche`, the data-plane relay.
///
/// The lesche's in-memory surfaces (presence, conversations, app streams) are
/// rebuilt on restart from tagmas reconnecting + conversations created on
/// demand; it owns the durable chat store (rooms, membership, message payloads)
/// in its own Postgres. Durable identity / credential / tagma metadata stays in
/// the archeion, reached through the `/internal/*` ControlPlane API.
#[derive(Parser)]
#[command(
    name = "kallipai-lesche",
    version,
    about = "kallipai data-plane relay: tagma tunnels, app events, envelope routing"
)]
pub struct Args {
    /// Address to listen on (behind a TLS-terminating reverse proxy).
    #[arg(long, env = "KALLIPAI_LESCHE_ADDR", default_value = "127.0.0.1:7200")]
    pub listen_addr: String,
    /// Archeion internal base URL for `/internal/*` ControlPlane calls (e.g.
    /// `http://127.0.0.1:7100`). Must NOT be publicly reachable.
    #[arg(long, env = "KALLIPAI_LESCHE_ARCHEION_INTERNAL_URL")]
    pub archeion_internal_url: String,
    /// File holding the shared secret bearer for the archeion
    /// `/internal/*` API — provisioned by the archeion (0640 in its state
    /// directory), read here at boot and never rewritten by this service.
    #[arg(long, env = "KALLIPAI_POLIS_INTERNAL_TOKEN_FILE")]
    pub archeion_internal_token_file: String,
    /// Shared secret bearer for THIS service's internal surface, consumed
    /// by the files service to push file-delivery events. Must equal the
    /// files service's KALLIPAI_FILES_NOTIFY_TOKEN. Empty (the default)
    /// leaves the internal surface unmounted -- the event push is then
    /// disabled, which is the safe posture for a standalone lesche.
    #[arg(default_value = "", long, env = "KALLIPAI_LESCHE_INTERNAL_TOKEN")]
    pub internal_token: String,
    /// Acceptable clock skew (both directions) on a tagma tunnel reconnect
    /// proof's timestamp, in seconds.
    #[arg(long, env = "KALLIPAI_LESCHE_PROOF_SKEW_SECS", default_value = "60")]
    pub proof_skew_secs: i64,
    /// How long a synchronous key exchange waits for the tagma's response
    /// before failing with 504, in seconds.
    #[arg(
        long,
        env = "KALLIPAI_LESCHE_KEY_EXCHANGE_TIMEOUT_SECS",
        default_value = "10"
    )]
    pub key_exchange_timeout_secs: u64,
    /// Max HTTP request body size in kilobytes. 0 = axum default (2 MB).
    #[arg(long, env = "KALLIPAI_LESCHE_MAX_BODY_SIZE_KB", default_value = "256")]
    pub max_body_size_kb: usize,
    /// Comma-separated CORS allowed origins (the app's origin(s)). Empty = no
    /// cross-origin allowed. Never use a wildcard on a public-facing deploy.
    #[arg(long, env = "KALLIPAI_LESCHE_CORS_ORIGINS", default_value = "")]
    pub cors_origins: String,
    /// Postgres URL for the durable room-message store (e.g.
    /// `postgres://user:pass@host/db`). Required: the relay's durable surface
    /// is room history, so a missing URL fails fast at boot rather than
    /// silently running with no store.
    #[arg(long, env = "KALLIPAI_LESCHE_DATABASE_URL")]
    pub database_url: String,
}
