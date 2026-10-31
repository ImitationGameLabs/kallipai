//! Blob verb: rewriting an instance's blob store on disk.

use crate::cli::RewriteTarget;
use anyhow::Context as _;
use anyhow::Result;
use kallipai_daemon_client::DaemonClient;
use kallipai_daemon_common::wire::{InstanceState, OkPayload, RequestBody, ResponseBody};

/// One fully-parsed rewrite invocation, handed over by the CLI
/// dispatch.
pub(crate) struct RewriteInvocation {
    pub(crate) target: RewriteTarget,
    pub(crate) slug: String,
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
    // The daemon's record supplies the data directory; a running
    // instance is a warning, not a refusal -- the walk plus atomic
    // renames stays safe under a live writer (the worst case is a
    // concurrent write winning the rename race and getting re-framed
    // next run) -- but the operator should know.
    let response = client
        .call(RequestBody::Record {
            slug: args.slug.clone(),
        })
        .await
        .context("talking to the kallipai daemon")?;
    let data_dir = match response.body {
        ResponseBody::Ok {
            payload: OkPayload::Record { data_dir, state },
        } => {
            if state == InstanceState::Running {
                eprintln!(
                    "warning: instance {} is running; rewriting its store in place",
                    args.slug
                );
            }
            data_dir
        }
        ResponseBody::Err { message, .. } => {
            anyhow::bail!("cannot rewrite {}: {message}", args.slug);
        }
        _ => anyhow::bail!("unexpected daemon response while resolving {}", args.slug),
    };
    let root = data_dir.join("blobs").join(args.target.dir_name());
    // The instance stores' writers have no size threshold at all, so the
    // default is unlimited and the tool never disagrees with its writer.
    let threshold = args.above_bytes.filter(|n| *n > 0);
    let report = kallipai_blob_store::rewrite_root(
        &root,
        &kallipai_blob_store::RewriteOptions {
            dry_run: args.dry_run,
            level: args.level,
            corrupt,
            progress_every: args.progress_every,
            max_stored_bytes: threshold,
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
