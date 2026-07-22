//! Tagma-wide agent shutdown: bounded graceful drain with a force-abort safety net.
//!
//! Tagma exit previously blocked for a fixed timeout (`tokio::time::sleep`)
//! before force-aborting — paying the worst-case protection time on every exit.
//! [`graceful_agent_shutdown`] instead drains the registry and awaits real task
//! completion (via [`crate::state::Agent::shutdown`]) under a deadline,
//! force-aborting only the genuinely stuck tasks.

use std::time::Duration;

use futures_util::future::join_all;
use kallip_common::agentid::AgentId;

use crate::state::{AppState, RegistryEntry};

/// Maximum time to wait for a single agent's tasks to finish on deletion.
///
/// The agent is idle + cancellation-signalled: the agent task persists and
/// returns (dropping its sender), and the bridge exits on channel-close (see
/// [`crate::bridge::bridge_task`]) — both finish in milliseconds. This is a
/// safety net for stuck tasks, not the expected wait.
pub(crate) const REMOVE_AGENT_SHUTDOWN_TIMEOUT_SECS: u64 = 10;

/// Maximum time to wait for all agents to persist before force-abort at exit.
///
/// All agents are already cancellation-signalled (each agent's `cancel` is a
/// child of the tagma-wide `shutdown` token) when this runs; they finish in
/// milliseconds (the bridge exits on channel-close, see
/// [`crate::bridge::bridge_task`]). This is a safety net, not the expected wait.
pub(crate) const GRACEFUL_SHUTDOWN_TIMEOUT_SECS: u64 = 30;

/// Drain all agents and wait for their tasks to persist before force-aborting.
///
/// Called after the HTTP server has stopped and the tagma-wide `shutdown` token
/// has been cancelled. Takes ownership of the agents out of the registry under a
/// write lock, then drops the lock before awaiting — a bridge task re-entering
/// the registry read lock (via `route_to_superior`) must not contend with an
/// await held under the write lock.
pub(crate) async fn graceful_agent_shutdown(state: &AppState) {
    let entries: Vec<(AgentId, RegistryEntry)> = {
        let mut registry = state.registry.write().await;
        registry.drain()
    };
    if entries.is_empty() {
        return;
    }
    tracing::info!(count = entries.len(), "waiting for agents to persist");

    let bound = Duration::from_secs(GRACEFUL_SHUTDOWN_TIMEOUT_SECS);
    join_all(entries.into_iter().map(|(id, entry)| async move {
        // Defense-in-depth: release this agent's directory locks (the process
        // is exiting, but explicit release keeps the coordinator consistent if
        // shutdown ever runs without a full process exit). A no-op for faulted
        // entries, which never acquired locks.
        state.lock_manager.release_all(&id);
        match entry {
            RegistryEntry::Live(live) => {
                if !live.agent.shutdown(bound).await {
                    tracing::warn!(id = %id, "agent did not shut down in time, force-aborted");
                }
            }
            RegistryEntry::Faulted(_) => {
                // No task to await -- the on-disk data stays for the next
                // startup to retry. Nothing to do but drop the entry.
                tracing::info!(id = %id, "faulted agent drained (no task)");
            }
        }
    }))
    .await;

    tracing::info!("all agents shut down");
}
