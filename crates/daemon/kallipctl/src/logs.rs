//! Log verbs: cursor reads.

use crate::print::{error_prefix, print_log_text};
use anyhow::Context as _;
use anyhow::Result;
use kallipai_daemon_client::DaemonClient;
use kallipai_daemon_common::wire::{LogCursor, OkPayload, RequestBody, ResponseBody};

/// Client-side mirrors of the daemon's line-count bounds: the CLI
/// always sends a concrete count; the daemon clamps its copy too.
const DEFAULT_LINES: u32 = 20;

const MAX_LINES: u32 = 1000;

/// The logs verb's own loop: one request per beat (the wire has no
/// streaming), printing each increment. The first beat carries the
/// tail (the `--lines` first screen); later beats carry only what
/// the cursor has not seen. A rotated or emptied log set comes back
/// with a changed file name, a regressed offset, or no next cursor
/// at all — warn and adopt the daemon's rebased cursor instead of
/// silently stalling at a dead position.
pub(crate) async fn run_logs(
    client: &DaemonClient,
    slug: &str,
    lines: Option<u32>,
    file: Option<&str>,
    follow: bool,
) -> Result<()> {
    let lines = Some(lines.unwrap_or(DEFAULT_LINES).min(MAX_LINES));
    let mut cursor: Option<LogCursor> = None;
    loop {
        let body = RequestBody::Log {
            slug: slug.to_string(),
            lines,
            file: file.map(str::to_string),
            cursor: cursor.clone(),
        };
        let response = client
            .call(body)
            .await
            .context("talking to the kallipai daemon")?;
        match response.body {
            ResponseBody::Ok {
                payload: OkPayload::Log { text, next_cursor },
            } => {
                if cursor_lost(cursor.as_ref(), next_cursor.as_ref()) {
                    eprintln!("log cursor regressed or the log set changed; re-baselining");
                }
                match (text.is_empty(), follow) {
                    (true, false) => println!("(no logs)"),
                    (true, true) => {}
                    (false, _) => print_log_text(&text),
                }
                cursor = next_cursor;
            }
            ResponseBody::Err { code, message } => {
                anyhow::bail!("{}: {message}", error_prefix(code));
            }
            other => anyhow::bail!("unexpected response: {other:?}"),
        }
        if !follow {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// True when the tail's baseline is gone: the next cursor names
/// another file or sits before the sent byte offset, or the server
/// returned no cursor at all. The log loop re-baselines after the
/// warning this signals.
fn cursor_lost(sent: Option<&LogCursor>, next: Option<&LogCursor>) -> bool {
    match (sent, next) {
        (Some(sent), Some(next)) => next.file != sent.file || next.byte < sent.byte,
        (Some(_), None) => true,
        _ => false,
    }
}
