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
/// state is one process running the newest recorded configuration (a
/// `--bin` overrides the binary of that launch). `exe` is the operator's
/// `--bin` when one was given: the same dev-only override spawn and start
/// carry.
pub(crate) async fn run_restart(
    client: &DaemonClient,
    slug: &str,
    exe: Option<&str>,
) -> Result<()> {
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
        .call(restart_start_body(slug, exe))
        .await
        .context("talking to the kallipai daemon")?;
    match print(start, true) {
        Ok(()) => Ok(()),
        Err(error) => Err(error.context(format!(
            "the instance stays stopped; fix the cause and run kallipctl start {slug}"
        ))),
    }
}

/// The start half of restart as a wire body: the env is empty because the
/// daemon replays the record's own, and `exe` carries the operator's
/// `--bin` when given. Named apart from the `Command::Start` body in
/// `main.rs`, which also carries a one-shot env overlay.
pub(crate) fn restart_start_body(slug: &str, exe: Option<&str>) -> RequestBody {
    RequestBody::Start {
        slug: slug.to_owned(),
        env: Vec::new(),
        exe: exe.map(str::to_owned),
    }
}
