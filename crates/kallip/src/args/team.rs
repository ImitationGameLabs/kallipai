//! `team` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};
use std::path::PathBuf;

/// The `kallip team` family — declarative team management. The
/// declaration (tagma.toml) is the desired team; the lock (tagma.lock,
/// a CLI-side archive next to it) records role→agent bindings; converge
/// makes reality match the declaration and rewrites the lock.
#[derive(Subcommand)]
pub(crate) enum TeamCommand {
    /// Show the three-way comparison for one declaration: declaration vs
    /// lock vs live registry, one row per role, with converge's verdict.
    Status(TeamStatusArgs),
    /// Converge the live fleet to the declaration: plan, preflight
    /// (whole-batch refusal on structural problems), then execute with
    /// fail-fast. Writes the lock on applied/aborted outcomes.
    Converge(TeamConvergeArgs),
    /// Rebuild the lock archive from reality: live members come from the
    /// registry, parked members from the inactive area (their agent dirs
    /// carry the role). Every recovered parked body is listed.
    LockRebuild(TeamLockRebuildArgs),
}

/// Path arguments shared by the team family: the declaration file and
/// the lock archive. The lock defaults to `tagma.lock` next to the
/// declaration (they are one team archive); the declaration defaults to
/// `./tagma.toml`.
#[derive(Args)]
pub(crate) struct TeamCommonArgs {
    /// Declaration file (tagma.toml).
    #[arg(long)]
    pub file: Option<String>,
    /// Lock archive (tagma.lock); defaults to the declaration's directory.
    #[arg(long)]
    pub lock: Option<PathBuf>,
    /// Machine face: JSON output, stable field names.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub(crate) struct TeamStatusArgs {
    #[command(flatten)]
    pub common: TeamCommonArgs,
}

#[derive(Args)]
pub(crate) struct TeamConvergeArgs {
    #[command(flatten)]
    pub common: TeamCommonArgs,
    /// Plan only: print what would happen, touch nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Wait for busy deactivation targets to go idle, then converge
    /// (poll loop; Ctrl-C aborts the wait, never the fleet).
    #[arg(long)]
    pub drain: bool,
    /// Interrupt busy deactivation targets instead of refusing (recorded
    /// as an auditable escape in the affected result rows).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub(crate) struct TeamLockRebuildArgs {
    /// Directory to write tagma.lock into (defaults to the declaration's
    /// directory, or the current directory with no --file).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Machine face: JSON output.
    #[arg(long)]
    pub json: bool,
}
