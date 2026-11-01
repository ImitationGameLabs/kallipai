//! kallipctl: operator-side management CLI for the kallipai local daemon.
//!
//! Instance-management verbs over the daemon's UDS protocol; the socket's
//! 0600 mode is the auth. Deliberately NOT part of the `kallip` command
//! family: `kallip` is the in-instance runtime, `kallipctl` manages
//! instances from outside (separate installation surfaces).

mod archeion;
mod blobs;
mod cli;
mod completion;
mod logs;
mod print;
mod restart;
mod spawn_env;
#[cfg(test)]
mod tests;
mod token;

use crate::blobs::{RewriteInvocation, run_blobs_rewrite};
use crate::cli::{BlobsCommand, Cli, Command, ConfigCommand, EnvCommand};
use crate::logs::run_logs;
use crate::print::print;
use crate::restart::run_restart;
use crate::token::run_operator_token;
use anyhow::{Context as _, Result};
use clap::{CommandFactory, Parser};
use kallipai_daemon_client::DaemonClient;
use kallipai_daemon_common::wire::RequestBody;

fn main() -> Result<()> {
    // The dynamic-completion hook runs before any runtime is up: the
    // slug completer does synchronous daemon IPC, and a runtime nested
    // inside another would panic. When COMPLETE names a shell the hook
    // answers the completion request and exits, so the static `generate`
    // path below (reached after parsing) never runs on that invocation;
    // with COMPLETE unset, empty, or `0` this call is a no-op.
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
        | Command::Logs { slug, .. }
        | Command::Stop { slug } => Some(slug),
        Command::Restart { slug } => Some(slug),
        Command::Blobs {
            command: BlobsCommand::Rewrite { slug, .. },
        } => Some(slug),
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
    // Set when this invocation minted an enrollment code, so a failed
    // spawn can still point at the pending tagma it left behind.
    let mut pending_mint: Option<archeion::MintedCode> = None;
    let body = match cli.command {
        Command::Spawn {
            slug,
            workspace,
            env,
            user,
            polis,
            token_file,
        } => {
            // The daemon resolves a relative workspace against its own cwd,
            // which is not this process's, and it refuses a missing one only
            // after an enrollment code would already be spent. Resolving here
            // checks the path the daemon will actually use and sends that one.
            let workspace = absolute_workspace(&workspace)?;
            let (env, minted) = spawn_env::prepare(env, polis, token_file).await?;
            if let Some(minted) = &minted {
                eprintln!(
                    "pending tagma {} (code expires at {})",
                    minted.id, minted.expires_at
                );
            }
            pending_mint = minted;
            RequestBody::Spawn {
                slug,
                workspace,
                env,
                exe: cli.bin,
                user,
            }
        }
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
    let response = match client.call(body).await {
        Ok(response) => response,
        Err(err) => {
            return Err(pending_note(
                pending_mint.as_ref(),
                format!("talking to the kallipai daemon: {err}"),
            ));
        }
    };
    // A daemon-side refusal (a taken slug, an overlapping workspace, a spawn
    // that timed out) arrives as an `Ok` response holding the error, so the
    // note has to cover printing too, not just the call.
    match print(response, started) {
        Ok(()) => Ok(()),
        Err(err) => Err(pending_note(pending_mint.as_ref(), format!("{err:#}"))),
    }
}

/// Resolve a workspace path against this process's cwd and check it exists.
///
/// Joining rather than canonicalizing is deliberate: `canonicalize` resolves
/// symlinks, so a workspace the operator wrote as a link would be recorded
/// as its target -- a different stored path for the same command.
fn absolute_workspace(workspace: &str) -> Result<String> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    absolute_workspace_in(workspace, &cwd)
}

/// [`absolute_workspace`] with the base directory passed in, so a test does
/// not have to move the whole process's working directory.
fn absolute_workspace_in(workspace: &str, cwd: &std::path::Path) -> Result<String> {
    let path = std::path::Path::new(workspace);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if !absolute.is_dir() {
        anyhow::bail!(
            "workspace {} is not an existing directory",
            absolute.display()
        );
    }
    Ok(absolute.to_string_lossy().into_owned())
}

/// Attach what the minted code left behind to a failure message.
///
/// The pending tagma keeps one of the owner's live-pending slots until it
/// enrolls or is revoked, and an expired code does not free it, so a spawn
/// that fails after the mint has to say which row to revoke.
fn pending_note(minted: Option<&archeion::MintedCode>, message: String) -> anyhow::Error {
    match minted {
        Some(minted) => anyhow::anyhow!(
            "{message} (minted an enrollment code for this spawn: pending tagma {} holds \
             a live slot until it enrolls or is revoked, expiring at {}; an expired code \
             keeps its slot, so revoke the pending tagma in the web UI if you are not retrying)",
            minted.id,
            minted.expires_at
        ),
        None => anyhow::anyhow!(message),
    }
}
