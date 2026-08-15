use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallip_agora_common::ids::ParticipantKind;
use kallip_common::protocol::{ApiError, MessageResponse};
use kallip_lesche_common::message::Participant;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::messaging::{MessageSender, SenderRelation, format_incoming, sanitize_sender};

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
    // Offline-direct path: there is no relay envelope and no authenticated
    // user, so the inbound is recorded under the operator partition (`NULL`) —
    // `deliver_message` receives `sender = None` for that. The relay path
    // passes the envelope `Participant` explicitly.
    let response = deliver_message(&state, auth.identity().clone(), None, &id, &req.text).await?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

/// Deliver `text` to agent `id` as `identity`, attaching the `[From: ...]`
/// header, enqueuing on the live prompt channel, and reactivating a dead agent.
/// The HTTP [`send_message`] handler and the in-process relay share this single
/// seam so reactivation + header formatting cannot drift.
///
/// `sender` is the user-facing wire sender (`Participant`): the relay passes
/// the (relay-authenticated) envelope peer; the offline HTTP path passes `None`
/// (the operator is anonymous, recorded under the `NULL` partition). `None`
/// also for inter-agent messages (no user-facing transcript entry). The handle
/// is sanitized at this ingest point before it is persisted or rendered into
/// the prompt.
pub async fn deliver_message(
    state: &SharedState,
    identity: crate::auth::Identity,
    sender: Option<Participant>,
    id: &AgentId,
    text: &str,
) -> Result<MessageResponse, ApiError> {
    // Sanitize the wire sender's handle once, at ingest, so the persisted row
    // and the prompt header both see a clean value (format_incoming sanitizes
    // again as defense in depth).
    let sender = sender.map(sanitize_sender);
    // Derive the prompt-header sender from the caller's auth identity, enriched
    // with the user handle when the wire sender is a user. Computed once and
    // reused across the fast path and the reactivation slow path.
    let (header_sender, relation) = {
        let registry = state.registry.read().await;
        match identity {
            crate::auth::Identity::Operator => {
                let handle = match &sender {
                    Some(p) if p.kind == ParticipantKind::Human => Some(p.handle.clone()),
                    _ => None,
                };
                (MessageSender::Operator { handle }, SenderRelation::Operator)
            }
            crate::auth::Identity::Agent { id: sender_id } => {
                let role = registry
                    .get(&sender_id)
                    .map(|e| e.identity().config.role.clone())
                    .unwrap_or_else(|| "unknown".to_owned());
                let relation = registry.relation_of(Some(&sender_id), id);
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
    info!(receiver = %id, sender = ?header_sender, relation = ?relation, "delivering message");
    let envelope = format_incoming(&header_sender, relation, text);

    // The external chat-room conversation is root-only. For a user->root
    // message, record it via the external projector BEFORE enqueuing the prompt
    // so the inbound row is durable even if the agent reply races, and so the
    // projector's published `UserMessage` frame lets the frontend promote its
    // optimistic line. A user-facing entry is one from the operator identity
    // (the direct operator path with `sender = None`, or the relay path with
    // `sender = Some(user)`); inter-agent messages carry a different relation
    // and are skipped. The projector is the sole writer; both the direct and
    // relay paths funnel through here. `sender` is the peer partition (`None` =
    // operator, `Some` = relay user).
    let is_root = {
        let registry = state.registry.read().await;
        registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == id)
    };
    if is_root
        && matches!(relation, SenderRelation::Operator)
        && let Some(projector) = state.external.get()
    {
        projector
            .record_inbound(sender.clone(), text.to_string())
            .await;
    }

    enqueue_prompt(state, id, envelope, "operator").await
}

/// Deliver an inbound room message to the root agent's prompt channel (the
/// inbound counterpart of the outbound `send_room_message` in `routes/lesche`).
/// The room header carries the authenticated sender tagma id + the room id, so
/// the agent can reply with `kallip lesche send --room <room>`. Unlike
/// [`deliver_message`], this does NOT call `record_inbound`: rooms bypass the
/// bilateral projector entirely (lesche is the room's store of record; the
/// tagma is one member, not the transcript owner), so no local
/// `chat_history` row is written and no bilateral `UserMessage` frame is
/// published. The shared [`enqueue_prompt`] (fast path + reactivation) is reused
/// so a room message wakes a dead root agent just like a bilateral one.
pub async fn deliver_inbound_room_message(
    state: &SharedState,
    id: &AgentId,
    room: &kallip_agora_common::ids::RoomId,
    sender_kind: &str,
    sender_id: &str,
    sender_handle: String,
    text: &str,
) -> Result<MessageResponse, ApiError> {
    info!(receiver = %id, room = %room, sender_kind, sender_id, "delivering room message");
    let envelope =
        crate::messaging::format_room_incoming(sender_kind, sender_id, sender_handle, room, text);
    enqueue_prompt(state, id, envelope, "room").await
}

/// Enqueue an already-formatted prompt string to an agent: the fast path
/// (non-blocking send to a live agent's prompt channel) and the slow path
/// (reactivating a dead agent on a fresh channel). Shared by the bilateral
/// [`deliver_message`] and the room `deliver_room_message` so both paths
/// wake a dead root agent identically; only the prompt formatting and the
/// (bilateral-only) inbound persistence differ between the callers.
pub(crate) async fn enqueue_prompt(
    state: &SharedState,
    id: &AgentId,
    envelope: String,
    source: &str,
) -> Result<MessageResponse, ApiError> {
    // Push the full message body to the inbox — always. The inbox is the
    // universal message store; the agent pulls undelivered direct messages on
    // wake via the MessagePuller trait.
    let inbox_store = state
        .inboxes
        .get()
        .ok_or_else(|| ApiError::internal("inbox store not installed"))?;
    inbox_store
        .push(
            id.clone(),
            crate::inbox::BufferedEvent {
                timestamp: time::OffsetDateTime::now_utc(),
                source: source.to_string(),
                body: envelope,
            },
        )
        .await;

    // Duty gate: an off-duty agent must not wake. The message sits in the
    // inbox (delivered=0) and is pulled when the agent transitions to on-duty.
    if state.duty.is_off_duty(id) {
        return Ok(MessageResponse {
            queue_depth: 0,
            warning: Some("agent is off-duty; message buffered to inbox".to_string()),
        });
    }

    // On-duty: check agent liveness via prompt_tx.is_closed(). A closed channel
    // means the task has died and needs reactivation. The message is already in
    // the inbox, so the reactivated agent pulls it on its first notify wake.
    {
        let registry = state.registry.read().await;
        let entry = registry
            .get(id)
            .ok_or_else(|| ApiError::not_found("agent not found"))?;
        let live = entry.as_live().ok_or_else(|| {
            let reason = match entry {
                RegistryEntry::Faulted(f) => f.reason.clone(),
                _ => String::new(),
            };
            ApiError::conflict(format!(
                "agent is faulted ({reason}); it cannot receive messages"
            ))
        })?;
        if !live.agent.prompt_tx.is_closed() {
            live.agent.notify.notify_one();
            return Ok(MessageResponse { queue_depth: 0, warning: None });
        }
        // Channel closed: fall through to reactivation.
    }

    // Slow path: agent is dead, reactivate.
    //
    // Split into a reserve step and a spawn step so the write lock is not held
    // during spawn:
    //   - Reserve step (write lock): abort old handles, create a fresh channel,
    //     install the sender, and pre-send the message so it occupies a slot.
    //     Concurrent requests then see an open channel and won't fall through.
    //   - Spawn step (no lock): spawn the new agent on the pre-created channel,
    //     then re-acquire the write lock to install the full Agent struct.

    // Reserve step (under the write lock): install a fresh channel + message.
    let mut spawn_args = {
        let mut registry = state.registry.write().await;
        let entry = registry
            .get_mut(id)
            .ok_or_else(|| ApiError::not_found("agent not found"))?;
        // Defensive: the fast path rejects faulted entries, so reaching here
        // means the entry is live. Reject anyway if a future refactor bypasses
        // the fast path -- a faulted entry has no runtime fields to read.
        let live = entry
            .as_live_mut()
            .ok_or_else(|| ApiError::conflict("agent is faulted; cannot reactivate"))?;

        // Double-check: another request may have reactivated since the read-lock
        // probe. If the channel is now open, just notify.
        if !live.agent.prompt_tx.is_closed() {
            live.agent.notify.notify_one();
            return Ok(MessageResponse { queue_depth: 0, warning: None });
        }

        info!(id = %id, "reactivating agent");
        live.agent.agent_handle.abort();
        live.agent.bridge_handle.abort();
        // Release the dead incarnation's directory write-locks before re-spawn,
        // so the new incarnation starts with an empty lock set and any peer it
        // was blocking is freed. The workspace write-lock is re-acquired in
        // the spawn step below (mirroring `create_agent`), so the reactivated
        // agent can write its own workspace once more.
        state.lock_manager.release_all(id);
        // Create a fresh channel (no pre-send: the message is already in the
        // inbox; the reactivated agent pulls it on its first notify wake).
        let (prompt_tx, prompt_rx) = tokio::sync::mpsc::channel(state.prompt_queue_size);
        live.agent.prompt_tx = prompt_tx;

        // Resolve the tier purely by depth (positional tiers) — reactivation re-derives the same
        // way restore does.
        let config = live.identity.config.clone();
        let tier = state
            .profiles
            .load()
            .registry
            .select_profile(config.permissions.depth())
            .clone();

        SpawnArgs {
            agent_id: id.clone(),
            // Placeholder — the supervisor chain (needed to resolve the real
            // root) is walked after this block, under the registry read-lock.
            // Correct as-is for a root reactivation (chain empty ⇒ self).
            root_agent_id: id.clone(),
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
            preset: live.agent.preset,
            exec_policy: live.agent.exec_policy.clone(),
            prompt_queue_size: state.prompt_queue_size,
            prompt_channel: Some((live.agent.prompt_tx.clone(), prompt_rx)),
            tier,
        }
    }; // Write lock released. Concurrent requests see open channel.

    // Spawn step: re-acquire the workspace write-lock, then spawn outside the lock.
    //
    // The dead incarnation's locks were released in the reserve step above; re-acquire the
    // workspace lock (Normal only) so the agent can write its own workspace --
    // mirrors `create_agent` and closes the post-reactivation EACCES gap. On
    // conflict (a peer legitimately grabbed the workspace while this agent was
    // dead), REFUSE reactivation: waking the agent without its workspace lock
    // would silently reproduce the exact EACCES gap this re-acquire exists to
    // close. The sender gets holder/conflict; a retry re-attempts once the peer
    // releases. The guard's `Drop` releases the lock if spawn fails below.
    // Walk the supervisor chain and resolve the root under one registry
    // read-lock, then drop the guard before the workspace-lock acquire below.
    // The reactivation path does not call `default_env` (it reuses the dead
    // incarnation's env map), so the identity env vars are injected via the
    // shared helper further down.
    //
    // The root is resolved authoritatively from the registry's single root,
    // independent of the supervisor chain: a broken chain (warned, empty
    // `chain_ids`) degrades only the workspace carve-out below, never the root
    // identity — so a reactivated subagent never sees its own id as `root`.
    let (chain_ids, root_agent_id) = {
        let registry = state.registry.read().await;
        let chain_ids: Vec<AgentId> = match spawn_args.config.created_by.as_ref() {
            Some(sup) => match registry.supervisor_chain_ids(sup) {
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
        let root_agent_id =
            super::agent::resolve_root_agent(registry.root_agent().map(|(rid, _)| rid));
        (chain_ids, root_agent_id)
    };
    spawn_args.root_agent_id = root_agent_id.clone();
    super::agent::inject_identity_env(
        &mut spawn_args.env,
        spawn_args.config.created_by.as_ref(),
        &root_agent_id,
    );
    let workspace_lock = match try_acquire_workspace_lock(state, id, &spawn_args.config, &chain_ids)
    {
        Ok(guard) => guard,
        Err(WorkspaceAcquireFailure::Busy { holder, conflict }) => {
            close_prompt_channel(state, id).await;
            return Err(ApiError::conflict(format!(
                "workspace {} overlaps a write-lock on {} held by agent {}; \
                 remove it or wait for release before reactivating",
                spawn_args.config.workspace_root.display(),
                conflict.display(),
                holder,
            )));
        }
        Err(WorkspaceAcquireFailure::Other(e)) => {
            close_prompt_channel(state, id).await;
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
            close_prompt_channel(state, id).await;
            warn!(id = %id, "agent left in dead state; next message will retry reactivation");
            return Err(ApiError::internal(format!("reactivation failed: {e:#}")));
        }
    };

    {
        let mut registry = state.registry.write().await;
        let Some(entry) = registry.get_mut(id) else {
            // Agent was removed while we were spawning. Release any locks the
            // fresh incarnation may have acquired (defense-in-depth, mirroring
            // the shutdown drain — the new task should not have run yet, but be
            // explicit).
            abort_agent(&agent, new_identity.agent_dir.as_deref());
            state.lock_manager.release_all(id);
            return Err(ApiError::not_found("agent removed during reactivation"));
        };
        // Structural write-back: the entry is live (the fast path rejects
        // faulted entries), so swap in the freshly-spawned runtime handle
        // while preserving identity and subagent_ids.
        let live = match entry {
            RegistryEntry::Live(live) => live,
            RegistryEntry::Faulted(_) => {
                // The entry became faulted between the reserve and spawn steps. Abort
                // the fresh spawn and release any locks it acquired (the
                // workspace lock was disarmed on spawn success, so the manager
                // is the only cleanup path) -- mirrors the entry-removed arm.
                abort_agent(&agent, new_identity.agent_dir.as_deref());
                state.lock_manager.release_all(id);
                return Err(ApiError::conflict(
                    "agent became faulted during reactivation",
                ));
            }
        };
        // No try_enqueue double-check needed: the sender we installed in the
        // reserve step is still there, and the new Agent's prompt_tx is the same
        // sender (passed through prompt_channel).
        live.agent = agent;
    }

    // Notify the reactivated agent to wake and pull from inbox.
    {
        let registry = state.registry.read().await;
        if let Some(entry) = registry.get(id)
            && let Some(live) = entry.as_live()
        {
            live.agent.notify.notify_one();
        }
    }

    Ok(MessageResponse {
        queue_depth: 0,
        warning: None,
    })
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

/// The direct (local) external SSE: serves the projected external vocabulary
/// (authored messages, runtime signals, status snapshots) to a local frontend
/// client. Root-only — the direct stream is the root agent's conversation, and
/// the projector subscribes to the root regardless of the path id, so a non-root
/// id is rejected to keep the route honest. Any authenticated identity may
/// subscribe (the root owns the single conversation). The stream is one
/// multiplexed channel discriminated by the SSE `event:` name.
pub async fn external_events(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    // Hold the registry read-lock once for both the root check and the initial
    // status snapshot. The snapshot is prepended to the SSE so the chat header
    // renders at connect instead of after the next status-pump tick (~2 s).
    let initial = {
        let registry = state.registry.read().await;
        let is_root = registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == &id);
        if !is_root {
            return Err(ApiError::not_found(
                "external event stream is root-only; the direct conversation is the root's",
            ));
        }
        let payload = crate::relay::status_pump::snapshot_status(&registry, &state.token_budget);
        crate::direct::DirectFrame::Status(payload)
    };
    let direct = state
        .direct
        .get()
        .ok_or_else(|| ApiError::unavailable("direct serving not initialized"))?;
    let rx = direct.subscribe();
    Ok(crate::sse::direct_sse_stream(
        rx,
        Some(initial),
        state.shutdown.clone(),
    ))
}

/// Query params for [`external_history`]: `after` (incremental catch-up, rows
/// with id > after), `before` (scroll-up, id < before), or neither (the most
/// recent `limit`). Mirrors the relay `TagmaControl::History` modes.
#[derive(Debug, Default, Deserialize)]
pub struct ExternalHistoryQuery {
    #[serde(default)]
    pub after: Option<i64>,
    #[serde(default)]
    pub before: Option<i64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `GET /agents/{id}/external/history` -- the pull-based history window for the
/// direct (offline) path, cursor-driven by the frontend's high-water mark.
/// Root-only and projector-owned; returns decoded entries (sender + reply) and
/// a `more` flag. The direct SSE stays live-only; history is always a
/// frontend-initiated pull, symmetric with the relay's `TagmaControl::History`.
#[derive(Debug, Serialize)]
pub struct ExternalHistoryResponse {
    pub rows: Vec<kallip_lesche_common::message::HistoryEntry>,
    pub more: bool,
}

pub async fn external_history(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Query(query): Query<ExternalHistoryQuery>,
) -> Result<Json<ExternalHistoryResponse>, ApiError> {
    {
        let registry = state.registry.read().await;
        let is_root = registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == &id);
        if !is_root {
            return Err(ApiError::not_found(
                "external history is root-only; the direct conversation is the root's",
            ));
        }
    }
    let projector = state
        .external
        .get()
        .ok_or_else(|| ApiError::unavailable("external projector not initialized"))?;
    // Bound the page the same way the relay does (HISTORY_BATCH_MAX = 50); a
    // missing `limit` defaults to the cap.
    const DEFAULT_LIMIT: u32 = 50;
    const MAX_LIMIT: u32 = 50;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    // The direct endpoint serves the operator partition (`user_id IS NULL`).
    let (rows, more) = projector
        .read_history(None, query.after, query.before, limit)
        .await;
    Ok(Json(ExternalHistoryResponse { rows, more }))
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

#[cfg(test)]
mod tests;
