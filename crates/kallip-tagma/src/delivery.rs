//! The in-process message-delivery seam: prompt delivery to an agent's inbox
//! and prompt channel, including the dead-agent reactivation path. Shared by
//! three entry points — the `send_message` HTTP route, the relay's
//! `execute_op` and room-inbound dispatch, and the engine's schedule system
//! messages — so every entry wakes a dead agent identically; only prompt
//! formatting and (bilateral-only) inbound persistence differ between callers.

use kallip_archeion_common::ids::ParticipantKind;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{ApiError, DeliveryMode, MessageResponse};
use kallip_lesche_common::message::Participant;
use tracing::{error, info, warn};

use crate::lifecycle::{
    SpawnArgs, WorkspaceAcquireFailure, abort_agent, inject_identity_env, resolve_root_agent,
    try_acquire_workspace_lock,
};
use crate::messaging::{MessageSender, SenderRelation, format_incoming, sanitize_sender};
use crate::state::{RegistryEntry, SharedState};

/// What the busy branch of `enqueue_prompt` may inject into a live agent's
/// prompt channel. Peer direct messages inject the full message payload
/// unless the sender deferred; every other caller (relay surfaces, task
/// watcher, schedule engine) keeps the historical run-boundary behavior.
///
/// Injection consumes the inbox row: the caller marks the row delivered
/// when the payload is accepted onto the prompt channel, so the wake batch
/// does not re-present the message. A failed injection (full queue) leaves
/// the row undelivered and the wake batch presents the full message —
/// failure degrades to run-boundary visibility, never to loss.
///
/// Declared windows, both accepted: between the accepted `try_send` and
/// the `mark_delivered` write, a concurrent `pull_undelivered` can present
/// the message twice (direction-safe: nothing is lost, the `delivered = 0`
/// guard keeps re-marks idempotent, SQLite serializes writes); and if the
/// process dies after the mark but before the injected turn is persisted,
/// the presentation does not replay — the full text survives in the inbox
/// store, but no wake re-delivers it.
///
/// The payload is injected verbatim: the `[Interjected message]` marker
/// family is not stripped, so a sender body containing a closing marker
/// can end the interjection block early. The full content still lands in
/// the turn (nothing lost), and this is not a security boundary.
pub(crate) enum DeliveryNotice {
    Peer {
        /// Sender-opted run-boundary visibility: suppresses the in-round
        /// injection here and the parked kick below.
        defer: bool,
    },
    /// No injection; the envelope rides the inbox alone.
    Surface,
}

/// Assemble the fast-path response for a live (running or idle-waiting)
/// agent: peer notices `try_send` the full payload onto the prompt channel
/// for in-round visibility (honestly degrading to run-boundary when the
/// queue is full), deferred sends skip the channel write, and surface
/// messages keep the historical notify-only behavior with no delivery-mode
/// claim. Returns the response plus whether the injection was accepted —
/// the caller marks the inbox row delivered only on `true`.
///
/// Declared race window (accepted, microseconds): a peer notice can land in
/// the prompt channel just as the agent parks; the prompt arm has no parked
/// guard, so the notice turn presents (and may run a round) in Parked
/// state. The message itself is already durable in the inbox either way.
fn busy_peer_response(
    prompt_tx: &tokio::sync::mpsc::Sender<String>,
    notice: &DeliveryNotice,
    payload: &str,
) -> (MessageResponse, bool) {
    match notice {
        DeliveryNotice::Peer { defer: false } => match prompt_tx.try_send(payload.to_string()) {
            Ok(()) => (
                MessageResponse {
                    queue_depth: 0,
                    warning: None,
                    delivery_mode: Some(DeliveryMode::InRound),
                },
                true,
            ),
            Err(_) => (
                MessageResponse {
                    queue_depth: 0,
                    warning: Some("notice queue full; visibility at run boundary".to_string()),
                    delivery_mode: Some(DeliveryMode::Deferred),
                },
                false,
            ),
        },
        DeliveryNotice::Peer { defer: true } => (
            MessageResponse {
                queue_depth: 0,
                warning: Some("deferred by sender".to_string()),
                delivery_mode: Some(DeliveryMode::Deferred),
            },
            false,
        ),
        DeliveryNotice::Surface => (
            MessageResponse {
                queue_depth: 0,
                warning: None,
                delivery_mode: None,
            },
            false,
        ),
    }
}
/// Deliver `text` to agent `id` as `identity`, attaching the `[From: ...]`
/// header, enqueuing on the live prompt channel, and reactivating a dead agent.
/// The HTTP `send_message` handler and the in-process relay share this single
/// seam so reactivation + header formatting cannot drift.
///
/// `defer` (the wire `MessageRequest.defer`) asks for run-boundary
/// visibility: no in-round notice and no parked kick — the message still
/// lands in the inbox either way.
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
    attachment: Option<kallip_common::protocol::agent::FileAttachment>,
    defer: bool,
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

    let response = enqueue_prompt(
        state,
        id,
        envelope,
        "operator",
        DeliveryNotice::Peer { defer },
    )
    .await?;
    // The external chat-room conversation is root-only, and only
    // user-facing inbounds (operator identity — `sender = None` on the
    // direct path, `Some(user)` on the relay; inter-agent messages
    // carry a different relation and are skipped) are recorded, via
    // the projector — the sole writer; both the direct and relay
    // paths funnel through here. Recording happens AFTER the enqueue
    // accepts: a refused message (parked 409, queue-full) must not
    // append a transcript row, or every client retry appends another
    // (the frontend re-sends refused messages). Recording still wins
    // the race that matters — an agent reply needs an LLM round-trip —
    // and the crash window shrinks to the ms between accept and append.
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
            .record_inbound(sender.clone(), text.to_string(), attachment)
            .await;
    }

    Ok(response)
}

