//! The agent-management HTTP face: handlers that create, inspect and change
//! agents — thin shells over `crate::lifecycle` (where the spawn domain lives)
//! and the registry.
//!
//! The handlers live in submodules grouped by what they act on: `lifecycle`
//! (create, root bootstrap and lookup, verification, listing, metadata,
//! activity, duty, removal), `park` (deactivate, restore, interrupt),
//! `subagent` (spawn, profile-set) and `workspace` (relocate, retarget).
//! The re-exports below keep the names callers outside the module use
//! resolving under `crate::routes::agent::`; `permissions` stays here as
//! the permission-endpoint domain.

mod lifecycle;
mod park;
mod subagent;
mod workspace;

pub use lifecycle::{
    create_agent, ensure_root_agent, get_root_agent, list_agents, remove_agent, update_activity,
    update_duty, update_metadata, verify_agent,
};
pub use park::interrupt_agent;
pub use subagent::update_profile_set;
pub use workspace::{relocate_agent, update_workspace_root};

pub(crate) use lifecycle::ensure_workspace_dir;
pub(crate) use park::{
    DeactivateErr, RestoreStepErr, deactivate_core, interrupt_core, restore_boot,
    restore_reactivate,
};
pub(crate) use subagent::{SubagentSpawn, spawn_subagent};

mod permissions;
pub(crate) use permissions::{parse_requested_class, validate_subagent_request};

#[cfg(test)]
mod tests;
