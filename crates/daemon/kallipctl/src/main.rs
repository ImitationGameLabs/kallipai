//! kallipctl: operator-side management CLI for the kallipai local daemon.
//!
//! Five verbs over the daemon's UDS protocol; the socket's 0600 mode is the
//! auth. Deliberately NOT part of the `kallip` command family: `kallip` is
//! the in-instance runtime, `kallipctl` manages instances from outside
//! (separate installation surfaces).

use anyhow::{Context as _, Result};
use clap::CommandFactory;
use clap::{Parser, Subcommand};
use clap_complete::Shell;
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use kallipai_common::authtoken::{OPERATOR_TOKEN_ENV_KEY, OPERATOR_TOKEN_FILE};
use kallipai_daemon_client::DaemonClient;
use kallipai_daemon_common::wire::{
    ErrorCode, InstanceState, LogCursor, OkPayload, RequestBody, Response, ResponseBody,
};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "kallipctl",
    about = "Manage local kallipai instances via the kallipai daemon",
    version
)]
struct Cli {
    /// Daemon control socket. When omitted the shared resolution chain
    /// is probed in order: $KALLIPAI_DAEMON_SOCKET, the runtime dir, the
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
        /// KALLIPAI_* keys plus RUST_LOG and PATH are accepted by the daemon.
        /// `KALLIPAI_TAGMA_ADDR=<addr>` pins the tagma's listen address
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
    Stop {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
    },
    /// Deregister an instance without touching its process: a
    /// running instance keeps running unmanaged — stop it first if
    /// you want it terminated. Idempotent.
    Remove {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
    },
    /// Relaunch a stopped or dead instance under its recorded workspace
    /// and env.
    Start {
        /// Instance slug.
        #[arg(add = ArgValueCompleter::new(complete_slug))]
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
    Health {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: Option<String>,
    },
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
    Restart {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
    },
    /// Tail an instance's or a polis service's logs. The argument
    /// picks the source: `logs <slug>` tails a managed instance
    /// through the daemon; `logs --service <name>` tails a polis
    /// service's rolling file on this host; with neither argument,
    /// list the services that have logs.
    Logs {
        /// Instance slug: same grammar as spawn's. Instance
        /// logs flow through the daemon.
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: Option<String>,
        /// The polis service to read on this host: its rolling
        /// file under `/var/log/kallipai/<service>`.
        #[arg(long, conflicts_with = "slug")]
        service: Option<String>,
        /// Lines from the tail (instance default 20, service
        /// default 50, capped at 1000).
        #[arg(short = 'n', long = "lines")]
        lines: Option<u32>,
        /// Read this single file from the instance's log
        /// directory instead of the merged tail. (Instance mode.)
        #[arg(long, conflicts_with_all = ["service", "level"], requires = "slug")]
        file: Option<String>,
        /// Keep polling for new lines (~500 ms beats) until Ctrl-C.
        #[arg(short = 'f', long = "follow")]
        follow: bool,
        /// Keep only lines carrying this level word.
        /// (Service mode.)
        #[arg(long, value_enum, conflicts_with = "slug", requires = "service")]
        level: Option<LogLevel>,
    },
    /// Directly rewrite a content-addressed blob store's on-disk
    /// representation (raw bytes -> zstd frames). Runs against the
    /// store on disk -- never through the daemon's wire protocol and
    /// never against the files metadata store.
    Blobs {
        #[command(subcommand)]
        command: BlobsCommand,
    },
    /// Manage an instance's operator token. One daemon query supplies the
    /// instance's real data dir (the daemon computed it at launch); the
    /// secret itself is then read and written on local disk and never
    /// crosses the wire.
    OperatorToken {
        #[command(subcommand)]
        command: OperatorTokenCommand,
    },
    /// Emit a shell completion script for this CLI to stdout. Hidden:
    /// an installer concern (nix postInstall, user dotfiles), not a
    /// daily verb.
    #[command(hide = true)]
    Generate {
        /// The shell to emit completions for.
        #[arg(value_enum)]
        shell: Shell,
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
        /// and tmp/). Defaults to env KALLIPAI_FILES_BLOB_ROOT. Only the
        /// files target reads this.
        #[arg(long)]
        blob_root: Option<String>,
        /// Raw objects whose stored size exceeds this many bytes stay
        /// raw. Defaults to env KALLIPAI_FILES_BLOB_COMPRESSION_ABOVE_BYTES
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
    /// Print the persisted pairs, one KEY=VALUE per line, no separator.
    /// Values are shown in full, so a line can be copied straight into
    /// a shell.
    List {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
    },
    /// Merge the given KEY=VALUE pairs into the persisted env: existing
    /// keys are replaced in place, new keys are appended (same allowlist
    /// as spawn's env). The running process is untouched; the change
    /// takes effect on next start.
    Set {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
        #[arg(value_name = "KEY=VALUE")]
        pairs: Vec<String>,
    },
    /// Remove keys from the persisted env. A key that is not present
    /// fails the whole request and is named in the error.
    Unset {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
        #[arg(value_name = "KEY")]
        keys: Vec<String>,
    },
}

#[derive(Subcommand)]
enum OperatorTokenCommand {
    /// Print the instance's operator token (the bare secret).
    Show {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
    },
    /// Store a chosen operator token for the instance. The value is read
    /// from stdin: pipe or redirect it for automation (docker login
    /// --password-stdin shape), or type it at the hidden prompt when
    /// stdin is a terminal — a flag or argument would leak the secret
    /// into shell history and process listings. Only the trailing
    /// newline is stripped; empty input is refused. Takes effect on the
    /// instance's next start.
    Set {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
    },
    /// Mint a fresh random operator token into the instance's token
    /// file and print it once (the one print the operator explicitly
    /// asked for). Takes effect on the instance's next start.
    Reset {
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
    },
}

fn main() -> Result<()> {
    // The dynamic-completion hook runs before any runtime is up: the
    // slug completer does synchronous daemon IPC, and a runtime nested
    // inside another would panic. With COMPLETE unset (every normal
    // invocation) this is a no-op.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run())
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Generate { shell } = cli.command {
        let mut cmd = Cli::command();
        clap_complete::generate(shell, &mut cmd, "kallipctl", &mut std::io::stdout());
        return Ok(());
    }
    // The shared chain, probed in order - the first socket that answers
    // is wherever the daemon actually bound (identical ordering on both
    // sides is what keeps client and daemon converged).
    let candidates = kallipai_daemon_common::socket::candidates_from_env(
        cli.socket.as_deref().map(std::path::Path::new),
    );
    let socket = kallipai_daemon_common::socket::probe(&candidates).map_err(|err| {
        anyhow::anyhow!(kallipai_daemon_common::socket::describe_probe_failure(&err))
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
        | Command::Logs {
            slug: Some(slug), ..
        }
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
        && !kallipai_daemon_common::wire::valid_slug(slug)
    {
        anyhow::bail!("slug {slug:?} does not match [a-z0-9][a-z0-9-]* (max 64 chars)");
    }
    // Client-side advisory only: an explicitly pinned listen address
    // means the port in the reply is the actual bind result, not a
    // daemon-chosen ephemeral port.
    if let Command::Spawn { env, .. } | Command::Start { env, .. } = &cli.command
        && env
            .iter()
            .any(|pair| pair.starts_with("KALLIPAI_TAGMA_ADDR="))
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
            service,
            lines,
            file,
            follow,
            level,
        } => {
            // The argument picks the source: an instance slug
            // rides the daemon wire; a polis service reads this
            // host's own /var/log/kallipai; neither just lists
            // the services. The client is lazy, so the local
            // arms never touch the socket.
            return match (slug, service) {
                (Some(slug), None) => {
                    run_logs(&client, &slug, lines, file.as_deref(), follow).await
                }
                (None, Some(service)) => run_service_logs(&service, lines, follow, level).await,
                (None, None) => run_service_log_list(),
                (Some(_), Some(_)) => {
                    unreachable!("clap enforces slug/--service exclusion")
                }
            };
        }
        Command::Restart { slug } => {
            return run_restart(&client, &slug).await;
        }
        Command::OperatorToken { command } => {
            // One daemon query resolves the instance's real data dir; the
            // secret itself then moves only between this process and disk.
            return run_operator_token(command, &client).await;
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
        // Unreachable: Generate early-returns before the connection is
        // set up; the arm only keeps this match exhaustive.
        Command::Generate { .. } => unreachable!("handled in main"),
    };
    let response = client
        .call(body)
        .await
        .context("talking to the kallipai daemon")?;
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
            .context("talking to the kallipai daemon")?;
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

/// Print a log chunk as-is with exactly one trailing newline.
fn print_log_text(text: &str) {
    if text.ends_with('\n') {
        print!("{text}");
    } else {
        println!("{text}");
    }
}

/// The level words a rendered tracing line carries: `--level` keeps
/// only the lines mentioning the chosen one.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum LogLevel {
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

/// The polis services' log root on a systemd host: each service owns
/// `<root>/<service>/` (systemd `LogsDirectory=kallipai/<service>`).
const SERVICE_LOG_ROOT: &str = "/var/log/kallipai";

/// Service mode's tail depth (the instance mode keeps its own 20).
const SERVICE_DEFAULT_LINES: u32 = 50;

/// The services that have a log directory under `root`, sorted. A
/// missing root is the container-deployment shape, said out loud.
fn list_service_dirs(root: &Path) -> Result<Vec<String>> {
    let mut services: Vec<String> = match std::fs::read_dir(root) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect(),
        Err(e) => anyhow::bail!(
            "no polis service log directory at {root:?} ({e}); container deployments \
             log inside the containers -- use `docker logs <container>` there"
        ),
    };
    services.sort();
    Ok(services)
}

/// The bare `logs` shape: name what is on this host and how to tail it.
fn run_service_log_list() -> Result<()> {
    let root = Path::new(SERVICE_LOG_ROOT);
    let services = list_service_dirs(root)?;
    if services.is_empty() {
        anyhow::bail!(
            "no service directories under {SERVICE_LOG_ROOT}; container deployments \
             log inside the containers -- use `docker logs <container>` there"
        );
    }
    println!("services with logs under {SERVICE_LOG_ROOT}:");
    for service in &services {
        println!("  {service}  ({SERVICE_LOG_ROOT}/{service})");
    }
    println!("tail one with `kallipctl logs --service <name>`");
    Ok(())
}

/// The retained rolling files for `service`, oldest first (the date in
/// `<service>.<date>.log` sorts the days). Nothing readable there is a
/// service that never logged on this host -- the container hint applies.
fn service_log_files(root: &Path, service: &str) -> Result<Vec<PathBuf>> {
    let dir = root.join(service);
    let files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect(),
        Err(e) => anyhow::bail!(
            "no log directory for service {service:?} at {dir:?} ({e}); is it running \
             on this host? container deployments use `docker logs <container>`"
        ),
    };
    let mut files = files;
    files.sort();
    if files.is_empty() {
        anyhow::bail!(
            "service {service:?} has no log files under {dir:?} yet; is it running \
             on this host? container deployments use `docker logs <container>`"
        );
    }
    Ok(files)
}

/// The last `n` lines across `files`, read oldest first so later files
/// continue the same line stream.
fn merged_tail(files: &[PathBuf], n: usize) -> std::io::Result<Vec<String>> {
    let mut lines = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(file)?;
        lines.extend(text.lines().map(str::to_string));
    }
    let start = lines.len().saturating_sub(n);
    Ok(lines.split_off(start))
}

/// Keep only the lines carrying `level`'s word.
fn keep_level(lines: &[String], level: Option<LogLevel>) -> Vec<String> {
    match level {
        None => lines.to_vec(),
        Some(level) => lines
            .iter()
            .filter(|line| line.contains(level.word()))
            .cloned()
            .collect(),
    }
}

/// Complete lines of `path` starting at `offset`, and the offset just
/// past the last newline seen (a trailing partial line waits for the
/// next beat). A file that shrank under the cursor re-baselines from
/// zero rather than failing.
fn lines_since(path: &Path, offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let offset = if len < offset { 0 } else { offset };
    file.seek(SeekFrom::Start(offset))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let consumed = text.rfind('\n').map_or(0, |pos| pos as u64 + 1);
    text.truncate(consumed as usize);
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    Ok((lines, offset + consumed))
}

/// Where a service-follow loop left off: the newest file it has seen
/// and how many bytes of it are already printed.
struct FollowState {
    file: PathBuf,
    offset: u64,
}

/// One follow beat: the lines that appeared since the last beat. A
/// rotated day (a file sorting after the recorded one) is finished off
/// and every newer day is taken whole; a recorded file that left the
/// retention window re-baselines on the newest file instead of
/// silently stalling at a dead position.
fn poll_service_increment(
    root: &Path,
    service: &str,
    state: &mut FollowState,
) -> Result<Vec<String>> {
    let files = service_log_files(root, service)?;
    let mut lines = Vec::new();
    match files.iter().position(|f| f == &state.file) {
        Some(i) => {
            let (rest, offset) = lines_since(&state.file, state.offset)?;
            lines.extend(rest);
            state.offset = offset;
            for file in &files[i + 1..] {
                let (fresh, offset) = lines_since(file, 0)?;
                lines.extend(fresh);
                state.file = file.clone();
                state.offset = offset;
            }
        }
        None => {
            let newest = files
                .last()
                .expect("service_log_files returns a non-empty set");
            let (fresh, offset) = lines_since(newest, 0)?;
            lines.extend(fresh);
            state.file = newest.clone();
            state.offset = offset;
            eprintln!("the log file changed under the follow cursor; re-baselined");
        }
    }
    Ok(lines)
}

/// The service-logs loop: one merged tail (the `--lines` first
/// screen), then a beat per ~500 ms when following. Level filtering
/// applies to the tail and to every beat alike.
async fn run_service_logs(
    service: &str,
    lines: Option<u32>,
    follow: bool,
    level: Option<LogLevel>,
) -> Result<()> {
    let root = Path::new(SERVICE_LOG_ROOT);
    let files = service_log_files(root, service)?;
    let n = lines.unwrap_or(SERVICE_DEFAULT_LINES).min(MAX_LINES) as usize;
    let tail = keep_level(&merged_tail(&files, n)?, level);
    if tail.is_empty() && !follow {
        println!("(no matching lines)");
    }
    for line in &tail {
        println!("{line}");
    }
    if !follow {
        return Ok(());
    }
    let latest = files
        .last()
        .expect("service_log_files returns a non-empty set");
    let offset = std::fs::metadata(latest).map(|m| m.len()).unwrap_or(0);
    let mut state = FollowState {
        file: latest.clone(),
        offset,
    };
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let fresh = keep_level(&poll_service_increment(root, service, &mut state)?, level);
        for line in &fresh {
            println!("{line}");
        }
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
                    // One KEY=VALUE line per variable, no separator:
                    // each line copies straight into a shell.
                    for pair in &env {
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

/// The instance's token file path: one daemon query supplies the data
/// dir the daemon computed at launch (in-place and drop-to launches
/// differ, so a local guess would miss drop-to instances). Only the
/// path crosses the wire; the secret is read and written locally.
async fn operator_token_path(client: &DaemonClient, slug: &str) -> Result<std::path::PathBuf> {
    let response = client
        .call(RequestBody::Record {
            slug: slug.to_owned(),
        })
        .await
        .context("talking to the kallipai daemon")?;
    let data_dir = match response.body {
        ResponseBody::Ok {
            payload: OkPayload::Record { data_dir, .. },
        } => data_dir,
        ResponseBody::Err { message, .. } => {
            anyhow::bail!("cannot resolve {slug}: {message}");
        }
        _ => anyhow::bail!("unexpected daemon response while resolving {slug}"),
    };
    Ok(data_dir
        .join("credentials")
        .join(kallipai_common::authtoken::OPERATOR_TOKEN_FILE))
}

/// Read and parse the token file: the bare secret from the
/// `KALLIPAI_OPERATOR_TOKEN=` line. Error messages are operator-facing
/// (they land on the CLI's `error:` line); the ENOENT one names the
/// pinned possibility, the EACCES one the owner-or-sudo requirement.
fn read_operator_token_file(path: &std::path::Path) -> Result<String> {
    let raw = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow::anyhow!("no operator token file at {}; the instance may not have run yet, or the token is pinned via {OPERATOR_TOKEN_ENV_KEY}", path.display()),
        std::io::ErrorKind::PermissionDenied => anyhow::anyhow!("permission denied reading {}; run as root (sudo) or the instance's owner user", path.display()),
        _ => anyhow::anyhow!("reading {}: {e}", path.display()),
    })?;
    let prefix = format!("{OPERATOR_TOKEN_ENV_KEY}=");
    raw.lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} carries no {OPERATOR_TOKEN_ENV_KEY} value",
                path.display()
            )
        })
}

