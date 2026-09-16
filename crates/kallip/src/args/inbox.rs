//! `inbox` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};
use kallip_common::agentid::AgentId;

// ---------------------------------------------------------------------------
// Inbox commands — self-scoped via KALLIP_ID
// ---------------------------------------------------------------------------

/// Manage this agent's message inbox. The acting agent is taken from
/// `KALLIP_ID` (self-only).
#[derive(Subcommand)]
pub(crate) enum InboxCommand {
    /// List messages in the inbox (newest first).
    List(InboxListArgs),
    /// Read a single message by id (marks it as read).
    Read(InboxReadArgs),
    /// Show inbox summary counts (total, unread).
    Summary(InboxSummaryArgs),
    /// Mark a message as done.
    Done(InboxReadArgs),
    /// Clear messages: done-only by default, all with --all.
    Clear(InboxClearArgs),
}

#[derive(Args)]
pub(crate) struct InboxListArgs {
    /// Agent ID or role (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
    /// Filter by status: unread, read, done.
    #[arg(long)]
    pub status: Option<String>,
    /// Max messages to return (default 50, max 200).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Render timestamps as relative distances (8m ago) instead of absolute UTC.
    #[arg(long)]
    pub relative_time: bool,
}

#[derive(Args)]
pub(crate) struct InboxReadArgs {
    /// Agent ID or role (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
    /// Message ID (positional).
    pub msg_id: i64,
}

#[derive(Args)]
pub(crate) struct InboxSummaryArgs {
    /// Agent ID or role (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
}

#[derive(Args)]
pub(crate) struct InboxClearArgs {
    /// Agent ID or role (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
    /// Clear all messages, not just done ones.
    #[arg(long)]
    pub all: bool,
}
