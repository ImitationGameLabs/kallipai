//! The restart verb: stop, then start.

use anyhow::Context as _;
use anyhow::Result;
use kallipai_daemon_client::DaemonClient;
use kallipai_daemon_common::wire::{ErrorCode, RequestBody, ResponseBody};

use crate::print::{error_prefix, print};

/// The restart orchestration: stop if running, then start. The daemon
/// has no restart verb — the CLI composes the two, exactly the shape
/// systemd uses for `restart` on a plain service: an already-stopped
/// instance skips the stop (NotRunning is the expected outcome, not
/// an error), and the start replays the record's latest env. The end
/// state is one process running the newest recorded configuration.
pub(crate) async fn run_restart(client: &DaemonClient, slug: &str) -> Result<()> {
    let stop = client
        .call(RequestBody::Stop {
            slug: slug.to_owned(),
        })
        .await
        .context("talking to the kallipai daemon")?;
    match stop.body {
        ResponseBody::Ok { .. } => println!("stopped {slug}"),
        ResponseBody::Err {
            code: ErrorCode::NotRunning,
            ..
        } => {}
        ResponseBody::Err { code, message } => {
            anyhow::bail!("{}: {message}", error_prefix(code));
        }
    }
    let start = client
        .call(RequestBody::Start {
            slug: slug.to_owned(),
            env: Vec::new(),
            exe: None,
        })
        .await
        .context("talking to the kallipai daemon")?;
    print(start, true)
}
