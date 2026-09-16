//! `profile` command subtree of the `kallip` CLI (clap derive).

use clap::Subcommand;

/// The `kallip profile-set` family — runtime profile-set management.
#[derive(Subcommand)]
pub(crate) enum ProfileSetCommand {
    /// List the configured profile sets, their profile counts, and the
    /// default marker.
    List,
    /// Rebind an agent to a named set. A live agent swaps its failover
    /// chain on its next wake-up; a parked one resolves it at restore.
    Bind {
        /// Agent id or role (resolved like every agent-addressing command).
        id: String,
        /// Exact set name; unknown names list the available sets.
        set: String,
    },
    /// Transfer the default-set marker to an existing set.
    Default {
        /// Exact set name.
        set: String,
    },
    /// Remove a set. The default set and the root's set are refused;
    /// other referenced sets list their bound agents and need --force
    /// (they are interrupted, then keep a dangling record until rebound).
    Remove {
        /// Exact set name.
        set: String,
        /// Interrupt the bound agents, then remove.
        #[arg(long)]
        force: bool,
    },
}
