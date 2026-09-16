//! `budget` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};

// ---------------------------------------------------------------------------
// Budget commands
// ---------------------------------------------------------------------------

/// Manage tagma-wide token budget.
#[derive(Subcommand)]
pub(crate) enum BudgetCommand {
    /// Show tagma-wide token budget status
    Get,
    /// Increase the tagma-wide token budget by an amount.
    Increase(BudgetAmountArgs),
    /// Decrease the tagma-wide token budget by an amount.
    Decrease(BudgetAmountArgs),
    /// Set remaining tagma-wide token budget (=0 pauses all agents)
    Set(BudgetAmountArgs),
    /// Switch to an unlimited budget (enforcement off, consumption still tracked)
    Unlimited,
}

#[derive(Args)]
pub(crate) struct BudgetAmountArgs {
    /// Token amount (supports K, M, G suffixes, e.g. 100M, 500K, 1G).
    pub amount: String,
}
