//! Command-line surface: the clap parser tree and its subcommand enums.

use crate::completion::complete_slug;
use clap::{Parser, Subcommand};
use clap_complete::Shell;
use clap_complete::engine::ArgValueCompleter;

#[derive(Parser)]
#[command(
    name = "kallipctl",
    about = "Manage local kallipai instances via the kallipai daemon",
    version
)]
pub(crate) struct Cli {
    /// Daemon control socket. When omitted the shared resolution chain
    /// is probed in order: $KALLIPAI_DAEMON_SOCKET, the runtime dir, the
    /// state home default - the first socket that answers wins.
    #[arg(long, global = true)]
    pub(crate) socket: Option<String>,

    /// Dev-only: run this explicit tagma binary in the launched instance
    /// instead of the daemon's resolved one (dev loops testing a fresh
    /// build against a running daemon).
    #[arg(long, global = true)]
    pub(crate) bin: Option<String>,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Launch a new instance under a slug.
    Spawn {
        /// Instance slug: lowercase letters, digits, and '-'; must start
        /// with a letter or digit.
        slug: String,
        /// Path of the instance workspace; a relative path is resolved
        /// against the current directory before it reaches the daemon.
        workspace: String,
        /// Extra env for the instance, KEY=VALUE (repeatable); only
        /// KALLIPAI_* keys plus RUST_LOG and PATH are accepted by the daemon.
        /// `KALLIPAI_TAGMA_ADDR=<addr>` pins the tagma's listen address
        /// (default 127.0.0.1:0); prefer a concrete interface over
        /// 0.0.0.0 — the API is Bearer-gated but plain HTTP on the LAN.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Run the instance as this pre-declared system user (the
        /// dedicated-user form; requires the daemon to run as root).
        #[arg(long)]
        user: Option<String>,
        /// Platform edge origin: the value instances receive as
        /// KALLIPAI_POLIS_URL, not a service's bare address. Setting it mints
        /// a single-use enrollment code through {origin}/v1/archeion with the
        /// access token, then launches the instance with this origin in its env.
        #[arg(long, value_name = "URL")]
        polis: Option<String>,
        /// File carrying the platform access token the mint authenticates
        /// with; it must hold archeion:tagma:write. Defaults to
        /// $XDG_CONFIG_HOME/kallipai/access-token.
        #[arg(long, value_name = "PATH")]
        token_file: Option<String>,
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
    /// Tail a managed instance's logs.
    Logs {
        /// Instance slug: same grammar as spawn's. Instance
        /// logs flow through the daemon.
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
        /// Lines from the tail (default 20, capped at 1000).
        #[arg(short = 'n', long = "lines")]
        lines: Option<u32>,
        /// Read this single file from the instance's log
        /// directory instead of the merged tail.
        #[arg(long)]
        file: Option<String>,
        /// Keep polling for new lines (~500 ms beats) until Ctrl-C.
        #[arg(short = 'f', long = "follow")]
        follow: bool,
    },
    /// Rewrite an instance's blob store on disk: raw bytes become zstd
    /// frames. The store's directory comes from the instance record, so
    /// the daemon must be reachable; the rewrite itself only touches disk.
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
pub(crate) enum BlobsCommand {
    /// Re-encode every raw object of one store as a zstd frame. All
    /// targets walk the store on disk (plain, catalog-free), verify
    /// bytes against addresses before rewriting, and commit by atomic
    /// rename; re-runs are safe. A running owner is a warning, not a
    /// refusal -- but stopping first is still the polite form.
    Rewrite {
        /// Which store to rewrite.
        #[arg(long, value_enum, default_value_t = RewriteTarget::Tasks)]
        target: RewriteTarget,
        /// The instance owning the store: its daemon record supplies
        /// the data directory and the running state for the warning.
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
        /// Raw objects whose stored size exceeds this many bytes stay
        /// raw; 0 = no limit.
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
pub(crate) enum RewriteTarget {
    /// The daemon task-archive blobs (`<data_dir>/blobs/tasks`).
    Tasks,
    /// The attachment-mirror blobs (`<data_dir>/blobs/attachments`).
    Attachments,
}

impl RewriteTarget {
    /// The blob store directory name under an instance data dir.
    pub(crate) fn dir_name(self) -> &'static str {
        match self {
            RewriteTarget::Tasks => "tasks",
            RewriteTarget::Attachments => "attachments",
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum ConfigCommand {
    /// The instance's persistent env: the pairs a later start replays.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
}

#[derive(Subcommand)]
pub(crate) enum EnvCommand {
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
pub(crate) enum OperatorTokenCommand {
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