/// Strip exactly one trailing newline (LF, or CRLF) — what a pipe, a
/// redirect, or an Enter keypress adds. Every other byte stays verbatim,
/// matching how the env pin keeps the operator's value untouched (the
/// tool never silently rewrites the value).
fn strip_trailing_newline(mut raw: String) -> String {
    if raw.ends_with('\n') {
        raw.pop();
    }
    if raw.ends_with('\r') {
        raw.pop();
    }
    raw
}

/// Terminal echo disabled for the enclosing scope (the hidden-input
/// prompt). The original termios is parked in [`SAVED_TERMIOS`] so the
/// signal watcher can restore it even though Ctrl-C bypasses `Drop`;
/// `Drop` still covers every ordinary exit, including error unwinding.
struct EchoOff(libc::termios);

/// The terminal state captured while a hidden read is in flight.
static SAVED_TERMIOS: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);

impl EchoOff {
    fn new() -> Result<Self> {
        // SAFETY: tcgetattr on our own stdin fd; libc fills the struct
        // completely on success.
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
            anyhow::bail!("could not read terminal settings");
        }
        let original = term;
        term.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) } != 0 {
            anyhow::bail!("could not disable terminal echo");
        }
        if let Ok(mut slot) = SAVED_TERMIOS.lock() {
            *slot = Some(original);
        }
        Ok(EchoOff(original))
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restoring the settings captured at construction.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
        if let Ok(mut slot) = SAVED_TERMIOS.lock() {
            *slot = None;
        }
    }
}

