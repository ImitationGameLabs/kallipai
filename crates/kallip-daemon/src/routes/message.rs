use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallip_common::protocol::{ApiError, MessageResponse};
use tracing::{error, info, warn};

use super::MessageRequest;
use crate::routes::agent::{
    SpawnArgs, WorkspaceAcquireFailure, abort_agent, spawn_agent, try_acquire_workspace_lock,
};
use crate::sse::sse_stream;
use crate::state::SharedState;
use kallip_common::agentid::AgentId;

/// Any authenticated agent may send a message to any other agent.
/// This is intentional: inter-agent communication should not require a
/// supervisor relationship. Agents cooperate as peers.
///
/// Returns [`MessageResponse`] with queue depth feedback:
/// - `queue_depth == 0`: agent will process the message immediately.
/// - `queue_depth > 0`: message is queued behind existing messages (warning included).
/// - `503`: message queue is full, caller should retry later.
pub async fn send_message(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(req): Json<MessageRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), ApiError> {
    // Fast path: agent is alive, try non-blocking send.
    {
        let registry = state.registry.read().await;
        let entry = registry
            .get(&id)
            .ok_or_else(|| ApiError::not_found("agent not found"))?;
        match try_enqueue(&entry.agent.prompt_tx, &req.text) {
            EnqueueResult::Accepted(response) => return Ok((StatusCode::ACCEPTED, Json(response))),
            EnqueueResult::Full => {
                let cap = entry.agent.prompt_tx.max_capacity();
                return Err(ApiError::unavailable(format!(
                    "agent message queue is full ({cap} messages), retry later"
                )));
            }
            EnqueueResult::Closed => { /* fall through to reactivation */ }
        }
    }

    // Slow path: agent is dead, reactivate.
    //
    // Two-phase approach to avoid holding the write lock during spawn:
    //   1. Write lock: abort old handles, create fresh channel, install sender
    //      (pre-send the message so it occupies a slot). Concurrent requests
    //      now see an open channel and won't fall through to reactivation.
    //   2. No lock:    spawn new agent using the pre-created channel.
    //      Then re-acquire write lock to install the full Agent struct.

    // Phase 1: Pre-reserve under write lock — install fresh channel + message.
    let spawn_args = {
        let mut registry = state.registry.write().await;
        let entry = registry
            .get_mut(&id)
            .ok_or_else(|| ApiError::not_found("agent not found"))?;

        // Double-check under write lock: another request may have reactivated.
        match try_enqueue(&entry.agent.prompt_tx, &req.text) {
            EnqueueResult::Accepted(response) => return Ok((StatusCode::ACCEPTED, Json(response))),
            EnqueueResult::Full => {
                let cap = entry.agent.prompt_tx.max_capacity();
                return Err(ApiError::unavailable(format!(
                    "agent message queue is full ({cap} messages), retry later"
                )));
            }
            EnqueueResult::Closed => { /* proceed to reactivation */ }
        }

        info!(id = %id, "reactivating agent");
        entry.agent.agent_handle.abort();
        entry.agent.bridge_handle.abort();
        // Release the dead incarnation's directory write-locks before re-spawn,
        // so the new incarnation starts with an empty lock set and any peer it
        // was blocking is freed. The workspace write-lock is re-acquired in
        // Phase 2 below (mirroring `create_agent`), so the reactivated agent
        // can write its own workspace once more.
        state.lock_manager.release_all(&id);
        // Create a fresh channel and install the sender immediately.
        // This "reserves" the reactivation: concurrent requests see an open
        // channel instead of a closed one, so they try_enqueue normally.
        let (prompt_tx, prompt_rx) = tokio::sync::mpsc::channel(state.prompt_queue_size);
        // Pre-send the message so it's already queued when the agent starts.
        prompt_tx.try_send(req.text.clone()).map_err(|e| {
            error!(id = %id, "fresh channel rejected pre-send: {e}");
            ApiError::internal("failed to pre-send message")
        })?;
        entry.agent.prompt_tx = prompt_tx;

        // Resolve the tier purely by depth (positional tiers) — reactivation re-derives the same
        // way restore does.
        let config = entry.agent.config.clone();
        let tier = state
            .profiles
            .select_profile(config.permissions.depth())
            .clone();

        SpawnArgs {
            agent_id: id.clone(),
            store: entry.agent.store.clone(),
            approvals: entry.agent.approvals.clone(),
            agent_dir: entry.agent.agent_dir.clone().unwrap_or_default(),
            config,
            initial_prompt: None, // message already pre-sent to the channel
            shutdown_cancel: state.shutdown.clone(),
            events_tx: entry.agent.events_tx.clone(),
            // Hash preserved across reactivation → token_index stays consistent
            // (same id, same hash), so the reactivated agent needs no re-registration.
            auth_token_hash: entry.agent.auth_token_hash.clone(),
            env: entry.agent.env.clone(),
            shared_state: state.clone(),
            tool_policy: entry.agent.tool_policy.clone(),
            exec_policy: entry.agent.exec_policy.clone(),
            prompt_queue_size: state.prompt_queue_size,
            prompt_channel: Some((entry.agent.prompt_tx.clone(), prompt_rx)),
            tier,
        }
    }; // Write lock released. Concurrent requests see open channel.

    // Phase 2: re-acquire the workspace write-lock, then spawn outside the lock.
    //
    // The dead incarnation's locks were released in Phase 1; re-acquire the
    // workspace lock (Normal only) so the agent can write its own workspace --
    // mirrors `create_agent` and closes the post-reactivation EACCES gap. On
    // conflict (a peer legitimately grabbed the workspace while this agent was
    // dead), REFUSE reactivation: waking the agent without its workspace lock
    // would silently reproduce the exact EACCES gap this re-acquire exists to
    // close. The sender gets holder/conflict; a retry re-attempts once the peer
    // releases. The guard's `Drop` releases the lock if spawn fails below.
    let chain_ids: Vec<AgentId> = match spawn_args.config.created_by.as_ref() {
        Some(sup) => match state.registry.read().await.supervisor_chain_ids(sup) {
            Ok(ids) => ids,
            Err(e) => {
                warn!(
                    id = %id,
                    supervisor = %sup,
                    "supervisor chain broken on reactivation ({e}); \
                     proceeding with empty carve-out"
                );
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let workspace_lock =
        match try_acquire_workspace_lock(&state, &id, &spawn_args.config, &chain_ids) {
            Ok(guard) => guard,
            Err(WorkspaceAcquireFailure::Busy { holder, conflict }) => {
                close_prompt_channel(&state, &id).await;
                return Err(ApiError::conflict(format!(
                    "workspace {} overlaps a write-lock on {} held by agent {}; \
                 remove it or wait for release before reactivating",
                    spawn_args.config.workspace_root.display(),
                    conflict.display(),
                    holder,
                )));
            }
            Err(WorkspaceAcquireFailure::Other(e)) => {
                close_prompt_channel(&state, &id).await;
                return Err(ApiError::internal(format!(
                    "failed to re-acquire workspace lock: {e}"
                )));
            }
        };

    let agent = match spawn_agent(spawn_args).await {
        Ok(a) => {
            // Spawn succeeded: the agent owns the workspace lock for its
            // lifetime. Disarm so the guard's (imminent) Drop does not release.
            if let Some(mut guard) = workspace_lock {
                guard.disarm();
            }
            a
        }
        Err(e) => {
            // `workspace_lock`'s Drop releases the re-acquired lock as this
            // arm unwinds -- no manual `release_all` needed.
            error!(id = %id, "reactivation failed: {e:#}");
            close_prompt_channel(&state, &id).await;
            warn!(id = %id, "agent left in dead state; next message will retry reactivation");
            return Err(ApiError::internal(format!("reactivation failed: {e:#}")));
        }
    };

    {
        let mut registry = state.registry.write().await;
        let Some(entry) = registry.get_mut(&id) else {
            // Agent was removed while we were spawning. Release any locks the
            // fresh incarnation may have acquired (defense-in-depth, mirroring
            // the shutdown drain — the new task should not have run yet, but be
            // explicit).
            abort_agent(&agent);
            state.lock_manager.release_all(&id);
            return Err(ApiError::not_found("agent removed during reactivation"));
        };
        // No try_enqueue double-check needed: the sender we installed in
        // Phase 1 is still there, and the new Agent's prompt_tx is the same
        // sender (passed through prompt_channel).
        entry.agent = agent;
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(MessageResponse {
            queue_depth: 0,
            warning: None,
        }),
    ))
}

/// Any authenticated agent may subscribe to any other agent's event stream.
/// Mirrors the peer communication model of `send_message`.
pub async fn sse_events(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    let rx = entry.agent.events_tx.subscribe();
    Ok(sse_stream(rx, state.shutdown.clone()))
}

// -- Helpers --

/// Swap the agent's prompt sender to a closed channel so concurrent
/// `try_enqueue` callers see `Closed` instead of accepting a message into a
/// dead-end. Used when reactivation fails before or during spawn.
async fn close_prompt_channel(state: &SharedState, id: &AgentId) {
    let mut registry = state.registry.write().await;
    if let Some(entry) = registry.get_mut(id) {
        let (dead_tx, dead_rx) = tokio::sync::mpsc::channel(1);
        drop(dead_rx);
        entry.agent.prompt_tx = dead_tx;
    }
}

/// Outcome of a non-blocking message enqueue attempt.
#[derive(Debug)]
enum EnqueueResult {
    /// Message accepted. Includes queue depth feedback.
    Accepted(MessageResponse),
    /// Queue is at capacity.
    Full,
    /// Channel closed (agent task exited).
    Closed,
}

/// Try to enqueue a message into the agent's channel without blocking.
/// Returns queue depth feedback on success.
fn try_enqueue(tx: &tokio::sync::mpsc::Sender<String>, text: &str) -> EnqueueResult {
    // Sender exposes capacity() (available slots) and max_capacity() (total).
    // Queue depth = max_capacity - capacity.
    let capacity = tx.capacity();
    let max_capacity = tx.max_capacity();
    let queue_depth = max_capacity - capacity;

    if queue_depth >= max_capacity {
        return EnqueueResult::Full;
    }

    match tx.try_send(text.to_owned()) {
        Ok(()) => {
            let warning = if queue_depth > 0 {
                let plural = if queue_depth == 1 { "" } else { "s" };
                Some(format!(
                    "{queue_depth} message{plural} already queued, processing may be delayed"
                ))
            } else {
                None
            };
            EnqueueResult::Accepted(MessageResponse {
                queue_depth,
                warning,
            })
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => EnqueueResult::Full,
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => EnqueueResult::Closed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A newly created channel accepts a message with queue_depth == 0 and no warning.
    #[tokio::test]
    async fn try_enqueue_empty_channel_accepted() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(5);
        let result = try_enqueue(&tx, "hello");
        match result {
            EnqueueResult::Accepted(resp) => {
                assert_eq!(resp.queue_depth, 0);
                assert!(resp.warning.is_none());
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    /// Filling the channel produces Full result.
    #[tokio::test]
    async fn try_enqueue_full_channel_rejected() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(3);
        // Fill the channel.
        for i in 0..3 {
            try_enqueue(&tx, &format!("msg-{i}"));
        }
        // Next send should be Full.
        match try_enqueue(&tx, "overflow") {
            EnqueueResult::Full => {}
            other => panic!("expected Full, got {other:?}"),
        }
    }

    /// Partially filled channel returns queue_depth > 0 with a warning.
    #[tokio::test]
    async fn try_enqueue_partial_channel_warns() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(5);
        // Send one message to partially fill.
        try_enqueue(&tx, "first");
        // Second message should see queue_depth == 1.
        match try_enqueue(&tx, "second") {
            EnqueueResult::Accepted(resp) => {
                assert_eq!(resp.queue_depth, 1);
                assert!(resp.warning.is_some());
                assert!(resp.warning.unwrap().contains("1 message"));
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    /// Closed channel returns Closed result.
    #[tokio::test]
    async fn try_enqueue_closed_channel() {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(5);
        drop(rx); // Close the receiving end.
        match try_enqueue(&tx, "hello") {
            EnqueueResult::Closed => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }
}
