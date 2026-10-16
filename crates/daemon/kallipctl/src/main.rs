//! kallipctl: operator-side management CLI for the kallipai local daemon.
//!
//! Five verbs over the daemon's UDS protocol; the socket's 0600 mode is the
//! auth. Deliberately NOT part of the `kallip` command family: `kallip` is
//! the in-instance runtime, `kallipctl` manages instances from outside
//! (separate installation surfaces).

mod blobs;
mod cli;
mod completion;
mod logs;
mod print;
#[cfg(test)]
mod tests;
mod token;

use crate::blobs::{RewriteInvocation, run_blobs_rewrite};
use crate::cli::{BlobsCommand, Cli, Command, ConfigCommand, EnvCommand, RewriteTarget};
use crate::logs::{run_logs, run_restart, run_service_log_list, run_service_logs};
use crate::print::print;
use crate::token::run_operator_token;
use anyhow::Context as _;
use anyhow::Result;
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
            // Both sources ride the daemon wire: an instance slug
            // reads that instance's log directory, a polis service
            // reads the host's /var/log/kallipai tree through the
            // same socket, and neither argument lists the services.
            return match (slug, service) {
                (Some(slug), None) => {
                    run_logs(&client, &slug, lines, file.as_deref(), follow).await
                }
                (None, Some(service)) => {
                    run_service_logs(&client, &service, lines, follow, level).await
                }
                (None, None) => run_service_log_list(&client).await,
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
