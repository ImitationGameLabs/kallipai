use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallip_common::protocol::{ApiError, MessageResponse};
use tracing::{error, info, warn};

use crate::messaging::{MessageSender, SenderRelation, format_incoming};

use super::MessageRequest;
use crate::routes::agent::{
    SpawnArgs, WorkspaceAcquireFailure, abort_agent, spawn_agent, try_acquire_workspace_lock,
};
use crate::sse::sse_stream;
use crate::state::{RegistryEntry, SharedState};
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
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(req): Json<MessageRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), ApiError> {
    // Derive the sender from the caller's auth identity and render a
    // `[From: ...]` header so the receiver knows who sent the message and how
    // they relate. Computed once and reused across the fast path and the
    // reactivation slow path. The sender's entry may have been unregistered
    // between auth resolution and this lock; fall back to a placeholder role.
    let (sender, relation) = {
        let registry = state.registry.read().await;
        match auth.identity() {
            crate::auth::Identity::Operator => (MessageSender::Operator, SenderRelation::Operator),
            crate::auth::Identity::Agent { id: sender_id } => {
                let role = registry
                    .get(sender_id)
                    .map(|e| e.identity().config.role.clone())
                    .unwrap_or_else(|| "unknown".to_owned());
                let relation = registry.relation_of(Some(sender_id), &id);
                (
                    MessageSender::Agent {
                        id: sender_id.clone(),
                        role,
                    },
                    relation,
                )
            }
        }
    };
    info!(receiver = %id, sender = ?sender, relation = ?relation, "delivering message");
    let envelope = format_incoming(&sender, relation, &req.text);

    // Fast path: agent is alive, try non-blocking send.
    {
        let registry = state.registry.read().await;
        let entry = registry
            .get(&id)
            .ok_or_else(|| ApiError::not_found("agent not found"))?;
        // A faulted agent has no prompt channel and can never run; reject up
        // front rather than falling through to reactivation (which would try to
        // read runtime fields that don't exist on a faulted entry).
        let live = entry.as_live().ok_or_else(|| {
            let reason = match entry {
                RegistryEntry::Faulted(f) => f.reason.clone(),
                _ => String::new(),
            };
            ApiError::conflict(format!(
                "agent is faulted ({reason}); it cannot receive messages"
            ))
        })?;
        match try_enqueue(&live.agent.prompt_tx, &envelope) {
            EnqueueResult::Accepted(response) => return Ok((StatusCode::ACCEPTED, Json(response))),
            EnqueueResult::Full => {
                let cap = live.agent.prompt_tx.max_capacity();
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
        // Defensive: the fast path rejects faulted entries, so reaching here
        // means the entry is live. Reject anyway if a future refactor bypasses
        // the fast path -- a faulted entry has no runtime fields to read.
        let live = entry
            .as_live_mut()
            .ok_or_else(|| ApiError::conflict("agent is faulted; cannot reactivate"))?;

        // Double-check under write lock: another request may have reactivated.
        match try_enqueue(&live.agent.prompt_tx, &envelope) {
            EnqueueResult::Accepted(response) => return Ok((StatusCode::ACCEPTED, Json(response))),
            EnqueueResult::Full => {
                let cap = live.agent.prompt_tx.max_capacity();
                return Err(ApiError::unavailable(format!(
                    "agent message queue is full ({cap} messages), retry later"
                )));
            }
            EnqueueResult::Closed => { /* proceed to reactivation */ }
        }

        info!(id = %id, "reactivating agent");
        live.agent.agent_handle.abort();
        live.agent.bridge_handle.abort();
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
        // Pre-send the labeled message so it's already queued when the agent
        // starts -- it becomes the reactivated agent's first user turn, carrying
        // the same `[From: ...]` header as the live path.
        prompt_tx.try_send(envelope.clone()).map_err(|e| {
            error!(id = %id, "fresh channel rejected pre-send: {e}");
            ApiError::internal("failed to pre-send message")
        })?;
        live.agent.prompt_tx = prompt_tx;

        // Resolve the tier purely by depth (positional tiers) — reactivation re-derives the same
        // way restore does.
        let config = live.identity.config.clone();
        let tier = state
            .profiles
            .select_profile(config.permissions.depth())
            .clone();

        SpawnArgs {
            agent_id: id.clone(),
            store: live.agent.store.clone(),
            approvals: live.agent.approvals.clone(),
            agent_dir: live.identity.agent_dir.clone().unwrap_or_default(),
            config,
            initial_prompt: None, // message already pre-sent to the channel
            shutdown_cancel: state.shutdown.clone(),
            events_tx: live.agent.events_tx.clone(),
            // Hash preserved across reactivation → token_index stays consistent
            // (same id, same hash), so the reactivated agent needs no re-registration.
            auth_token_hash: live.agent.auth_token_hash.clone(),
            env: live.agent.env.clone(),
            shared_state: state.clone(),
            tool_policy: live.agent.tool_policy.clone(),
            exec_policy: live.agent.exec_policy.clone(),
            prompt_queue_size: state.prompt_queue_size,
            prompt_channel: Some((live.agent.prompt_tx.clone(), prompt_rx)),
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

    let (agent, new_identity) = match spawn_agent(spawn_args).await {
        Ok((a, new_identity)) => {
            // Spawn succeeded: the agent owns the workspace lock for its
            // lifetime. Disarm so the guard's (imminent) Drop does not release.
            if let Some(mut guard) = workspace_lock {
                guard.disarm();
            }
            // Reactivation preserves the existing identity (config/agent_dir are
            // unchanged); hold the returned identity only for its dir, used on
            // the rollback paths below.
            (a, new_identity)
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
            abort_agent(&agent, new_identity.agent_dir.as_deref());
            state.lock_manager.release_all(&id);
            return Err(ApiError::not_found("agent removed during reactivation"));
        };
        // Structural write-back: the entry is live (the fast path rejects
        // faulted entries), so swap in the freshly-spawned runtime handle
        // while preserving identity and subagent_ids.
        let live = match entry {
            RegistryEntry::Live(live) => live,
            RegistryEntry::Faulted(_) => {
                // The entry became faulted between Phase 1 and Phase 2. Abort
                // the fresh spawn and release any locks it acquired (the
                // workspace lock was disarmed on spawn success, so the manager
                // is the only cleanup path) -- mirrors the entry-removed arm.
                abort_agent(&agent, new_identity.agent_dir.as_deref());
                state.lock_manager.release_all(&id);
                return Err(ApiError::conflict(
                    "agent became faulted during reactivation",
                ));
            }
        };
        // No try_enqueue double-check needed: the sender we installed in
        // Phase 1 is still there, and the new Agent's prompt_tx is the same
        // sender (passed through prompt_channel).
        live.agent = agent;
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
    // Subscribe and clone the sender under one lock, then build the SSE stream
    // after releasing it. The sender outlives this call (held by the agent's
    // registry entry); the receiver-count transition logged by `sse_stream` is
    // observed against the same channel the receiver was subscribed to.
    let (rx, events_tx) = {
        let registry = state.registry.read().await;
        let entry = registry
            .get(&id)
            .ok_or_else(|| ApiError::not_found("agent not found"))?;
        let live = entry
            .as_live()
            .ok_or_else(|| ApiError::conflict("agent is faulted; no event stream"))?;
        let rx = live.agent.events_tx.subscribe();
        let events_tx = live.agent.events_tx.clone();
        (rx, events_tx)
    };
    Ok(sse_stream(id, events_tx, rx, state.shutdown.clone()))
}

// -- Helpers --

/// Swap the agent's prompt sender to a closed channel so concurrent
/// `try_enqueue` callers see `Closed` instead of accepting a message into a
/// dead-end. Used when reactivation fails before or during spawn.
async fn close_prompt_channel(state: &SharedState, id: &AgentId) {
    let mut registry = state.registry.write().await;
    if let Some(entry) = registry.get_mut(id)
        && let Some(live) = entry.as_live_mut()
    {
        let (dead_tx, dead_rx) = tokio::sync::mpsc::channel(1);
        drop(dead_rx);
        live.agent.prompt_tx = dead_tx;
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
    use crate::auth::{AuthIdentity, Identity};
    use crate::state::AgentId;
    use crate::test_helpers::{add_faulted_root, make_entry_with_rx, make_state};
    use axum::Json;
    use axum::extract::{Path, State};
    use kallip_common::protocol::MessageRequest;

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

    // -- send_message: sender identity is attached to the delivered payload --

    /// Deliver a message as the operator and assert the receiver sees a
    /// `[From: operator]` header.
    #[tokio::test]
    async fn operator_message_carries_operator_header() {
        let state = make_state();
        let receiver = AgentId::random();
        let (mut entry, mut rx) = make_entry_with_rx(None, "recv".into());
        entry.identity.config.role = "root".into();
        state
            .registry
            .write()
            .await
            .register(receiver.clone(), RegistryEntry::Live(entry));

        let resp = send_message(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(receiver),
            Json(MessageRequest {
                text: "do the thing".into(),
            }),
        )
        .await
        .expect("operator send accepted");
        assert_eq!(resp.0, StatusCode::ACCEPTED);

        let delivered = rx.recv().await.expect("message delivered");
        assert_eq!(delivered, "[From: operator]\ndo the thing");
    }

    /// Deliver a message from a child agent to its parent and assert the
    /// header identifies the sender (id + role) and the subordinate relation.
    #[tokio::test]
    async fn agent_message_carries_sender_and_relation_header() {
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        let (mut parent_entry, mut parent_rx) = make_entry_with_rx(None, "parent".into());
        parent_entry.identity.config.role = "lead".into();
        state
            .registry
            .write()
            .await
            .register(parent.clone(), RegistryEntry::Live(parent_entry));

        let (mut child_entry, _child_rx) = make_entry_with_rx(Some(parent.clone()), "child".into());
        child_entry.identity.config.role = "researcher".into();
        state
            .registry
            .write()
            .await
            .register(child.clone(), RegistryEntry::Live(child_entry));

        let resp = send_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: child.clone() }),
            Path(parent),
            Json(MessageRequest {
                text: "results attached".into(),
            }),
        )
        .await
        .expect("agent send accepted");
        assert_eq!(resp.0, StatusCode::ACCEPTED);

        let delivered = parent_rx.recv().await.expect("message delivered");
        // Child is a subordinate of parent (parent is the receiver/ancestor).
        let expected =
            format!("[From: agent {child} (role: researcher, subordinate)]\nresults attached");
        assert_eq!(delivered, expected);
    }

    /// Self-message: an agent messaging itself is labeled `same`.
    #[tokio::test]
    async fn self_message_carries_same_relation() {
        let state = make_state();
        let me = AgentId::random();
        let (mut entry, mut rx) = make_entry_with_rx(None, "me".into());
        entry.identity.config.role = "solo".into();
        state
            .registry
            .write()
            .await
            .register(me.clone(), RegistryEntry::Live(entry));

        let _ = send_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: me.clone() }),
            Path(me.clone()),
            Json(MessageRequest {
                text: "note to self".into(),
            }),
        )
        .await
        .expect("self send accepted");

        let delivered = rx.recv().await.expect("message delivered");
        assert_eq!(
            delivered,
            format!("[From: agent {me} (role: solo, same)]\nnote to self")
        );
    }

    /// Messaging a faulted agent returns 409 with the reason -- it cannot
    /// receive messages and must not fall through to reactivation (no runtime
    /// fields to read).
    #[tokio::test]
    async fn send_message_to_faulted_returns_conflict() {
        let state = make_state();
        let faulted = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_faulted_root(&mut reg, &faulted, "restore failed: missing workspace");
        }
        let err = send_message(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(faulted),
            Json(MessageRequest { text: "hi".into() }),
        )
        .await
        .expect_err("faulted agent rejects messages");
        assert_eq!(err.status, 409);
        assert!(
            err.message.contains("faulted"),
            "message should mention faulted: {}",
            err.message
        );
        assert!(
            err.message.contains("missing workspace"),
            "message should include the reason: {}",
            err.message
        );
    }
}
