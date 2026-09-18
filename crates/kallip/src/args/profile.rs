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
}
