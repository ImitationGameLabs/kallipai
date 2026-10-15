//! Parking and resume: deactivation, restore from the inactive area,
//! and interrupts.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use kallipai_adk::persistence;
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::ApiError;

use super::lifecycle::{ensure_workspace_dir, teardown_agent};
use crate::state::{AgentState, SharedState};

/// The id-targeted deactivate sequence shared by converge's batch face and
/// the relocate orchestration: refuse a busy (neither idle nor parked)
/// agent, refuse live subagents, unregister, tear down, and park the body
/// in the inactive area. `force` tears a busy target down anyway.
///
/// The refusal details stay uncontextualized so each caller can phrase
/// them for its own face (a converge row says "batch stopped"; relocate
/// says "interrupt it first").
pub(crate) enum DeactivateErr {
    /// The target is neither idle nor parked: `{0}` is the agent id.
    Busy(String),
    /// The target supervises live subagents: `{0}` is the agent id.
    HasSubagents(String),
    /// Anything else (vanished mid-operation, park write failed): the
    /// detail string, passed through verbatim.
    Failed(String),
}

pub(crate) async fn deactivate_core(
    state: &SharedState,
    id: &AgentId,
    force: bool,
) -> Result<Vec<String>, DeactivateErr> {
    let mut notes = Vec::new();
    tracing::info!(id = %id, force, "deactivating agent");
    let entry = {
        let mut registry = state.registry.write().await;
        let Some(entry) = registry.get(id) else {
            return Err(DeactivateErr::Failed(format!(
                "agent {id} vanished mid-operation"
            )));
        };
        if let Some(live) = entry.as_live() {
            let state_now = live.agent.get_state();
            let busy = !matches!(
                state_now,
                crate::state::AgentState::Idle | crate::state::AgentState::Parked
            );
            if busy && !force {
                tracing::warn!(id = %id, state = %state_now, "deactivation refused: the target is neither idle nor parked");
                return Err(DeactivateErr::Busy(id.to_string()));
            }
            if busy {
                tracing::warn!(id = %id, state = %state_now, "force escape: tearing down a target that is neither idle nor parked");
                notes.push("force escape: target neither idle nor parked, torn down".to_string());
            }
        }
        if !entry.subagent_ids().is_empty() {
            tracing::warn!(id = %id, subagents = entry.subagent_ids().len(), "deactivation refused: live subagents");
            return Err(DeactivateErr::HasSubagents(id.to_string()));
        }
        registry.unregister(id).ok_or_else(|| {
            DeactivateErr::Failed(format!("agent {id} vanished during unregister"))
        })?
    };
    if force {
        // The call lands after the unregister: the registry no longer holds the
        // target, so interrupt_core returns not_found and changes nothing.
        let _ = interrupt_core(state, id).await;
    }
    teardown_agent(state, id, entry).await;
    persistence::deactivate_agent_dir(id).map_err(|e| {
        tracing::error!(id = %id, "agent is unregistered but the park failed; the body is left in the live area and needs manual cleanup: {e:#}");
        DeactivateErr::Failed(format!(
            "agent {id} is unregistered but the park failed: {e:#}"
        ))
    })?;
    tracing::info!(id = %id, notes = ?notes, "agent parked in the inactive area");
    state.invalidate();
    Ok(notes)
}

/// Error taxonomy of the restore cores. `Degraded` marks an unusable
/// identity: converge replaces the body with a fresh spawn, relocate
/// refuses with the body left parked. `Failed` marks a hard stop with the
/// body parked (or reported as needing manual cleanup).
pub(crate) enum RestoreStepErr {
    Degraded(Vec<String>),
    Failed(String),
}

