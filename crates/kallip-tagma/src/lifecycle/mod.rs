//! Agent lifecycle: the path from agent definition to a running task —
//! identity env injection, workspace lock acquisition, spawn with rollback,
//! and materialization of the subagent runtime, plus restart recovery
//! (`restore`). Shared by the agent-creation route and the delivery seam's
//! dead-agent reactivation: every path that brings an agent from definition
//! to a running task goes through here.

mod identity;
mod restore;
mod spawn;
mod workspace;

#[cfg(test)]
pub(crate) use identity::{compose_system_prompt, meta_skill_content};
pub(crate) use identity::{inject_identity_env, resolve_root_agent};
pub(crate) use restore::restore_agents;

pub(crate) use spawn::{Materialize, SpawnArgs, abort_agent, spawn_agent};

#[cfg(test)]
pub(crate) use workspace::{EstablishLockFailure, establish_lock_api_error};
pub(crate) use workspace::{
    WorkspaceAcquireFailure, establish_workspace_lock, try_acquire_workspace_lock,
};
