//! Blob verb: listing the daemon blob store and rewriting its references.

use crate::cli::RewriteTarget;
use crate::logs::files_service_answers;
use anyhow::Context as _;
use anyhow::Result;
use kallipai_daemon_client::DaemonClient;
use kallipai_daemon_common::wire::{InstanceState, OkPayload, RequestBody, ResponseBody};

/// One fully-parsed rewrite invocation, handed over by the CLI
/// dispatch.
pub(crate) struct RewriteInvocation {
    pub(crate) target: RewriteTarget,
    pub(crate) slug: Option<String>,
    pub(crate) blob_root: Option<String>,
    pub(crate) above_bytes: Option<u64>,
    pub(crate) dry_run: bool,
    pub(crate) skip_corrupt: bool,
    pub(crate) progress_every: usize,
    pub(crate) level: i32,
}

/// The blobs rewrite verb: plain on-disk work under the same
/// Result-based flow the wire verbs use, against whichever store the
/// target names.
pub(crate) async fn run_blobs_rewrite(
    args: RewriteInvocation,
    client: &DaemonClient,
) -> Result<()> {
    let corrupt = if args.skip_corrupt {
        kallipai_blob_store::CorruptPolicy::Skip
    } else {
        kallipai_blob_store::CorruptPolicy::Fail
    };
    let root = match args.target {
        RewriteTarget::Tasks | RewriteTarget::Attachments => {
            let slug = args
                .slug
                .as_deref()
                .context("this target rewrites a specific instance's store: pass --slug")?;
            // The daemon's record supplies the data directory; a
            // running instance is a warning, not a refusal -- the walk
            // plus atomic renames stays safe under a live writer (the
            // worst case is a concurrent write winning the rename race
            // and getting re-framed next run) -- but the operator
            // should know.
            let response = client
                .call(RequestBody::Record {
                    slug: slug.to_owned(),
                })
                .await
                .context("talking to the kallipai daemon")?;
            let data_dir = match response.body {
                ResponseBody::Ok {
                    payload: OkPayload::Record { data_dir, state },
                } => {
                    if state == InstanceState::Running {
                        eprintln!(
                            "warning: instance {slug} is running; rewriting its store in place"
                        );
                    }
                    data_dir
                }
                ResponseBody::Err { message, .. } => {
                    anyhow::bail!("cannot rewrite {slug}: {message}");
                }
                _ => anyhow::bail!("unexpected daemon response while resolving {slug}"),
            };
            data_dir.join("blobs").join(args.target.dir_name())
        }
        RewriteTarget::Files => {
            if args.slug.is_some() {
                anyhow::bail!("--slug applies to the tasks/attachments targets only");
            }
            let addr = std::env::var("KALLIPAI_FILES_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:7400".to_owned());
            if let Some(detail) = files_service_answers(&addr).await {
                eprintln!("warning: the files service answers on {addr} ({detail})");
            }
            args.blob_root
                .or_else(|| std::env::var("KALLIPAI_FILES_BLOB_ROOT").ok())
                .context("--blob-root or KALLIPAI_FILES_BLOB_ROOT is required")?
                .into()
        }
    };
    // The size threshold defaults to each store's writer policy: the
    // files service reads its env (8 MiB fallback), while the daemon
    // stores' writers have no threshold at all -- so the default there
    // is unlimited, and the tool never disagrees with its writer.
    let threshold = match args.above_bytes {
        Some(v) => v,
        None => match args.target {
            RewriteTarget::Files => {
                match std::env::var("KALLIPAI_FILES_BLOB_COMPRESSION_ABOVE_BYTES") {
                    Ok(v) => v
                        .parse::<u64>()
                        .context("KALLIPAI_FILES_BLOB_COMPRESSION_ABOVE_BYTES is not a number")?,
                    Err(_) => 8 * 1024 * 1024,
                }
            }
            // The daemon stores' writers have no threshold at all.
            _ => 0,
        },
    };
    let report = kallipai_blob_store::rewrite_root(
        &root,
        &kallipai_blob_store::RewriteOptions {
            dry_run: args.dry_run,
            level: args.level,
            corrupt,
            progress_every: args.progress_every,
            max_stored_bytes: (threshold > 0).then_some(threshold),
        },
    )?;
    print_rewrite_summary(&report);
    Ok(())
}

/// The shared end-of-run summary: the same honestly-counted totals in
/// every mode. The dry run never invents an "after" number.
fn print_rewrite_summary(report: &kallipai_blob_store::RewriteReport) {
    println!(
        "scanned={} rewritten={} would_rewrite={} already_compressed={} skipped_corrupt={} skipped_oversized={} bytes_raw={} bytes_compressed={}",
        report.scanned,
        report.rewritten,
        report.dry_run_would_rewrite,
        report.already_compressed,
        report.skipped_corrupt,
        report.skipped_oversized,
        report.bytes_raw,
        report.bytes_compressed,
    );
}
