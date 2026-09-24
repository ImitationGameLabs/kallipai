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
    /// Read and write an instance's persisted configuration (the env
    /// pairs today; more surfaces can join this verb family later).
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Stop the instance if it is running, then start it under the
    /// record's latest env: an already-stopped instance simply starts.
    /// The systemd restart shape — the end state is one process running
    /// the newest recorded configuration.
    Restart { slug: String },
    /// Tail an instance's log files (read-only diagnostic): the
    /// merged tail across retained daily files, one file with
    /// --file, or live with --follow.
    Logs {
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
    /// Directly rewrite a content-addressed blob store's on-disk
    /// representation (raw bytes -> zstd frames). Runs against the
    /// store on disk -- never through the daemon's wire protocol and
    /// never against the files metadata store.
    Blobs {
        #[command(subcommand)]
        command: BlobsCommand,
    },
}

#[derive(Subcommand)]
enum BlobsCommand {
    /// Re-encode every raw object of one store as a zstd frame. All
    /// targets walk the store on disk (plain, catalog-free), verify
    /// bytes against addresses before rewriting, and commit by atomic
    /// rename; re-runs are safe. A running owner is a warning, not a
    /// refusal -- but stopping first is still the polite form.
    Rewrite {
        /// Which store to rewrite.
        #[arg(long, value_enum, default_value_t = RewriteTarget::Tasks)]
        target: RewriteTarget,
        /// The instance owning the tasks/attachments blob store: its
        /// daemon record supplies the data directory and the running
        /// state for the warning. (The files target never reads this.)
        #[arg(
            long,
            required_if_eq("target", "tasks"),
            required_if_eq("target", "attachments")
        )]
        slug: Option<String>,
        /// The files blob store root (the directory containing blobs/
        /// and tmp/). Defaults to env KALLIP_FILES_BLOB_ROOT. Only the
        /// files target reads this.
        #[arg(long)]
        blob_root: Option<String>,
        /// Raw objects whose stored size exceeds this many bytes stay
        /// raw. Defaults to env KALLIP_FILES_BLOB_COMPRESSION_ABOVE_BYTES
        /// (8 MiB) for the files target; 0 = no limit.
        #[arg(long)]
        above_bytes: Option<u64>,
        /// Report what would be rewritten without touching anything.
        #[arg(long)]
        dry_run: bool,
        /// Leave corrupt blobs (bytes that no longer hash to their
        /// address) untouched and count them, instead of failing the run.
        #[arg(long)]
        skip_corrupt: bool,
        /// Print a cumulative progress line to stderr after this many
        /// objects (0 = quiet until the summary; 1 = every object).
        #[arg(long, default_value_t = 0)]
        progress_every: usize,
        /// zstd level for newly encoded frames (default 3).
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(i32).range(1..=22))]
        level: i32,
    },
}

/// The store a rewrite pass targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum RewriteTarget {
    /// The daemon task-archive blobs (`<data_dir>/blobs/tasks`).
    Tasks,
    /// The attachment-mirror blobs (`<data_dir>/blobs/attachments`).
    Attachments,
    /// The files service's content store.
    Files,
}

impl RewriteTarget {
    /// The blob store directory name under a data dir (the two daemon
    /// stores only; the files target resolves its root from --blob-root
    /// or the environment and never calls this).
    fn dir_name(self) -> &'static str {
        match self {
            RewriteTarget::Tasks => "tasks",
            RewriteTarget::Attachments => "attachments",
            RewriteTarget::Files => unreachable!("the files target has its own root"),
        }
    }
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// The instance's persistent env: the pairs a later start replays.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
}

#[derive(Subcommand)]
enum EnvCommand {
    /// Print the persisted pairs, one KEY=VALUE per line with a blank
    /// line between variables. Values are shown in full, so a line can
    /// be copied straight into a shell.
    List { slug: String },
    /// Merge the given KEY=VALUE pairs into the persisted env: existing
    /// keys are replaced in place, new keys are appended (same allowlist
    /// as spawn's env). The running process is untouched; the change
    /// takes effect on next start.
    Set {
        slug: String,
        #[arg(value_name = "KEY=VALUE")]
        pairs: Vec<String>,
    },
    /// Remove keys from the persisted env. A key that is not present
    /// fails the whole request and is named in the error.
    Unset {
        slug: String,
        #[arg(value_name = "KEY")]
        keys: Vec<String>,
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
        | Command::Logs { slug, .. }
        | Command::Stop { slug } => Some(slug),
        Command::Restart { slug } => Some(slug),
        Command::Blobs {
            command: BlobsCommand::Rewrite { target, slug, .. },
        } => match target {
            RewriteTarget::Tasks | RewriteTarget::Attachments => slug.as_ref(),
            RewriteTarget::Files => None,
        },
        Command::Config {
            command:
                ConfigCommand::Env {
                    command:
                        EnvCommand::List { slug }
                        | EnvCommand::Set { slug, .. }
                        | EnvCommand::Unset { slug, .. },
                },
        } => Some(slug),
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
        Command::Logs {
            slug,
            lines,
            file,
            follow,
        } => {
            return run_logs(&client, &slug, lines, file.as_deref(), follow).await;
        }
        Command::Restart { slug } => {
            return run_restart(&client, &slug).await;
        }
        Command::Blobs {
            command:
                BlobsCommand::Rewrite {
                    target,
                    slug,
                    blob_root,
                    above_bytes,
                    dry_run,
                    skip_corrupt,
                    progress_every,
                    level,
                },
        } => {
            return run_blobs_rewrite(
                RewriteInvocation {
                    target,
                    slug,
                    blob_root,
                    above_bytes,
                    dry_run,
                    skip_corrupt,
                    progress_every,
                    level,
                },
                &client,
            )
            .await;
        }
        Command::Config {
            command: ConfigCommand::Env { command },
        } => match command {
            EnvCommand::List { slug } => RequestBody::EnvGet { slug },
            EnvCommand::Set { slug, pairs } => RequestBody::EnvSet { slug, env: pairs },
            EnvCommand::Unset { slug, keys } => RequestBody::EnvUnset { slug, keys },
        },
    };
    let response = client
        .call(body)
        .await
        .context("talking to the kallip daemon")?;
    print(response, started)
}

