use clap::Parser;

/// CLI arguments for `kallipai-lesche`, the data-plane relay.
///
/// The lesche owns the durable chat store (rooms, membership, message
/// payloads) in its own Postgres; the per-tagma agent streams and per-user
/// app streams are soft state rebuilt on restart. Durable identity /
/// credential / tagma metadata stays in the archeion, reached through the
/// `/internal/*` ControlPlane API.
#[derive(Parser)]
#[command(
    name = "kallipai-lesche",
    version,
    about = "kallipai data-plane relay: rooms, direct sessions, agent streams"
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