/// First half of the shared restore sequence: blind-spot recheck (the id
/// must be absent, the role free), read the deactivated body's meta,
/// ensure the recorded workspace exists, and move the body back into the
/// live area. Returns the live dir plus its meta for the caller's
/// between-halves work (converge aligns the meta to the declaration
/// here; relocate goes straight through to the boot).
pub(crate) async fn restore_reactivate(
    state: &SharedState,
    id: &AgentId,
    role: &str,
) -> Result<(std::path::PathBuf, persistence::AgentMeta), RestoreStepErr> {
    tracing::info!(id = %id, role, "restoring the parked body");
    {
        let registry = state.registry.read().await;
        if registry.get(id).is_some() {
            tracing::warn!(id = %id, "agent came alive mid-operation");
            return Err(RestoreStepErr::Failed(format!(
                "agent {id} came alive mid-operation"
            )));
        }
        if let Some((holder, _)) = registry
            .iter()
            .find(|(_, e)| e.identity().config.role == role)
        {
            tracing::warn!(id = %id, role, holder = %holder, "role is now held by another agent");
            return Err(RestoreStepErr::Failed(format!(
                "role {role:?} is now held by agent {holder}"
            )));
        }
    }
    let inactive_dir = match persistence::inactive_dir(id) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(id = %id, "inactive body unreadable: {e:#}");
            return Err(RestoreStepErr::Degraded(vec![format!(
                "inactive body unreadable: {e:#}"
            )]));
        }
    };
    let meta = match persistence::read_meta_from_dir(&inactive_dir) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::warn!(id = %id, "the inactive body's metadata is unreadable: {e:#}");
            return Err(RestoreStepErr::Degraded(vec![format!(
                "the inactive body's metadata is unreadable ({e:#})"
            )]));
        }
    };
    // A deactivated body whose recorded workspace vanished must fail
    // rather than wake unable to write its own workspace.
    if let Err(e) = ensure_workspace_dir(&meta.workspace_root) {
        tracing::warn!(id = %id, workspace = %meta.workspace_root.display(), "recorded workspace unusable: {e}");
        return Err(RestoreStepErr::Failed(e));
    }
    if let Err(e) = persistence::reactivate_agent_dir(id) {
        tracing::warn!(id = %id, "could not move the inactive body back to the live area: {e:#}");
        return Err(RestoreStepErr::Failed(format!(
            "could not move the inactive body back to the live area: {e:#}"
        )));
    }
    let dir = match persistence::agent_dir(id) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(id = %id, "live dir unresolved: {e:#}");
            return Err(RestoreStepErr::Failed(format!(
                "live dir unresolved: {e:#}"
            )));
        }
    };
    tracing::info!(id = %id, dir = %dir.display(), "body moved back to the live area");
    Ok((dir, meta))
}

/// Second half of the shared restore sequence: boot the reactivated body
/// through the same `restore_inactive` path the tagma boot uses, then
/// register under the write lock with the same re-checks; a raced
/// violation is undone (task torn down, body re-parked) so the abort
/// leaves no ghost.
pub(crate) async fn restore_boot(
    state: &SharedState,
    id: &AgentId,
    dir: &std::path::Path,
    meta: persistence::AgentMeta,
    role: &str,
) -> Result<(), RestoreStepErr> {
    match crate::lifecycle::restore_inactive(
        state.shutdown.clone(),
        state.clone(),
        persistence::PendingRestore {
            agent_id: id.clone(),
            agent_dir: dir.to_path_buf(),
            meta,
        },
    )
    .await
    {
        Ok((rid, entry)) => {
            let mut registry = state.registry.write().await;
            let violated = registry.get(id).is_some()
                || registry
                    .iter()
                    .any(|(_, e)| e.identity().config.role == role);
            if violated {
                drop(registry);
                teardown_agent(state, &rid, crate::state::RegistryEntry::Live(entry)).await;
                let parked = persistence::deactivate_agent_dir(id);
                let mut detail =
                    "registry changed during the restore (role or id taken)".to_string();
                match parked {
                    Ok(()) => {
                        detail.push_str("; the body was re-parked");
                        tracing::warn!(id = %id, "registry changed during the restore; the body was re-parked");
                    }
                    Err(e) => {
                        detail.push_str(&format!("; the re-park failed; the body is left in the live area and needs manual cleanup ({e:#})"));
                        tracing::error!(id = %id, "re-park failed; the body is left in the live area and needs manual cleanup: {e:#}");
                    }
                }
                return Err(RestoreStepErr::Failed(detail));
            }
            registry.register(rid, crate::state::RegistryEntry::Live(entry));
            drop(registry);
            state.invalidate();
            tracing::info!(id = %id, role, "agent restored and registered");
            Ok(())
        }
        Err(e) => {
            // The body cannot come up: park it back (best effort) so the
            // failure surfaces with the identity intact and parked. A
            // failed re-park is surfaced in the notes, never swallowed.
            tracing::warn!(id = %id, "restore failed; attempting to return the body to the inactive area: {e:#}");
            let mut loss = vec![format!("restore failed ({e:#})")];
            match persistence::deactivate_agent_dir(id) {
                Ok(()) => loss[0].push_str("; the body was re-parked"),
                Err(park) => {
                    tracing::error!(id = %id, "re-park failed; the body is left in the live area and needs manual cleanup: {park:#}");
                    loss[0].push_str(&format!(
                        "; the re-park failed; the body is left in the live area and needs manual cleanup ({park:#})"
                    ));
                }
            }
            Err(RestoreStepErr::Degraded(loss))
        }
    }
}

