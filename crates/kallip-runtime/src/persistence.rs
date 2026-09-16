//! Agent persistence: atomic JSON serialization to disk.
//!
//! Writes context and approval state to per-agent directories.
//! All writes use atomic rename (temp file + rename) with data and directory
//! fsync to prevent corruption on crash. On tagma restart, [`scan_agents`] scans for agents
//! that can be recovered.

mod archive;
mod io;
mod layout;
mod legacy;
mod meta;
mod repair;
mod scan;
mod store;
#[cfg(test)]
mod test_support;

pub(crate) use archive::copy_dir_all;
pub use archive::{archive_agent_dir, deactivate_agent_dir, reactivate_agent_dir};
pub use io::{load_exec_policy, persist_approvals, persist_context, persist_exec_policy};
pub use layout::{
    agent_dir, archived_dir, config_dir_root, data_dir_root, ensure_workspace_disjoint,
    inactive_dir, state_dir_root, workspace_overlaps_data_root,
};
pub use meta::{AgentMeta, create_agent_dir, read_meta, read_meta_from_dir, rewrite_meta};
pub use repair::{RepairAction, RepairReport, repair_agent_context};
pub use scan::{
    PendingRestore, RefusedRestore, RestorableAgent, find_disk_root, restore_agent, scan_agents,
    scan_inactive,
};
pub use store::{Degradation, DegradationKind};
