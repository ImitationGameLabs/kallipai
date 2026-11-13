//! Command-line surface: the clap parser tree and its subcommand enums.

use crate::completion::{complete_polis_name, complete_slug};
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

    /// Allow a plaintext http platform origin that is not loopback: the
    /// access token (and, for spawn, the enrollment code) would otherwise
    /// cross the network in the clear. `spawn --polis` and every `polis` verb
    /// that spends a token read it; a record or a request to a loopback or
    /// https origin never needs it. It sits at top level because a wrapper can
    /// only put a flag between the program name and the subcommand, and clap
    /// reads a global option there.
    #[arg(long, global = true)]
    pub(crate) allow_plaintext: bool,
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
    /// the newest recorded configuration, except that a `--bin` picks the
    /// binary of that launch.
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
    /// The platforms this host knows: one name per platform, with the
    /// address and the personal access token that talk to it. Host-level,
    /// so these verbs work while the daemon is down.
    Polis {
        #[command(subcommand)]
        command: PolisCommand,
    },
    /// The entries one running instance carries: written to that
    /// instance's data directory, and reachable only through the daemon
    /// that locates it.
    Tagma {
        #[command(subcommand)]
        command: TagmaCommand,
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

#[derive(Subcommand)]
pub(crate) enum PolisCommand {
    /// Record a platform under a name. The address is normalized to an
    /// origin, one name per platform and one platform per name.
    Add {
        /// Platform name: lowercase letters, digits and '-'; the name
        /// instances and spawn flags will use for it.
        #[arg(add = ArgValueCompleter::new(complete_polis_name))]
        name: String,
        /// The platform origin: http or https, with no path.
        url: String,
        /// Point a known name at a new address. Without it a different
        /// address for a known name is refused.
        #[arg(long)]
        replace: bool,
    },
    /// Drop a platform record and nothing else.
    Remove {
        /// Platform name.
        #[arg(add = ArgValueCompleter::new(complete_polis_name))]
        name: String,
    },
    /// Print the recorded platforms with their addresses and whether a
    /// token is stored. Reads the file; never the network.
    List,
    /// Manage a platform's access token.
    Token {
        #[command(subcommand)]
        command: PolisTokenCommand,
    },
    /// Ask the platform what it holds: whether it answers, whether the
    /// token is accepted, and the tagmas registered for its owner.
    Status {
        /// Platform name.
        #[arg(add = ArgValueCompleter::new(complete_polis_name))]
        name: String,
    },
    /// Revoke one tagma registration on the platform: the way out of a
    /// pending row or a tagma that will not come back.
    Revoke {
        /// Platform name.
        #[arg(add = ArgValueCompleter::new(complete_polis_name))]
        name: String,
        /// Tagma id, as `polis status` prints it.
        tagma: String,
    },
    /// Mint an enrollment code here, print it with its expiry and the pending
    /// tagma it created, and leave the redeeming to you: the hand-carried
    /// path, for an instance this CLI cannot reach.
    MintCode {
        /// Platform name.
        #[arg(add = ArgValueCompleter::new(complete_polis_name))]
        platform: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum TagmaCommand {
    /// The entries one running instance carries in its own
    /// polis.toml, as opposed to the platform register this host keeps.
    Polis {
        #[command(subcommand)]
        command: TagmaPolisCommand,
    },
}

/// The entry verbs a running instance answers: the daemon says where the
/// instance listens, and the instance answers over its own loopback faces.
#[derive(Subcommand)]
pub(crate) enum TagmaPolisCommand {
    /// Print the entries inside a running instance: its own entries, not
    /// this host's platform register. Needs the daemon, to learn where the
    /// instance listens.
    List {
        /// Instance slug.
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
    },
    /// Add an entry inside a running instance: enroll it (with a code
    /// minted here unless --code brings one) and start its relay.
    Add {
        /// Instance slug.
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
        /// Name for the entry inside the instance.
        name: String,
        /// A platform from the register: its address, and its token when
        /// a code has to be minted.
        #[arg(add = ArgValueCompleter::new(complete_polis_name))]
        platform: String,
        /// An enrollment code to redeem instead of minting one.
        #[arg(long, conflicts_with = "token_file")]
        code: Option<String>,
        /// File holding the access token to mint with; 0600 or stricter.
        #[arg(long)]
        token_file: Option<String>,
    },
    /// Change an entry that already exists: its address (a platform from
    /// the register), its credentials (--code or --token-file), or neither,
    /// which only brings the relay back up. An address change keeps the
    /// credentials the entry already holds; a code or a token file replaces
    /// them with a fresh enrollment.
    Set {
        /// Instance slug.
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
        /// Entry name, as the list verb prints it.
        name: String,
        /// A platform from the register: the new address, and its token
        /// when a code has to be minted. Leaving it out keeps the recorded
        /// address.
        #[arg(add = ArgValueCompleter::new(complete_polis_name))]
        platform: Option<String>,
        /// An enrollment code to redeem: this replaces the entry's credentials.
        #[arg(long, conflicts_with = "token_file")]
        code: Option<String>,
        /// File holding the access token to mint the replacement code with.
        #[arg(long)]
        token_file: Option<String>,
    },
    /// Drop an entry from a running instance and stop its relay.
    Remove {
        /// Instance slug.
        #[arg(add = ArgValueCompleter::new(complete_slug))]
        slug: String,
        /// Entry name, as the list verb prints it.
        name: String,
        /// Also delete the entry's credentials directory.
        #[arg(long)]
        forget: bool,
    },
}

impl TagmaPolisCommand {
    /// The instance this verb speaks to.
    pub(crate) fn slug(&self) -> &str {
        match self {
            Self::List { slug }
            | Self::Add { slug, .. }
            | Self::Set { slug, .. }
            | Self::Remove { slug, .. } => slug,
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum PolisTokenCommand {
    /// Store the platform's personal access token, after the platform
    /// accepts it. The value is read from --token-file, or stdin (a
    /// hidden prompt on a terminal), and never printed back.
    Set {
        /// Platform name.
        #[arg(add = ArgValueCompleter::new(complete_polis_name))]
        name: String,
        /// File holding the token; 0600 or stricter, like spawn's.
        #[arg(long)]
        token_file: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{Cli, Command, PolisCommand, PolisTokenCommand, TagmaCommand, TagmaPolisCommand};

    /// Parse `kallipctl polis <args>` and hand back the subcommand.
    fn polis(args: &[&str]) -> PolisCommand {
        let mut argv = vec!["kallipctl", "polis"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv).expect("a polis command").command {
            Command::Polis { command } => command,
            _ => panic!("not a polis command"),
        }
    }

    /// Parse `kallipctl tagma polis <args>` and hand back the subcommand.
    fn tagma(args: &[&str]) -> TagmaPolisCommand {
        let mut argv = vec!["kallipctl", "tagma", "polis"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv)
            .expect("a tagma polis command")
            .command
        {
            Command::Tagma {
                command: TagmaCommand::Polis { command },
            } => command,
            _ => panic!("not a tagma polis command"),
        }
    }

    fn parses(argv: &[&str]) -> bool {
        Cli::try_parse_from(argv.iter().copied()).is_ok()
    }

    #[test]
    fn the_host_verbs_parse_into_their_own_shapes() {
        assert!(matches!(polis(&["list"]), PolisCommand::List));

        match polis(&["add", "local", "http://127.0.0.1:8080", "--replace"]) {
            PolisCommand::Add { name, url, replace } => {
                assert_eq!(name, "local");
                assert_eq!(url, "http://127.0.0.1:8080");
                assert!(replace, "the flag reaches the field");
            }
            _ => panic!("add must parse into Add"),
        }

        match polis(&["remove", "local"]) {
            PolisCommand::Remove { name } => assert_eq!(name, "local"),
            _ => panic!("remove must parse into Remove"),
        }

        match polis(&["token", "set", "local", "--token-file", "/tmp/token"]) {
            PolisCommand::Token { command } => match command {
                PolisTokenCommand::Set { name, token_file } => {
                    assert_eq!(name, "local");
                    assert_eq!(token_file.as_deref(), Some("/tmp/token"));
                }
            },
            _ => panic!("token set must parse into Token"),
        }

        match polis(&["status", "local"]) {
            PolisCommand::Status { name } => assert_eq!(name, "local"),
            _ => panic!("status must parse into Status"),
        }

        match polis(&["revoke", "local", "tagma-1"]) {
            PolisCommand::Revoke { name, tagma } => {
                assert_eq!(name, "local");
                assert_eq!(tagma, "tagma-1");
            }
            _ => panic!("revoke must parse into Revoke"),
        }

        match polis(&["mint-code", "edge"]) {
            PolisCommand::MintCode { platform } => assert_eq!(platform, "edge"),
            _ => panic!("mint-code must parse into MintCode"),
        }
    }

    #[test]
    fn a_missing_argument_is_a_parse_error() {
        for argv in [
            vec!["kallipctl", "polis", "add"],
            vec!["kallipctl", "polis", "add", "local"],
            vec!["kallipctl", "polis", "remove"],
            vec!["kallipctl", "polis", "token", "set"],
            vec!["kallipctl", "polis", "status"],
            vec!["kallipctl", "polis", "revoke", "local"],
            vec!["kallipctl", "tagma", "polis", "list"],
            vec!["kallipctl", "tagma", "polis", "add", "alpha"],
            vec!["kallipctl", "tagma", "polis", "set", "alpha"],
            vec!["kallipctl", "tagma", "polis", "remove", "alpha"],
        ] {
            assert!(
                Cli::try_parse_from(argv.clone()).is_err(),
                "{argv:?} parsed"
            );
        }
    }

    #[test]
    fn the_tagma_verbs_parse_into_their_own_shapes() {
        assert!(matches!(
            tagma(&["list", "alpha"]),
            TagmaPolisCommand::List { slug } if slug == "alpha"
        ));

        match tagma(&["add", "alpha", "local", "edge", "--code", "abc"]) {
            TagmaPolisCommand::Add {
                slug,
                name,
                platform,
                code,
                token_file,
            } => {
                assert_eq!(slug, "alpha");
                assert_eq!(name, "local");
                assert_eq!(platform, "edge");
                assert_eq!(code.as_deref(), Some("abc"));
                assert!(token_file.is_none());
            }
            _ => panic!("add must parse into Add"),
        }

        match tagma(&["add", "alpha", "local", "edge", "--token-file", "/tmp/t"]) {
            TagmaPolisCommand::Add { token_file, .. } => {
                assert_eq!(token_file.as_deref(), Some("/tmp/t"));
            }
            _ => panic!("add must parse into Add"),
        }

        // The platform is a required positional: leaving it out is a parse
        // error, not a runtime one.
        assert!(!parses(&[
            "kallipctl",
            "tagma",
            "polis",
            "add",
            "alpha",
            "local"
        ]));

        match tagma(&["set", "alpha", "local"]) {
            TagmaPolisCommand::Set {
                slug,
                name,
                platform,
                code,
                token_file,
            } => {
                assert_eq!(slug, "alpha");
                assert_eq!(name, "local");
                assert!(platform.is_none());
                assert!(code.is_none() && token_file.is_none());
            }
            _ => panic!("set must parse into Set"),
        }

        match tagma(&["set", "alpha", "local", "edge"]) {
            TagmaPolisCommand::Set { platform, .. } => {
                assert_eq!(platform.as_deref(), Some("edge"));
            }
            _ => panic!("set must parse into Set"),
        }

        match tagma(&["set", "alpha", "local", "--code", "abc"]) {
            TagmaPolisCommand::Set { code, .. } => assert_eq!(code.as_deref(), Some("abc")),
            _ => panic!("set must parse into Set"),
        }

        match tagma(&["remove", "alpha", "local", "--forget"]) {
            TagmaPolisCommand::Remove { slug, name, forget } => {
                assert_eq!(slug, "alpha");
                assert_eq!(name, "local");
                assert!(forget);
            }
            _ => panic!("remove must parse into Remove"),
        }
    }

    #[test]
    fn a_code_and_the_token_to_mint_with_cannot_meet() {
        for verb in ["add", "set"] {
            assert!(
                !parses(&[
                    "kallipctl",
                    "tagma",
                    "polis",
                    verb,
                    "alpha",
                    "local",
                    "edge",
                    "--code",
                    "abc",
                    "--token-file",
                    "/tmp/token",
                ]),
                "{verb}: a code and the token to mint with cannot both be given"
            );
        }
    }
}