/// Cancel the target's current round (or kick it out of a parked wait) — the
/// shared core of `POST /agents/{id}/interrupt` and the set-removal sweep.
/// The caller has already passed its own authorization check.
///
/// Interrupt = cancel the current round only (the task stays alive and returns
/// to its outer loop). A clean no-op when the agent is idle (no round to
/// abort); Err only when the agent is faulted (nothing to interrupt) or its
/// prompt queue is full.
pub(crate) async fn interrupt_core(state: &SharedState, id: &AgentId) -> Result<(), ApiError> {
    // Clone the shared slot Arc under the registry read-lock, then release it
    // before touching the inner std Mutex — so the async registry guard is
    // never held across the (sync) round-cancel lock.
    let (round_cancel, parked_interrupt) = {
        let registry = state.registry.read().await;
        let Some(entry) = registry.get(id) else {
            return Err(ApiError::not_found("agent not found"));
        };
        let live = entry
            .as_live()
            .ok_or_else(|| ApiError::conflict("agent is faulted; nothing to interrupt"))?;
        (
            live.agent.round_cancel.clone(),
            // Waiting/retrying park in the outer loop with no round token
            // to cancel — a bare interrupt would be a silent no-op. Enqueue
            // an interrupt turn instead (the kick pattern): the prompt arm
            // clears both fuses and the agent decides how to land.
            matches!(
                live.agent.get_state(),
                AgentState::Waiting | AgentState::Retrying
            )
            .then(|| live.agent.prompt_tx.clone()),
        )
    };
    if let Some(round) = round_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        tracing::info!(id = %id, "interrupting an in-flight round");
        round.cancel();
    }
    if let Some(prompt_tx) = parked_interrupt {
        if prompt_tx
            .try_send("[system] you were interrupted while parked in the outer loop (waiting or retrying). The wait/retry fuse was dropped; finish with break, or continue if the interruption was unintended.".to_string())
            .is_err()
        {
            tracing::warn!(id = %id, "interrupt refused: the prompt queue is full");
            return Err(ApiError::conflict("agent prompt queue is full"));
        }
        tracing::info!(id = %id, "interrupting a parked wait (waiting or retrying); kick turn enqueued");
    }
    Ok(())
}

/// Interrupt the current agent operation without deleting it.
pub async fn interrupt_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, ApiError> {
    {
        let registry = state.registry.read().await;
        registry.require_superior(auth.identity(), &id)?;
    }
    interrupt_core(&state, &id).await?;
    Ok(StatusCode::ACCEPTED)
}
