//! Log verbs: cursor reads and level filtering.

use crate::print::{error_prefix, print, print_log_text};
use anyhow::Context as _;
use anyhow::Result;
use kallipai_daemon_client::DaemonClient;
use kallipai_daemon_common::wire::{ErrorCode, LogCursor, OkPayload, RequestBody, ResponseBody};

/// Probe the files service's unauthenticated /health: `Some(detail)`
/// means something answered (the warning fires), `None` means nothing
/// is listening on the service address.
pub(crate) async fn files_service_answers(addr: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
    let request = format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut buf = vec![0u8; 256];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]).into_owned();
    let first = head.lines().next().unwrap_or("").to_owned();
    Some(
        if first.starts_with("HTTP/1.1 200") || first.starts_with("HTTP/1.0 200") {
            "answers /health with 200".to_owned()
        } else if first.is_empty() {
            "accepts connections but answered nothing".to_owned()
        } else {
            format!("answered unexpectedly: {first}")
        },
    )
}

/// Client-side mirrors of the daemon's line-count defaults: the CLI
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

/// The level words a rendered tracing line carries: `--level` keeps
/// only the lines mentioning the chosen one.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum LogLevel {
    /// Error lines.
    Error,
    /// Warning lines.
    Warn,
    /// Informational lines.
    Info,
    /// Debug lines.
    Debug,
}

impl LogLevel {
    /// The word as it appears in a rendered line.
    fn word(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
        }
    }
}

/// Keep only the lines carrying `level`'s word.
pub(crate) fn keep_level(lines: &[String], level: Option<LogLevel>) -> Vec<String> {
    match level {
        None => lines.to_vec(),
        Some(level) => lines
            .iter()
            .filter(|line| line.contains(level.word()))
            .cloned()
            .collect(),
    }
}

/// True when the tail's baseline is gone: the next cursor names
/// another file or sits before the sent byte offset, or the server
/// returned no cursor at all. Both log loops re-baseline after the
/// warning this signals.
fn cursor_lost(sent: Option<&LogCursor>, next: Option<&LogCursor>) -> bool {
    match (sent, next) {
        (Some(sent), Some(next)) => next.file != sent.file || next.byte < sent.byte,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The bare `logs` shape: the services that have a log directory,
/// through the same endpoint the completion and the tail read.
pub(crate) async fn run_service_log_list(client: &DaemonClient) -> Result<()> {
    let response = client
        .call(RequestBody::ServiceLogList)
        .await
        .context("talking to the kallipai daemon")?;
    match response.body {
        ResponseBody::Ok {
            payload: OkPayload::ServiceLogList { services },
        } => {
            if services.is_empty() {
                anyhow::bail!(
                    "no service log directories on this host; container deployments \
                     log inside the containers -- use `docker logs <container>` there"
                );
            }
            println!("services with logs on this host:");
            for service in &services {
                println!("  {service}");
            }
            println!("tail one with `kallipctl logs --service <name>`");
            Ok(())
        }
        ResponseBody::Err { code, message } => {
            anyhow::bail!("{}: {message}", error_prefix(code))
        }
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// The service-logs loop, the instance loop's mirror: one request
/// per beat, the first carrying the `--lines` first screen and
/// later ones only what the cursor has not seen. Level filtering
/// applies to the tail and to every beat alike.
pub(crate) async fn run_service_logs(
    client: &DaemonClient,
    service: &str,
    lines: Option<u32>,
    follow: bool,
    level: Option<LogLevel>,
) -> Result<()> {
    let lines = lines.map(|n| n.min(MAX_LINES));
    let mut cursor: Option<LogCursor> = None;
    loop {
        let body = RequestBody::ServiceLog {
            service: service.to_string(),
            lines,
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
                let screen = text.lines().map(str::to_string).collect::<Vec<_>>();
                let kept = keep_level(&screen, level);
                match (kept.is_empty(), follow) {
                    (true, false) => println!("(no matching lines)"),
                    (true, true) => {}
                    (false, _) => {
                        for line in &kept {
                            println!("{line}");
                        }
                    }
                }
                cursor = next_cursor;
            }
            ResponseBody::Err { code, message } => {
                anyhow::bail!("{}: {message}", error_prefix(code))
            }
            other => anyhow::bail!("unexpected response: {other:?}"),
        }
        if !follow {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}
