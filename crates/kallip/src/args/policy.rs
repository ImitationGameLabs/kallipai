//! `policy` command subtree of the `kallip` CLI (clap derive).

use super::agent::IdArgs;
use clap::{Args, Subcommand};
use kallip_common::agentid::AgentId;

#[derive(Subcommand)]
pub(crate) enum PolicyCommand {
    /// Show full agent permissions and the active classify preset
    Show(IdArgs),
    /// Show agent bash_exec command-policy overrides
    ExecGet(IdArgs),
    /// Set a per-command bash_exec override (superior-only)
    ExecSet(ExecSetArgs),
}

#[derive(Args)]
pub(crate) struct ExecSetArgs {
    /// Agent ID or role.
    pub id: AgentId,
    /// Command name (e.g. cargo, sudo).
    pub command: String,
    /// Decision: allow, ask, deny.
    pub decision: String,
    /// Optional reason surfaced to the agent when the decision narrows (ask/deny).
    #[arg(long)]
    pub reason: Option<String>,
}
