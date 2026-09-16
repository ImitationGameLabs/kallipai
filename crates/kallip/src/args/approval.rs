//! `approval` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};

#[derive(Subcommand)]
pub(crate) enum ApprovalCommand {
    /// List approvals; default shows committed ones awaiting a decision.
    List(ApprovalListArgs),
    /// Show details of an approval
    Get(ApprovalGetArgs),
    /// Approve a committed action
    Approve(ApprovalIdArgs),
    /// Deny a committed action
    Deny(ApprovalDenyArgs),
}

#[derive(Args)]
pub(crate) struct ApprovalListArgs {
    /// Page offset (0-based).
    #[arg(long)]
    pub offset: Option<u64>,
    /// Page size. Clamped to [1, 20]; defaults to 5.
    #[arg(long)]
    pub limit: Option<u64>,
    /// Filter by owning agent ID or role.
    #[arg(long)]
    pub requested_by: Option<String>,
    /// Show all statuses (default: committed only).
    #[arg(long, conflicts_with = "status")]
    pub all: bool,
    /// Filter by status: pending, committed, approved, denied, redeemed, cancelled.
    #[arg(long, conflicts_with = "all")]
    pub status: Option<String>,
    /// Reverse sort order (oldest first; default is newest first).
    #[arg(long)]
    pub reverse: bool,
}

#[derive(Args)]
pub(crate) struct ApprovalGetArgs {
    /// Approval ID.
    pub id: String,
}

#[derive(Args)]
pub(crate) struct ApprovalIdArgs {
    /// Approval ID.
    pub id: String,
}

#[derive(Args)]
pub(crate) struct ApprovalDenyArgs {
    /// Approval ID.
    pub id: String,
    /// Reason for denial.
    pub reason: String,
}
