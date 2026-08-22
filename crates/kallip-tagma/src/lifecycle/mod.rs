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
/// Indirect spawn entry used by delivery's reactivation (slow path); see
/// [`AppState::spawn_fn`](crate::state::AppState::spawn_fn). An `Arc<dyn Fn>`
/// so test stubs can capture state.
pub(crate) type SpawnFn = std::sync::Arc<
    dyn Fn(
            SpawnArgs,
        ) -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = anyhow::Result<(crate::state::Agent, crate::state::AgentIdentity)>,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

/// Production default for `AppState::spawn_fn`: boxes the async fn into the
/// `SpawnFn` closure type.
pub(crate) fn spawn_agent_boxed() -> SpawnFn {
    std::sync::Arc::new(|args| Box::pin(spawn_agent(args)))
}

#[cfg(test)]
pub(crate) use workspace::{EstablishLockFailure, establish_lock_api_error};
pub(crate) use workspace::{
    WorkspaceAcquireFailure, establish_workspace_lock, try_acquire_workspace_lock,
};
