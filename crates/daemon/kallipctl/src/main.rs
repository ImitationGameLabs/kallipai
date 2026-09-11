//! kallipctl: operator-side management CLI for the kallip local daemon.
//!
//! Five verbs over the daemon's UDS protocol; the socket's 0600 mode is the
//! auth. Deliberately NOT part of the `kallip` command family: `kallip` is
//! the in-instance runtime, `kallipctl` manages instances from outside
//! (separate installation surfaces).

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use kallip_daemon_client::DaemonClient;
use kallip_daemon_common::wire::{
    ErrorCode, InstanceState, LogCursor, OkPayload, RequestBody, Response, ResponseBody,
};

#[derive(Parser)]
#[command(
    name = "kallipctl",
    about = "Manage local kallip instances via the kallip daemon",
    version
)]
struct Cli {
    /// Daemon control socket. When omitted the shared resolution chain
    /// is probed in order: $KALLIP_DAEMON_SOCKET, the runtime dir, the
    /// state home default - the first socket that answers wins.
    #[arg(long, global = true)]
    socket: Option<String>,

    /// Dev-only: run this explicit tagma binary in the launched instance
    /// instead of the daemon's resolved one (dev loops testing a fresh
    /// build against a running daemon).
    #[arg(long, global = true)]
    bin: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Launch a new instance under a slug.
    Spawn {
        /// Instance slug: lowercase letters, digits, and '-'; must start
        /// with a letter or digit.
        slug: String,
        /// Absolute path of the instance workspace.
        workspace: String,
        /// Extra env for the instance, KEY=VALUE (repeatable); only
        /// KALLIP_* keys plus RUST_LOG and PATH are accepted by the daemon.
        /// `KALLIP_TAGMA_ADDR=<addr>` pins the tagma's listen address
        /// (default 127.0.0.1:0); prefer a concrete interface or the
        /// polis proxy over 0.0.0.0 — the API is Bearer-gated but plain
        /// HTTP on the LAN.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Run the instance as this pre-declared system user (the
        /// dedicated-user form; requires the daemon to run as root).
        #[arg(long)]
        user: Option<String>,
    },
    /// Terminate an instance (TERM, grace, KILL).
    Stop { slug: String },
    /// Deregister an instance without touching its process: a
    /// running instance keeps running unmanaged — stop it first if
    /// you want it terminated. Idempotent.
    Remove { slug: String },
    /// Relaunch a stopped or dead instance under its recorded workspace
    /// and env.
    Start {
        /// Instance slug.
        slug: String,
        /// One-shot env overlay, KEY=VALUE (repeatable); applied to this
        /// launch only, never persisted to the instance's record.
        /// Same allowlist as spawn's env.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
    },
    /// List managed instances.
    List,
    /// Daemon health, or one instance's health by slug.
    Health { slug: Option<String> },
    /// Register an existing instance (running or stopped) under this
    /// daemon: pure registration, no process is launched or signaled.
    Adopt {
        /// Instance slug: same grammar as spawn's.
        slug: String,
        /// Absolute path of the instance workspace.
        #[arg(long)]
        workspace: String,
        /// Absolute path of the instance's data directory (runtime.json,
        /// credentials/). Must carry at least one of the two.
        #[arg(long)]
        data_dir: String,
        /// Persistent env snapshot, KEY=VALUE (repeatable); same
        /// allowlist as spawn's env, replayed by later starts.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Dedicated-user form (requires the daemon to run as root).
        #[arg(long)]
        user: Option<String>,
        /// Assert the instance is meant to run local-only; skips the
        /// adopt-time relay probe.
        #[arg(long)]
        accept_local_only: bool,
    },
    /// Tail an instance's log files (read-only diagnostic): the
    /// merged tail across retained daily files, one file with
    /// --file, or live with --follow.
    Log {
        /// Instance slug: same grammar as spawn's.
        slug: String,
        /// Lines from the tail (default 20, capped at 1000).
        #[arg(short = 'n', long = "lines")]
        lines: Option<u32>,
        /// Read this single file from the log directory instead of
        /// the merged tail.
        #[arg(long)]
        file: Option<String>,
        /// Keep polling for new lines (~500 ms beats) until Ctrl-C.
        #[arg(short = 'f', long = "follow")]
        follow: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // The shared chain, probed in order - the first socket that answers
    // is wherever the daemon actually bound (identical ordering on both
    // sides is what keeps client and daemon converged).
    let candidates = kallip_daemon_common::socket::candidates_from_env(
        cli.socket.as_deref().map(std::path::Path::new),
    );
    let socket = kallip_daemon_common::socket::probe(&candidates).map_err(|err| {
        anyhow::anyhow!(kallip_daemon_common::socket::describe_probe_failure(&err))
    })?;
    let client = DaemonClient::new(socket);

    let started = matches!(cli.command, Command::Start { .. });
    // Cheap client-side slug check: the same grammar the daemon
    // enforces (shape and the 64-char cap), caught before a
    // round-trip.
    let slug = match &cli.command {
        Command::Spawn { slug, .. }
        | Command::Start { slug, .. }
        | Command::Adopt { slug, .. }
        | Command::Log { slug, .. }
        | Command::Stop { slug } => Some(slug),
        _ => None,
    };
    if let Some(slug) = slug
        && !kallip_daemon_common::wire::valid_slug(slug)
    {
        anyhow::bail!("slug {slug:?} does not match [a-z0-9][a-z0-9-]* (max 64 chars)");
    }
    // Client-side advisory only: an explicitly pinned listen address
    // means the port in the reply is the actual bind result, not a
    // daemon-chosen ephemeral port.
    if let Command::Spawn { env, .. } | Command::Start { env, .. } = &cli.command
        && env
            .iter()
            .any(|pair| pair.starts_with("KALLIP_TAGMA_ADDR="))
    {
        eprintln!("listening address explicitly set; the reported port is the actual bind result");
    }
    let body = match cli.command {
        Command::Spawn {
            slug,
            workspace,
            env,
            user,
        } => RequestBody::Spawn {
            slug,
            workspace,
            env,
            exe: cli.bin,
            user,
        },
        Command::Stop { slug } => RequestBody::Stop { slug },
        Command::Remove { slug } => RequestBody::Remove { slug },
        Command::Start { slug, env } => RequestBody::Start {
            slug,
            env,
            exe: cli.bin,
        },
        Command::Adopt {
            slug,
            workspace,
            data_dir,
            env,
            user,
            accept_local_only,
        } => RequestBody::Adopt {
            slug,
            workspace,
            data_dir,
            env,
            user,
            accept_local_only,
        },
        Command::List => RequestBody::List,
        Command::Health { slug } => RequestBody::Health { slug },
        Command::Log {
            slug,
            lines,
            file,
            follow,
        } => {
            return run_log(&client, &slug, lines, file.as_deref(), follow).await;
        }
    };
    let response = client
        .call(body)
        .await
        .context("talking to the kallip daemon")?;
    print(response, started)
}

/// The one-line adopt outcome: verb, slug, and the observed state —
/// the registration fact a script greps for.
fn adopt_line(slug: &str, state: InstanceState) -> String {
    format!("adopted {slug} ({})", state.as_str())
}

/// Client-side mirrors of the daemon's line-count defaults: the CLI
/// always sends a concrete count; the daemon clamps its copy too.
const DEFAULT_LINES: u32 = 20;
const MAX_LINES: u32 = 1000;

/// The log verb's own loop: one request per beat (the wire has no
/// streaming), printing each increment. The first beat carries the
/// tail (the `--lines` first screen); later beats carry only what
/// the cursor has not seen. A rotated or emptied log set comes back
/// with a changed file name, a regressed offset, or no next cursor
/// at all — warn and adopt the daemon's rebased cursor instead of
/// silently stalling at a dead position.
async fn run_log(
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
            .context("talking to the kallip daemon")?;
        match response.body {
            ResponseBody::Ok {
                payload: OkPayload::Log { text, next_cursor },
            } => {
                let baseline_lost = match (&cursor, &next_cursor) {
                    (Some(sent), Some(next)) => next.file != sent.file || next.byte < sent.byte,
                    (Some(_), None) => true,
                    _ => false,
                };
                if baseline_lost {
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

/// Print a log chunk as-is with exactly one trailing newline.
fn print_log_text(text: &str) {
    if text.ends_with('\n') {
        print!("{text}");
    } else {
        println!("{text}");
    }
}

/// The stable error vocabulary scripts match on, shared by the
/// plain verbs and the log loop.
fn error_prefix(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::SlugTaken => "instance conflict",
        ErrorCode::Denied => "not authorized",
        ErrorCode::WorkspaceOverlap => "workspace overlaps an existing instance",
        ErrorCode::InvalidSpawnInput => "invalid spawn input",
        ErrorCode::SpawnTimeout => "spawn timed out (rolled back)",
        ErrorCode::NotFound => "no such instance",
        ErrorCode::NotRunning => "not running",
        ErrorCode::BadRequest => "bad request",
        ErrorCode::Internal => "internal error",
        ErrorCode::Unknown => "unknown error code from daemon",
    }
}

fn print(response: Response, started: bool) -> Result<()> {
    match response.body {
        ResponseBody::Ok { payload } => {
            match payload {
                OkPayload::Spawn { slug, pid, port } => {
                    let verb = if started { "started" } else { "spawned" };
                    println!("{verb} {slug} (pid {pid}, port {port})");
                }
                OkPayload::Stop { slug } => {
                    println!("stopped {slug}");
                }
                OkPayload::Remove { slug } => {
                    println!("removed {slug}");
                }
                OkPayload::List { instances } => {
                    if instances.is_empty() {
                        println!("no instances");
                        return Ok(());
                    }
                    // One labeled block per instance: every field is a full
                    // greppable line, so agents and scripts read records
                    // without parsing column alignment.
                    for (n, i) in instances.iter().enumerate() {
                        if n > 0 {
                            println!();
                        }
                        println!("{}", i.slug);
                        println!("  state: {}", i.state.as_str());
                        println!("  workspace: {}", i.workspace);
                        println!(
                            "  owner-uid: {}",
                            i.owner.map(|u| u.to_string()).unwrap_or_else(|| "-".into())
                        );
                        println!("  instance-id: {}", i.instance_id);
                    }
                }
                OkPayload::Health { report } => match (&report.slug, report.running) {
                    (None, _) => println!("daemon: healthy"),
                    (Some(slug), true) => println!("{slug}: running"),
                    (Some(slug), false) => {
                        println!(
                            "{slug}: not running ({})",
                            report.detail.unwrap_or_default()
                        );
                    }
                },
                OkPayload::Adopt { slug, state } => {
                    println!("{}", adopt_line(&slug, state));
                }
                OkPayload::Log { text, .. } => {
                    if text.is_empty() {
                        println!("(no logs)");
                    } else {
                        print_log_text(&text);
                    }
                }
            }
            Ok(())
        }
        ResponseBody::Err { code, message } => {
            // Errors exit non-zero with a human line; stable codes are the
            // machine interface for scripting.
            let prefix = error_prefix(code);
            anyhow::bail!("{prefix}: {message}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopt_line_renders_the_registration_fact() {
        assert_eq!(
            adopt_line("team-a", InstanceState::Running),
            "adopted team-a (running)"
        );
        assert_eq!(
            adopt_line("team-b", InstanceState::Stopped),
            "adopted team-b (stopped)"
        );
    }
}

#[test]
fn error_prefix_is_the_scripting_vocabulary() {
    assert_eq!(error_prefix(ErrorCode::NotFound), "no such instance");
    assert_eq!(error_prefix(ErrorCode::Denied), "not authorized");
}