/// One fully-parsed rewrite invocation, handed over by the CLI
/// dispatch.
struct RewriteInvocation {
    target: RewriteTarget,
    slug: Option<String>,
    blob_root: Option<String>,
    above_bytes: Option<u64>,
    dry_run: bool,
    skip_corrupt: bool,
    progress_every: usize,
    level: i32,
}

/// The blobs rewrite verb: plain on-disk work under the same
/// Result-based flow the wire verbs use, against whichever store the
/// target names.
async fn run_blobs_rewrite(args: RewriteInvocation, client: &DaemonClient) -> Result<()> {
    let corrupt = if args.skip_corrupt {
        kallip_blob_store::CorruptPolicy::Skip
    } else {
        kallip_blob_store::CorruptPolicy::Fail
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
                .context("talking to the kallip daemon")?;
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
            let addr =
                std::env::var("KALLIP_FILES_ADDR").unwrap_or_else(|_| "127.0.0.1:7400".to_owned());
            if let Some(detail) = files_service_answers(&addr).await {
                eprintln!("warning: the files service answers on {addr} ({detail})");
            }
            args.blob_root
                .or_else(|| std::env::var("KALLIP_FILES_BLOB_ROOT").ok())
                .context("--blob-root or KALLIP_FILES_BLOB_ROOT is required")?
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
                match std::env::var("KALLIP_FILES_BLOB_COMPRESSION_ABOVE_BYTES") {
                    Ok(v) => v
                        .parse::<u64>()
                        .context("KALLIP_FILES_BLOB_COMPRESSION_ABOVE_BYTES is not a number")?,
                    Err(_) => 8 * 1024 * 1024,
                }
            }
            // The daemon stores' writers have no threshold at all.
            _ => 0,
        },
    };
    let report = kallip_blob_store::rewrite_root(
        &root,
        &kallip_blob_store::RewriteOptions {
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
fn print_rewrite_summary(report: &kallip_blob_store::RewriteReport) {
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

/// Probe the files service's unauthenticated /health: `Some(detail)`
/// means something answered (the warning fires), `None` means nothing
/// is listening on the service address.
async fn files_service_answers(addr: &str) -> Option<String> {
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

/// The one-line adopt outcome: verb, slug, and the observed state —
/// the registration fact a script greps for.
fn adopt_line(slug: &str, state: InstanceState) -> String {
    format!("adopted {slug} ({})", state.as_str())
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
async fn run_logs(
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

/// The restart orchestration: stop if running, then start. The daemon
/// has no restart verb — the CLI composes the two, exactly the shape
/// systemd uses for `restart` on a plain service: an already-stopped
/// instance skips the stop (NotRunning is the expected outcome, not
/// an error), and the start replays the record's latest env. The end
/// state is one process running the newest recorded configuration.
async fn run_restart(client: &DaemonClient, slug: &str) -> Result<()> {
    let stop = client
        .call(RequestBody::Stop {
            slug: slug.to_owned(),
        })
        .await
        .context("talking to the kallip daemon")?;
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
        .context("talking to the kallip daemon")?;
    print(start, true)
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
/// plain verbs and the logs loop.
fn error_prefix(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::SlugTaken => "instance conflict",
        ErrorCode::Denied => "not authorized",
        ErrorCode::WorkspaceOverlap => "instance path overlap",
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
                OkPayload::EnvGet { slug, env } => {
                    if env.is_empty() {
                        println!("{slug}: no persisted env");
                        return Ok(());
                    }
                    // One KEY=VALUE line per variable, blank line between
                    // variables: each line copies straight into a shell,
                    // and the blanks keep multi-variable dumps readable
                    // without wrapping long values by hand.
                    for (n, pair) in env.iter().enumerate() {
                        if n > 0 {
                            println!();
                        }
                        println!("{pair}");
                    }
                }
                OkPayload::EnvSet { slug } => {
                    println!("updated {slug} env; takes effect on next start");
                }
                OkPayload::EnvUnset { slug, removed } => {
                    let listed = removed.join(", ");
                    println!("removed {listed} from {slug} env; takes effect on next start");
                }
                OkPayload::Record { data_dir, state } => {
                    println!("{}", data_dir.display());
                    println!("state: {}", state.as_str());
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
    assert_eq!(
        error_prefix(ErrorCode::WorkspaceOverlap),
        "instance path overlap"
    );
}