/// Parked beside the hidden read: SIGINT/SIGTERM bypass `Drop`, so the
/// watcher restores the terminal echo first and only then exits with the
/// signal's conventional status. Runs as a normal task, so the plain
/// Mutex access here is sound.
async fn restore_echo_on_signal() -> ! {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM watcher");
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT watcher");
    tokio::select! {
        _ = terminate.recv() => restore_echo_and_exit(143),
        _ = interrupt.recv() => restore_echo_and_exit(130),
    }
}

fn restore_echo_and_exit(status: i32) -> ! {
    if let Ok(Some(term)) = SAVED_TERMIOS.lock().map(|mut slot| slot.take()) {
        // SAFETY: restoring the terminal state captured at prompt start.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) };
    }
    std::process::exit(status)
}

/// Read the token value from stdin: a hidden prompt when stdin is a
/// terminal (the operator types it; echo stays off), a plain line when
/// piped or redirected (automation). Empty or whitespace-only input is
/// refused before anything is written.
async fn read_token_value() -> Result<String> {
    let hidden = unsafe { libc::isatty(0) } == 1;
    let guard = if hidden {
        eprint!("Enter operator token (input hidden): ");
        use std::io::Write;
        std::io::stderr().flush()?;
        Some(EchoOff::new()?)
    } else {
        None
    };
    // The watcher loses the race only when the read returns; while the
    // read blocks it is the one that answers a signal.
    let watcher = tokio::spawn(restore_echo_on_signal());
    let raw = tokio::task::spawn_blocking(move || {
        let mut raw = String::new();
        std::io::stdin()
            .read_line(&mut raw)
            .context("reading from stdin")?;
        Ok::<String, anyhow::Error>(raw)
    })
    .await
    .context("stdin read task")??;
    watcher.abort();
    drop(guard);
    if hidden {
        eprintln!();
    }
    let value = strip_trailing_newline(raw);
    if value.trim().is_empty() {
        anyhow::bail!("no token value on stdin (empty input refused; nothing written)");
    }
    Ok(value)
}