/// Deliver an inbound relay-surface message (room or direct session) to the
/// root agent's prompt channel (the inbound counterpart of the outbound
/// `send_room_message` / `send_direct_message` in `routes/lesche`). The
/// header carries the authenticated sender id + the surface id (`room <id>`
/// / `direct <id>`), so the agent can reply with `kallip lesche send --room
/// <room>` / `send --tagma <tagma-id>`. Unlike [`deliver_message`], this does
/// NOT call `record_inbound`: relay surfaces bypass the bilateral projector
/// entirely (the lesche is the surface's store of record; the tagma is one
/// member, not the transcript owner), so no local `chat_history` row is
/// written and no bilateral `UserMessage` frame is published. The shared
/// `enqueue_prompt` (fast path + reactivation) is reused so a relay
/// message wakes a dead root agent just like a bilateral one.
pub async fn deliver_inbound_relay_message(
    state: &SharedState,
    id: &AgentId,
    surface: crate::messaging::Surface<'_>,
    sender_kind: &str,
    sender_id: &str,
    sender_handle: String,
    text: &str,
) -> Result<MessageResponse, ApiError> {
    info!(
        receiver = %id, surface = surface.label(), sender_kind, sender_id,
        "delivering relay message"
    );
    let envelope = crate::messaging::format_relay_incoming(
        sender_kind,
        sender_id,
        sender_handle,
        surface,
        text,
    );
    enqueue_prompt(
        state,
        id,
        envelope,
        surface.source(),
        DeliveryNotice::Surface,
    )
    .await
}

/// Coarse human-readable duration for the kick turn ("45s", "3m 12s",
/// "2h 5m") — the agent reads this in its transcript. Migrated from the
/// deleted wake endpoint; the delivery gate below is its only consumer.
fn format_parked_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Build the kick `[system]` turn for a parked agent: why and how long ago
/// it parked, leaving the decision (retry, adjust, report) to the agent.
/// Same text the deleted wake endpoint sent; the delivery gate's parked
/// branch (auto-wake) is its only consumer.
fn format_kick_text(
    reason: &kallip_common::protocol::ParkedReason,
    parked_at: std::time::Instant,
) -> String {
    let duration = format_parked_duration(parked_at.elapsed());
    format!(
        "[system] you were parked {duration} ago: {reason}. Decide whether to retry, adjust, or report."
    )
}

