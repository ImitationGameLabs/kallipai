//! `kallip-files`: the kallip file transfer service.
//!
//! Content lives in a local content-addressed blob directory; records,
//! reference counts, and the delivery log live in the service's own
//! Postgres. Every request authenticates against the archeion through the
//! `/internal/*` ControlPlane API (per request, no cache) and is
//! authorized by the single-point ACL (`acl`) against the two-layer space
//! namespace.

mod args;

use std::time::Duration;

use clap::Parser;

use crate::args::Args;
use kallip_files::gc;
use kallip_files::state::{BootConfig, FilesConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // File logging is opt-in via KALLIP_FILES_LOG_DIR: set, events are
    // double-written to a rolling file and stdout; unset keeps the historical
    // stdout-only behavior, so container and dev forms are untouched.
    let log_dir = kallip_common::logging::parse_log_dir(std::env::var("KALLIP_FILES_LOG_DIR").ok());
    kallip_common::logging::init_service_logging(&filter, "files", log_dir.as_deref());

    let config = FilesConfig {
        max_body_bytes: args.max_body_size_mb * 1024 * 1024,
        degrade_fail_soft: args.degrade == "soft",
        cors_origins: args.cors_origins,
        gc: gc::GcConfig {
            batch: args.gc_batch,
            interval: Duration::from_secs(args.gc_interval_secs),
            grace: Duration::from_secs(args.gc_grace_secs),
        },
    };
    let boot = BootConfig {
        listen_addr: args.listen_addr,
        database_url: args.database_url,
        archeion_internal_url: args.archeion_internal_url,
        archeion_internal_token: kallip_common::secret_file::read_trimmed_with_retry(
            std::path::Path::new(&args.archeion_internal_token_file),
            kallip_common::secret_file::BOOT_RETRY,
        )?,
        blob_root: args.blob_root.into(),
        blob_policy: kallip_blob_store::IngestPolicy {
            compression: kallip_blob_store::Compression::from_config_word(
                &args.blob_compression,
                args.blob_compression_level,
            ),
            compress_above: (args.blob_compression_above_bytes > 0)
                .then_some(args.blob_compression_above_bytes),
        },
        files: config,
        notify_url: args.notify_url,
        notify_token: args.notify_token,
    };

    tracing::info!("starting kallip-files");
    kallip_files::state::run(boot).await
}