/// Persist the value: credentials dir created owner-only when missing,
/// then temp-write + chmod 0600 + rename (atomic; the secret is never
/// world-readable). Same mechanics as the tagma's boot-time writer.
fn write_operator_token_file(credentials_dir: &std::path::Path, secret: &str) -> Result<()> {
    std::fs::create_dir_all(credentials_dir)
        .with_context(|| format!("creating {}", credentials_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(credentials_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", credentials_dir.display()))?;
    }
    let path = credentials_dir.join(OPERATOR_TOKEN_FILE);
    let body = format!("{OPERATOR_TOKEN_ENV_KEY}={secret}\n");
    let tmp = credentials_dir.join(format!(".{OPERATOR_TOKEN_FILE}.tmp"));
    std::fs::write(&tmp, body).context("write operator token temp file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .context("chmod operator token file")?;
    }
    std::fs::rename(&tmp, &path).context("rename operator token file into place")?;
    Ok(())
}

async fn run_operator_token(command: OperatorTokenCommand, client: &DaemonClient) -> Result<()> {
    match command {
        OperatorTokenCommand::Show { slug } => {
            let path = operator_token_path(client, &slug).await?;
            println!("{}", read_operator_token_file(&path)?);
        }
        OperatorTokenCommand::Set { slug } => {
            let value = read_token_value().await?;
            write_for_slug(client, &slug, &value).await?;
            eprintln!(
                "operator token written; it takes effect on next start (kallipctl restart {slug})"
            );
            eprintln!(
                "note: an instance started with {OPERATOR_TOKEN_ENV_KEY} set ignores this file"
            );
        }
        OperatorTokenCommand::Reset { slug } => {
            // The operator-prefix literal mirrors kallipai-tagma's
            // `token::OPERATOR` (each crate pins its own prefixes — see
            // kallipai-common::authtoken's module doc); tagma itself is
            // deliberately not a kallipctl dependency.
            const OPERATOR_PREFIX: kallipai_common::authtoken::TokenKind =
                kallipai_common::authtoken::TokenKind("sk-operator-");
            let secret = kallipai_common::authtoken::MintedToken::generate(OPERATOR_PREFIX);
            write_for_slug(client, &slug, secret.secret()).await?;
            // The one print the operator explicitly asked for (the admin
            // token reset contract).
            println!("{}", secret.secret());
            eprintln!(
                "fresh operator token written; it takes effect on next start (kallipctl restart {slug})"
            );
            eprintln!(
                "note: an instance started with {OPERATOR_TOKEN_ENV_KEY} set ignores this file"
            );
        }
    }
    Ok(())
}

/// Resolve the slug's credentials dir and persist the value there.
async fn write_for_slug(client: &DaemonClient, slug: &str, secret: &str) -> Result<()> {
    let path = operator_token_path(client, slug).await?;
    let dir = path
        .parent()
        .expect("the token path always has a credentials parent")
        .to_path_buf();
    write_operator_token_file(&dir, secret)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_newline_stripped_lf_crlf_and_none() {
        assert_eq!(strip_trailing_newline("sk-x\n".into()), "sk-x");
        assert_eq!(strip_trailing_newline("sk-x\r\n".into()), "sk-x");
        assert_eq!(strip_trailing_newline("sk-x".into()), "sk-x");
        // Interior whitespace is preserved verbatim (the tool never
        // rewrites the operator's value).
        assert_eq!(strip_trailing_newline("sk-x  \n".into()), "sk-x  ");
    }

    #[test]
    fn token_file_write_read_roundtrip_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("credentials");
        write_operator_token_file(&credentials, "sk-operator-static").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(credentials.join(OPERATOR_TOKEN_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let secret = read_operator_token_file(&credentials.join(OPERATOR_TOKEN_FILE)).unwrap();
        assert_eq!(secret, "sk-operator-static");
    }

    #[test]
    fn missing_and_valueless_token_files_get_operator_facing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.env");
        let err = read_operator_token_file(&path).unwrap_err().to_string();
        assert!(err.contains("no operator token file"), "{err}");
        assert!(err.contains("pinned"), "{err}");
        std::fs::write(&path, "OTHER=x\n").unwrap();
        let err = read_operator_token_file(&path).unwrap_err().to_string();
        assert!(err.contains("carries no KALLIPAI_OPERATOR_TOKEN"), "{err}");
    }

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

/// Dynamic completion for slug arguments: list the daemon's managed
/// instances and offer their slugs. Every failure path (no reachable
/// socket, IPC error) degrades to an empty candidate set: a completion
/// hook must never print an error into the shell's input line.
fn complete_slug(current: &OsStr) -> Vec<CompletionCandidate> {
    use kallipai_daemon_common::wire::RequestBody;
    use kallipai_daemon_common::wire::ResponseBody;

    // The hook runs before any tokio runtime is up, so a throwaway
    // current-thread runtime drives the async UDS client here.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return Vec::new(),
    };
    let prefix = current.to_string_lossy();
    rt.block_on(async move {
        let candidates = kallipai_daemon_common::socket::candidates_from_env(None);
        let socket = match kallipai_daemon_common::socket::probe(&candidates) {
            Ok(socket) => socket,
            Err(_) => return Vec::new(),
        };
        let client = DaemonClient::new(socket);
        let response = match client.call(RequestBody::List).await {
            Ok(response) => response,
            Err(_) => return Vec::new(),
        };
        let ResponseBody::Ok {
            payload: kallipai_daemon_common::wire::OkPayload::List { instances },
        } = response.body
        else {
            return Vec::new();
        };
        instances
            .into_iter()
            .filter(|instance| instance.slug.starts_with(prefix.as_ref()))
            .map(|instance| {
                CompletionCandidate::new(instance.slug).help(Some(instance.state.as_str().into()))
            })
            .collect()
    })
}

#[cfg(test)]
mod service_logs_tests {
    use super::*;

    fn fixture_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("kallipai");
        std::fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    fn write_log(root: &Path, service: &str, name: &str, text: &str) {
        let dir = root.join(service);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn list_service_dirs_lists_directories_sorted() {
        let (_guard, root) = fixture_root();
        write_log(&root, "lesche", "lesche.2026-09-26.log", "b\n");
        write_log(&root, "archeion", "archeion.2026-09-26.log", "a\n");
        std::fs::write(root.join("stray-file"), b"").unwrap();
        let services = list_service_dirs(&root).unwrap();
        assert_eq!(services, vec!["archeion".to_string(), "lesche".to_string()]);
    }

    #[test]
    fn list_service_dirs_missing_root_names_the_container_path() {
        let missing = Path::new("/nonexistent-kallipai-logs");
        let err = list_service_dirs(missing).unwrap_err().to_string();
        assert!(err.contains("docker logs"), "hint missing: {err}");
        assert!(
            err.contains("/nonexistent-kallipai-logs"),
            "path missing: {err}"
        );
    }

    #[test]
    fn service_log_files_orders_days_oldest_first() {
        let (_guard, root) = fixture_root();
        write_log(&root, "archeion", "archeion.2026-09-27.log", "new\n");
        write_log(&root, "archeion", "archeion.2026-09-26.log", "old\n");
        let files = service_log_files(&root, "archeion").unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["archeion.2026-09-26.log", "archeion.2026-09-27.log"]
        );
    }

    #[test]
    fn service_log_files_missing_dir_says_the_container_hint() {
        let (_guard, root) = fixture_root();
        let err = service_log_files(&root, "archeion")
            .unwrap_err()
            .to_string();
        assert!(err.contains("docker logs"), "hint missing: {err}");
    }

    #[test]
    fn service_log_files_empty_dir_says_no_files_yet() {
        let (_guard, root) = fixture_root();
        std::fs::create_dir_all(root.join("archeion")).unwrap();
        let err = service_log_files(&root, "archeion")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no log files"), "shape missing: {err}");
    }

    #[test]
    fn merged_tail_spans_files_oldest_to_newest() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("a.2026-09-26.log");
        let new = dir.path().join("a.2026-09-27.log");
        std::fs::write(&old, "1\n2\n3\n").unwrap();
        std::fs::write(&new, "4\n5\n").unwrap();
        let tail = merged_tail(&[old, new], 3).unwrap();
        assert_eq!(
            tail,
            vec!["3".to_string(), "4".to_string(), "5".to_string()]
        );
    }

    #[test]
    fn merged_tail_larger_than_total_returns_everything() {
        let dir = tempfile::tempdir().unwrap();
        let only = dir.path().join("a.2026-09-26.log");
        std::fs::write(&only, "1\n2\n").unwrap();
        let tail = merged_tail(&[only], 50).unwrap();
        assert_eq!(tail, vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn keep_level_filters_on_the_level_word() {
        let lines = vec![
            "2026-09-27T10:00:00Z  INFO ready".to_string(),
            "2026-09-27T10:00:01Z ERROR boom".to_string(),
            "2026-09-27T10:00:02Z  WARN careful".to_string(),
        ];
        assert_eq!(keep_level(&lines, None).len(), 3);
        let errors = keep_level(&lines, Some(LogLevel::Error));
        assert_eq!(errors, vec!["2026-09-27T10:00:01Z ERROR boom".to_string()]);
        assert_eq!(keep_level(&lines, Some(LogLevel::Debug)).len(), 0);
    }

    #[test]
    fn lines_since_holds_back_a_partial_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.2026-09-26.log");
        std::fs::write(&path, "one\ntwo").unwrap();
        let (lines, offset) = lines_since(&path, 0).unwrap();
        assert_eq!(lines, vec!["one".to_string()]);
        assert_eq!(offset, 4);
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let (lines, offset) = lines_since(&path, offset).unwrap();
        assert_eq!(lines, vec!["two".to_string(), "three".to_string()]);
        assert_eq!(offset, 14);
    }

    #[test]
    fn lines_since_rebaselines_a_shrunk_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.2026-09-26.log");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let (_, offset) = lines_since(&path, 0).unwrap();
        std::fs::write(&path, "fresh\n").unwrap();
        let (lines, offset) = lines_since(&path, offset).unwrap();
        assert_eq!(lines, vec!["fresh".to_string()]);
        assert_eq!(offset, 6);
    }

    #[test]
    fn poll_follows_the_same_day_and_then_the_rotation() {
        let dir_guard = tempfile::tempdir().unwrap();
        let root = dir_guard.path().join("kallipai");
        write_log(&root, "archeion", "archeion.2026-09-26.log", "d1a\nd1b\n");
        let files = service_log_files(&root, "archeion").unwrap();
        let latest = files.last().unwrap().clone();
        let offset = std::fs::metadata(&latest).unwrap().len();
        let mut state = FollowState {
            file: latest,
            offset,
        };

        std::fs::write(
            root.join("archeion/archeion.2026-09-26.log"),
            "d1a\nd1b\nd1c\n",
        )
        .unwrap();
        let fresh = poll_service_increment(&root, "archeion", &mut state).unwrap();
        assert_eq!(fresh, vec!["d1c".to_string()]);

        write_log(&root, "archeion", "archeion.2026-09-27.log", "d2a\n");
        let fresh = poll_service_increment(&root, "archeion", &mut state).unwrap();
        assert_eq!(fresh, vec!["d2a".to_string()]);
        assert!(state.file.ends_with("archeion.2026-09-27.log"));
    }

    #[test]
    fn poll_rebaselines_when_the_recorded_file_leaves_the_set() {
        let dir_guard = tempfile::tempdir().unwrap();
        let root = dir_guard.path().join("kallipai");
        write_log(&root, "archeion", "archeion.2026-09-26.log", "old\n");
        let files = service_log_files(&root, "archeion").unwrap();
        let mut state = FollowState {
            file: files[0].clone(),
            offset: 0,
        };
        std::fs::remove_file(&state.file).unwrap();
        write_log(&root, "archeion", "archeion.2026-09-27.log", "new\n");
        let fresh = poll_service_increment(&root, "archeion", &mut state).unwrap();
        assert_eq!(fresh, vec!["new".to_string()]);
        assert!(state.file.ends_with("archeion.2026-09-27.log"));
    }

    #[test]
    fn poll_crosses_multiple_new_files_in_one_beat() {
        let dir_guard = tempfile::tempdir().unwrap();
        let root = dir_guard.path().join("kallipai");
        write_log(&root, "archeion", "archeion.2026-09-25.log", "d0\n");
        let files = service_log_files(&root, "archeion").unwrap();
        let latest = files.last().unwrap().clone();
        let offset = std::fs::metadata(&latest).unwrap().len();
        let mut state = FollowState {
            file: latest,
            offset,
        };

        // Two rotations land between two beats: the beat finishes the
        // recorded day and carries both newer days in order.
        write_log(&root, "archeion", "archeion.2026-09-26.log", "d1a\nd1b\n");
        write_log(&root, "archeion", "archeion.2026-09-27.log", "d2a\n");
        let fresh = poll_service_increment(&root, "archeion", &mut state).unwrap();
        assert_eq!(
            fresh,
            vec!["d1a".to_string(), "d1b".to_string(), "d2a".to_string()]
        );
        assert!(state.file.ends_with("archeion.2026-09-27.log"));
    }

    #[test]
    fn logs_flag_sources_are_exclusive_and_bound_to_a_source() {
        // --file and --level without any source stay rejected: the
        // flags lock to their arm instead of falling through to the
        // listing shape.
        assert!(
            Cli::try_parse_from(["kallipctl", "logs", "--file", "x", "--level", "error",]).is_err()
        );
        // --file belongs to the instance arm.
        assert!(Cli::try_parse_from(["kallipctl", "logs", "myinstance", "--file", "x",]).is_ok());
        // --level belongs to the service arm.
        assert!(
            Cli::try_parse_from([
                "kallipctl",
                "logs",
                "--service",
                "archeion",
                "--level",
                "error",
            ])
            .is_ok()
        );
    }
}
