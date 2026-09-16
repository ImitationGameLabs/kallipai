//! `agent` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};
use kallip_common::agentid::AgentId;

/// The `kallip agent` command family: a read-only fleet directory.
#[derive(Subcommand)]
pub(crate) enum AgentDirCommand {
    /// List every agent on this tagma: role, state, since, id, workspace.
    List,
}

/// Ungrouped per-agent ops, flattened into the top-level command list — they
/// never appear as an "agent" group in `--help`.
#[derive(Subcommand)]
pub(crate) enum AgentCommand {
    /// Send a peer message to an agent (fire-and-forget; processed
    /// asynchronously). The message text is read from the full stdin
    /// (multiline); prefer a quoted heredoc `<<'EOF'` so shell expansion
    /// cannot corrupt it. Prints a one-line JSON echo on success.
    Message(MessageArgs),
    /// Show an agent's context token usage and recent retry history.
    Status(StatusArgs),
    /// Report this agent's current activity (self-only)
    Activity(ActivityArgs),
}

#[derive(Args)]
pub(crate) struct ActivityArgs {
    /// Current activity, in a short phrase (e.g. "reading docs/x.md"). Pass an
    /// empty string to clear. Field name matches `UpdateActivityRequest::activity`.
    pub activity: String,
}

/// Args for `kallip message`. The message text is read from the full stdin
/// (multiline — pipe, heredoc, or `< file`); prefer a quoted heredoc
/// `<<'EOF'` so shell expansion cannot corrupt it.
#[derive(Args)]
pub(crate) struct MessageArgs {
    /// Agent ID or role.
    pub id: AgentId,
    /// Defer visibility to the receiver's run boundary: skip the in-round
    /// notice and the parked wake. The message still lands in the inbox.
    #[arg(long)]
    pub defer: bool,
}

#[derive(Args)]
pub(crate) struct IdArgs {
    /// Agent ID or role.
    pub id: AgentId,
}

#[derive(Args)]
pub(crate) struct StatusArgs {
    /// Agent ID or role (positional; omit for the fleet overview).
    pub id: Option<AgentId>,
    /// Render timestamps as relative distances (8m ago) instead of absolute UTC.
    #[arg(long)]
    pub relative_time: bool,
}