/// Enqueue an already-formatted prompt string to an agent: the fast path
/// (non-blocking send to a live agent's prompt channel) and the slow path
/// (reactivating a dead agent on a fresh channel). Shared by the bilateral
/// [`deliver_message`], the relay inbound tail, the task watcher, and the
/// schedule engine, so every caller wakes a dead root agent identically;
/// `DeliveryNotice` separates peer in-round injection from surface calls.
pub(crate) async fn enqueue_prompt(
    state: &SharedState,
    id: &AgentId,
    envelope: String,
    source: &str,
    notice: DeliveryNotice,
) -> Result<MessageResponse, ApiError> {
    // Dangling-binding gate: an agent whose recorded profile-set binding
    // does not resolve (a record that predates set binding, or one naming
    // a set the registry no longer offers) cannot be woken — its LLM calls
    // would all fail. Reject before the inbox write so the message does
    // not sit in a dead queue; the recovery paths are binding a live set
    // (the root re-binds the default set at restore) or removing the agent.
    {
        let registry = state.registry.read().await;
        let dangling = registry
            .get(id)
            .and_then(|entry| entry.as_live())
            .is_some_and(|live| {
                state
                    .profiles
                    .load()
                    .registry
                    .resolve_recorded_set(live.identity.config.profile_set.as_deref())
                    .is_err()
            });
        if dangling {
            return Err(ApiError::conflict(
                "agent has no usable profile set (unbound or unknown); rebind it first (kallip profile-set bind <agent> <set>, or PUT /agents/{id}/profile-set) or remove the agent",
            ));
        }
    }
    // The ingest timestamp is stamped once here and rendered into both the
    // wake batch (pull_undelivered) and the in-round injection payload, so
    // the agent sees one timestamp for a message on either presentation
    // path; the inbox row and the payload differ by transport only.
    let ingested_at = time::OffsetDateTime::now_utc();
    // In-round payload for a non-deferred peer message: the full envelope
    // behind a wake-batch-shaped header. Built before the envelope moves
    // into the inbox row. Size is bounded downstream by the runtime's
    // interjection cap — this crate adds no truncation of its own.
    let injection_payload = matches!(notice, DeliveryNotice::Peer { defer: false }).then(|| {
        let ts = ingested_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "?".to_owned());
        format!("[{ts}] {source}:\n{envelope}")
    });
    let inbox_store = state
        .inboxes
        .get()
        .ok_or_else(|| ApiError::internal("inbox store not installed"))?;
    let inbox_id = inbox_store
        .push_get_id(
            id.clone(),
            crate::inbox::BufferedEvent {
                timestamp: ingested_at,
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
            delivery_mode: Some(DeliveryMode::Buffered),
        });
    }

    // Auto-wake gate (parked): the message is durable in the inbox above,
    // so it carries the wake warrant — enqueue the kick [system] turn and
    // the prompt arm's post-round drain pulls the message once the kick
    // round un-parks the agent (the kick text itself carries no message
    // body). Notify-class events stay buffered: transient signals carry
    // no warrant (guard matrix). Falls through on a raced un-park (the
    // state moved under the lock re-check — an ordinary race) so the
    // liveness path below delivers normally.
    enum ParkedWake {
        Kick(String, tokio::sync::mpsc::Sender<String>),
        ParkedWithoutReason,
    }
    let parked_wake: Option<ParkedWake> = {
        let registry = state.registry.read().await;
        match registry.get(id).and_then(|entry| entry.as_live()) {
            Some(live) if live.agent.get_state() == crate::state::AgentState::Parked => {
                // Re-check under the parked lock (ex-endpoint precedent): the
                // outer Parked read races the bridge's Busy path (state mark +
                // payload clear under this same lock). A still-Parked mark
                // with no payload is the real invariant break — the message
                // is already safe in the inbox, so surface it as a warning
                // rather than failing the send.
                let cell = live.agent.parked.lock().unwrap_or_else(|e| e.into_inner());
                if live.agent.get_state() != crate::state::AgentState::Parked {
                    None
                } else if let Some(snapshot) = cell.as_ref() {
                    Some(ParkedWake::Kick(
                        format_kick_text(&snapshot.reason, snapshot.at),
                        live.agent.prompt_tx.clone(),
                    ))
                } else {
                    Some(ParkedWake::ParkedWithoutReason)
                }
            }
            _ => None,
        }
    };
    if let Some(wake) = parked_wake {
        // Sender-opted defer: skip the kick entirely — no wake turn, no
        // notify. The message waits in the inbox for the agent's next
        // natural wake.
        if matches!(notice, DeliveryNotice::Peer { defer: true }) {
            return Ok(MessageResponse {
                queue_depth: 0,
                warning: Some("deferred by sender; agent parked".to_string()),
                delivery_mode: Some(DeliveryMode::Deferred),
            });
        }
        let (warning, delivery_mode) = match wake {
            ParkedWake::Kick(text, prompt_tx) => match prompt_tx.try_send(text) {
                Ok(()) => (
                    "agent was parked; a kick turn was sent to wake it".to_string(),
                    DeliveryMode::Kicked,
                ),
                // Queue full: the message is safe in the inbox; the kick is
                // deferred until the queue drains (a parked agent keeps
                // consuming prompt turns, so this is a pathological state).
                Err(_) => (
                    "agent is parked and its prompt queue is full; message buffered to inbox, wake deferred".to_string(),
                    DeliveryMode::Deferred,
                ),
            },
            ParkedWake::ParkedWithoutReason => (
                "agent is parked without a parked reason (invariant break); message buffered to inbox"
                    .to_string(),
                DeliveryMode::Deferred,
            ),
        };
        return Ok(MessageResponse {
            queue_depth: 0,
            warning: Some(warning),
            delivery_mode: Some(delivery_mode),
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
            // A live Normal-class agent must hold the write-lock
            // on its workspace at every delivery; absence here is the
            // lock-evaporation signature. Log-only probe — the delivery
            // itself is unaffected.
            if entry.state_for_summary() != crate::state::AgentState::Faulted
                && entry.identity().config.permissions_class
                    == kallip_runtime::config::PermissionClass::Normal
                && !state
                    .lock_manager
                    .holds_exact(id, &entry.identity().config.workspace_root)
                    .unwrap_or(false)
            {
                warn!(
                    id = %id,
                    ws = %entry.identity().config.workspace_root.display(),
                    "live normal-class agent is missing its workspace lock at delivery"
                );
            }
            let (response, injected) = busy_peer_response(
                &live.agent.prompt_tx,
                &notice,
                injection_payload.as_deref().unwrap_or_default(),
            );
            live.agent.notify.notify_one();
            if injected && let Some(inbox_id) = inbox_id {
                drop(registry);
                inbox_store.mark_delivered(id, inbox_id).await;
            }
            return Ok(response);
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
        // probe. If the channel is now open, take the busy fast path.
        if !live.agent.prompt_tx.is_closed() {
            let (response, injected) = busy_peer_response(
                &live.agent.prompt_tx,
                &notice,
                injection_payload.as_deref().unwrap_or_default(),
            );
            live.agent.notify.notify_one();
            drop(registry);
            if injected && let Some(inbox_id) = inbox_id {
                inbox_store.mark_delivered(id, inbox_id).await;
            }
            return Ok(response);
        }

        info!(id = %id, "reactivating agent");
        live.agent.agent_abort.abort();
        live.agent.bridge_handle.abort();
        // The panic watcher cannot tell incarnations apart: if it wins the
        // registry lock only after the fresh install, it faults the healthy
        // replacement. Its swap duty is moot here -- reactivation replaces
        // the entry wholesale.
        live.agent.agent_watch.abort();
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

        // Resolve the set from the recorded binding, as restore does. A
        // dangling binding cannot be reactivated — reject before any
        // state is swapped.
        let config = live.identity.config.clone();
        let set = {
            let bundle = state.profiles.load();
            let set = bundle
                .registry
                .resolve_recorded_set(config.profile_set.as_deref())
                .map_err(|e| ApiError::bad_request(format!("{e}")))?;
            set.clone()
        };

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
            set,
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
        let root_agent_id = resolve_root_agent(registry.root_agent().map(|(rid, _)| rid));
        (chain_ids, root_agent_id)
    };
    spawn_args.root_agent_id = root_agent_id.clone();
    inject_identity_env(
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

    let (agent, new_identity) = match (state.spawn_fn)(spawn_args).await {
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

    // The envelope never entered a prompt channel: the fresh incarnation
    // pulls it from the inbox on its first wake — buffered visibility.
    Ok(MessageResponse {
        queue_depth: 0,
        warning: None,
        delivery_mode: Some(DeliveryMode::Buffered),
    })
}

#[cfg(test)]
mod tests;

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
