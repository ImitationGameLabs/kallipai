use clap::Parser;

/// CLI arguments for `kallipai-files`, the file transfer service.
///
/// The service owns two stores: the content-addressed blob directory
/// (local volume; the cloud object-store backend is a postponed trait
/// sibling) and the metadata Postgres. Identity and enrollment facts stay
/// in the archeion, reached through the `/internal/*` ControlPlane API.
#[derive(Parser)]
#[command(
    name = "kallipai-files",
    version,
    about = "kallipai file transfer service: content-addressed blobs, ACL'd spaces"
)]
pub struct Args {
    /// Address to listen on (behind a TLS-terminating reverse proxy).
    #[arg(long, env = "KALLIPAI_FILES_ADDR", default_value = "127.0.0.1:7400")]
    pub listen_addr: String,
    /// Root directory of the content-addressed blob store. Created on
    /// demand by the store itself.
    #[arg(long, env = "KALLIPAI_FILES_BLOB_ROOT")]
    pub blob_root: String,
    /// Archeion internal base URL for `/internal/*` ControlPlane calls (e.g.
    /// `http://127.0.0.1:7100`). Must NOT be publicly reachable.
    #[arg(long, env = "KALLIPAI_FILES_ARCHEION_INTERNAL_URL")]
    pub archeion_internal_url: String,
    /// File holding the shared secret bearer for the archeion
    /// `/internal/*` API — provisioned by the archeion (0640 in its state
    /// directory), read here at boot and never rewritten by this service.
    #[arg(long, env = "KALLIPAI_POLIS_INTERNAL_TOKEN_FILE")]
    pub archeion_internal_token_file: String,
    /// Postgres URL for the metadata store (e.g.
    /// `postgres://user:pass@host/db`). Required: records, refcounts, and
    /// the delivery log are the service's durable surface; a missing URL
    /// fails fast at boot rather than silently running with no store.
    #[arg(long, env = "KALLIPAI_FILES_DATABASE_URL")]
    pub database_url: String,
    /// Maximum accepted upload body, in megabytes. A larger stream is cut
    /// off with 413. The default is a placeholder value (100 MB); the
    /// operator may tune it (structure is unaffected).
    #[arg(long, env = "KALLIPAI_FILES_MAX_BODY_SIZE_MB", default_value_t = 100)]
    pub max_body_size_mb: u64,
    /// Comma-separated CORS allowed origins (the app's origin). Empty (the
    /// default) = no cross-origin allowed at all: the allowlist is
    /// `AllowOrigin::list` (never `Any`), so a misconfigured `*` yields an
    /// empty allowlist rather than an open hole. Same shape as the archeion's
    /// `KALLIPAI_ARCHEION_CORS_ORIGINS`.
    #[arg(long, env = "KALLIPAI_FILES_CORS_ORIGINS", default_value = "")]
    pub cors_origins: String,
    /// Lesche internal base URL for the file-delivered event push (e.g.
    /// `http://lesche:7200`). Empty disables the push entirely.
    #[arg(long, env = "KALLIPAI_FILES_NOTIFY_URL", default_value = "")]
    pub notify_url: String,
    /// Shared secret bearer for the lesche internal API. Must equal the
    /// lesche's `KALLIPAI_LESCHE_INTERNAL_TOKEN`.
    #[arg(long, env = "KALLIPAI_FILES_NOTIFY_TOKEN", default_value = "")]
    pub notify_token: String,
    /// Archeion degrade posture (seventh approved default). `closed` (the
    /// default) fails every authorization decision with 503 when the
    /// registry cannot answer; `soft` degrades the enrollment lookup to an
    /// empty fact set, so tagma decisions deny with 403 instead of 503.
    /// Neither posture weakens credential verification.
    #[arg(long, env = "KALLIPAI_FILES_DEGRADE", value_parser = ["closed", "soft"], default_value = "closed")]
    pub degrade: String,
    /// Delay between GC passes (sweep + reconcile), in seconds.
    #[arg(long, env = "KALLIPAI_FILES_GC_INTERVAL_SECS", default_value_t = 60)]
    pub gc_interval_secs: u64,
    /// How long a zero-refcount row must have been freed before the GC may
    /// reclaim it and unlink the file, in seconds.
    #[arg(long, env = "KALLIPAI_FILES_GC_GRACE_SECS", default_value_t = 60)]
    pub gc_grace_secs: u64,
    /// Maximum catalog rows reclaimed per GC pass.
    #[arg(long, env = "KALLIPAI_FILES_GC_BATCH", default_value_t = 128)]
    pub gc_batch: u32,
    /// How new uploads are physically stored: `zstd` frames them
    /// (below the size threshold), `off` keeps every object raw. This
    /// is the write-side policy only -- reads self-identify from the
    /// bytes either way.
    #[arg(
        long,
        env = "KALLIPAI_FILES_BLOB_COMPRESSION",
        value_parser = clap::builder::PossibleValuesParser::new(kallipai_blob_store::Compression::CONFIG_WORDS),
        default_value = "off"
    )]
    pub blob_compression: String,
    /// zstd level for newly framed uploads (clamped 1..=22).
    #[arg(long, env = "KALLIPAI_FILES_BLOB_COMPRESSION_LEVEL", default_value_t = 3, value_parser = clap::value_parser!(i32).range(1..=22))]
    pub blob_compression_level: i32,
    /// Uploads larger than this many logical bytes stay raw even when
    /// compression is on: a compressed ranged read decodes the whole
    /// frame, so large objects keep raw cost and memory behavior.
    /// `0` disables the threshold.
    #[arg(long, env = "KALLIPAI_FILES_BLOB_COMPRESSION_ABOVE_BYTES", default_value_t = 8 * 1024 * 1024)]
    pub blob_compression_above_bytes: u64,
}
