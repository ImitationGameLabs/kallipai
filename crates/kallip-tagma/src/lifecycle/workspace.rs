//! Workspace directory-lock protocol: acquire/establish guards and their
//! failure mapping shared by create, restore, and reactivation.

use std::path::PathBuf;

use kallip_common::agentid::AgentId;
use kallip_common::protocol::ApiError;
use kallip_runtime::config::{AgentConfig, DelegationMode, PermissionClass};

use crate::state::SharedState;

/// RAII guard for the workspace write-lock acquired on every materialization
/// path (create, restore, reactivation) for a Normal agent.
///
/// Releases the lock on `Drop` (every `return Err` -- and any panic -- between
/// acquire and successful registration) unless disarmed. The success path
/// disarms it so the registered agent keeps the lock for its lifetime (it is
/// released later by `remove_agent`/reactivation). This covers the panic case a
/// manual `release_all` at each error return cannot reach.
pub(crate) struct WorkspaceLockGuard<'a> {
    state: &'a SharedState,
    id: &'a AgentId,
    armed: bool,
}

impl WorkspaceLockGuard<'_> {
    /// Disarm so `Drop` no longer releases -- call exactly once, on the success
    /// path once the agent is registered and owns the lock.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorkspaceLockGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.state.lock_manager.release_all(self.id);
        }
    }
}

/// Failure to acquire the workspace write-lock for a Normal agent. Callers
/// apply path-specific policy: create rolls back the agent dir and returns 409;
/// restore bails and the caller registers the agent `Faulted` (its children are
/// still attempted); reactivation refuses to wake the agent into a state where
/// it cannot write its own workspace (returning conflict to the sender).
pub(crate) enum WorkspaceAcquireFailure {
    /// Another agent holds an overlapping write-lock.
    Busy { holder: AgentId, conflict: PathBuf },
    /// The acquire itself errored (e.g. an unresolvable workspace path).
    Other(std::io::Error),
}

/// Acquire the workspace write-lock for a Normal agent (Guests acquire nothing
/// and get `Ok(None)`). Returns an armed [`WorkspaceLockGuard`] on success --
/// its `Drop` releases the lock, so a failure on the caller's path between this
/// call and a successful spawn needs no manual `release_all`. Callers that want
/// the lock to persist past a success point call `guard.disarm()`.
///
/// `chain` is the agent's delegation ancestors (owned ids), so a nested lock
/// held under an ancestor is treated as delegation, not conflict (see
/// [`DirLockManager::acquire`]).
///
/// Pure acquire: NO rollback, NO `ApiError` mapping -- each materialization path
/// decides its own conflict policy. This is the single place the workspace lock
/// is taken, so the "a Normal agent holds a write-lock on its workspace for the
/// lifetime of its task" invariant cannot be bypassed by skipping a call site.
///
/// [`DirLockManager::acquire`]: kallip_runtime::dirlock::DirLockManager::acquire
pub(crate) fn try_acquire_workspace_lock<'a>(
    state: &'a SharedState,
    id: &'a AgentId,
    config: &AgentConfig,
    chain: &[AgentId],
) -> Result<Option<WorkspaceLockGuard<'a>>, WorkspaceAcquireFailure> {
    if config.permissions_class != PermissionClass::Normal {
        return Ok(None);
    }
    match state
        .lock_manager
        .acquire(id, &config.workspace_root, chain)
    {
        Ok(kallip_runtime::dirlock::AcquireOutcome::Acquired)
        | Ok(kallip_runtime::dirlock::AcquireOutcome::AlreadyHeld) => {
            Ok(Some(WorkspaceLockGuard {
                state,
                id,
                armed: true,
            }))
        }
        Ok(kallip_runtime::dirlock::AcquireOutcome::Busy { holder, conflict }) => {
            Err(WorkspaceAcquireFailure::Busy { holder, conflict })
        }
        Err(e) => Err(WorkspaceAcquireFailure::Other(e)),
    }
}

/// The result of [`establish_workspace_lock`]: the workspace write-lock guard
/// and, for a `FullHandoff` spawn, the data to reverse the forward transfer.
///
/// # Drop ordering is structural
///
/// The custom [`Drop`] runs the reverse handoff transfer (if armed) BEFORE the
/// `workspace` field auto-drops. This is load-bearing: the transfer back must
/// run while the dirlock's `writer == child`, before
/// [`WorkspaceLockGuard`]'s `Drop` runs `release_all(child)` and clears the
/// entry. If `release_all` ran first, `writer` would become `None` and the
/// back-transfer would be a `NotOwner` no-op, permanently stranding the lock.
/// Encoding the reverse transfer in the manual `Drop` body (which always runs
/// before field autodrop) makes this previously prose-only invariant
/// structural — every caller that lets an `EstablishedLock` go out of scope on
/// an error path gets the right order for free.
///
/// On the success path the caller calls [`EstablishedLock::disarm`] before the
/// agent is considered registered, then drops the value (a no-op).
pub(crate) struct EstablishedLock<'a> {
    handoff: Option<HandoffRollback<'a>>,
    pub(crate) workspace: Option<WorkspaceLockGuard<'a>>,
}

/// The reverse half of a full-handoff transfer, undone on unwind. A plain
/// struct (no `Drop`): [`EstablishedLock`]'s manual `Drop` performs the
/// transfer so the ordering relative to the workspace guard's release is
/// explicit at one site. `armed` flips to false on the success path.
struct HandoffRollback<'a> {
    state: &'a SharedState,
    child: AgentId,
    supervisor: AgentId,
    ws: std::path::PathBuf,
    armed: bool,
}

impl<'a> EstablishedLock<'a> {
    /// Disarm both the handoff rollback and the workspace guard. Call exactly
    /// once, on the success path after the agent is registered, so the imminent
    /// `Drop` neither reverses the handoff transfer nor releases the workspace
    /// lock.
    pub(crate) fn disarm(&mut self) {
        if let Some(h) = self.handoff.as_mut() {
            h.armed = false;
        }
        if let Some(g) = self.workspace.as_mut() {
            g.disarm();
        }
    }
}

impl Drop for EstablishedLock<'_> {
    fn drop(&mut self) {
        // Reverse the forward handoff transfer WHILE the dirlock writer is still
        // the child, i.e. before the `workspace` field auto-drops and runs
        // `release_all(child)`. Rust runs this manual body before field autodrop.
        if let Some(h) = self.handoff.as_ref()
            && h.armed
        {
            let _ = h
                .state
                .lock_manager
                .transfer(&h.child, &h.supervisor, &h.ws);
        }
    }
}

/// Failure to [`establish_workspace_lock`]. Each variant's doc states whether
/// any dirlock mutation happened before the error, so callers know whether the
/// lock state is intact (the helper itself guarantees the as-if-never-ran
/// contract documented on [`establish_workspace_lock`]). The `Display` impl is
/// the single source for the message prose; callers only pick a status code.
#[derive(Debug)]
pub(crate) enum EstablishLockFailure {
    /// A `FullHandoff` config with no `created_by`. Rejected before any dirlock
    /// mutation, so the state is untouched. (Spawn reaches this only on a
    /// corrupted `meta.json` at restore time; `validate_subagent_request`
    /// guarantees a live supervisor at spawn time.)
    HandoffWithoutSupervisor,
    /// The forward handoff transfer itself errored. Nothing followed it, so
    /// there is nothing to reverse.
    ForwardTransferFailed(std::io::Error),
    /// Another agent holds an overlapping write-lock. The forward transfer (if
    /// any) has already been reversed before this is returned.
    Busy { holder: AgentId, conflict: PathBuf },
    /// The acquire itself errored (e.g. an unresolvable workspace path). The
    /// forward transfer (if any) has already been reversed before this is
    /// returned.
    AcquireFailed(std::io::Error),
}

impl std::fmt::Display for EstablishLockFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HandoffWithoutSupervisor => {
                f.write_str("full-handoff agent has no supervisor (created_by); corrupt meta.json")
            }
            Self::ForwardTransferFailed(io) => {
                write!(f, "full-handoff lock transfer failed: {io}")
            }
            Self::Busy { holder, conflict } => write!(
                f,
                "workspace overlaps a write-lock on {} held by agent {holder}; \
                 remove it or choose a non-overlapping workspace",
                conflict.display()
            ),
            Self::AcquireFailed(io) => write!(f, "failed to acquire workspace lock: {io}"),
        }
    }
}

/// Execute the workspace carve-out that every materialization path shares:
/// the `FullHandoff` forward transfer, the workspace write-lock acquire, and the
/// reverse-transfer rollback guard. This is the single place the carve logic
/// lives, so spawn ([`Materialize::run`](super::spawn::Materialize::run)) and restore (`restore_one`) cannot
/// drift apart as they did when each inlined its own copy.
///
/// # Error contract
///
/// On `Ok`, the dirlock state reflects the child (writer == child for the
/// workspace; a nested lock for a `CarveOut` subdir) and the returned
/// [`EstablishedLock`] holds the rollback guards. On `Err`, the dirlock state is
/// **as-if this call never ran**: a forward transfer that succeeded is reversed
/// before a `Busy`/`AcquireFailed` is returned, and the two pre-mutation
/// variants (`HandoffWithoutSupervisor`, `ForwardTransferFailed`) leave nothing
/// to reverse. The caller is responsible only for *persistence* rollback (the
/// agent dir), not the dirlock.
pub(crate) fn establish_workspace_lock<'a>(
    state: &'a SharedState,
    id: &'a AgentId,
    config: &AgentConfig,
    chain: &[AgentId],
) -> Result<EstablishedLock<'a>, EstablishLockFailure> {
    let is_handoff = config.delegation_mode == DelegationMode::FullHandoff;
    // Resolve the supervisor id once. `HandoffWithoutSupervisor` returns before
    // any mutation; this replaces the prior `.expect` that crashed restore on a
    // corrupted meta.
    let supervisor_id: Option<AgentId> = match (is_handoff, config.created_by.as_ref()) {
        (true, None) => return Err(EstablishLockFailure::HandoffWithoutSupervisor),
        (true, Some(s)) => Some(s.clone()),
        (false, _) => None,
    };

    // Forward handoff: transfer the supervisor's root lock to the child BEFORE
    // the acquire, so the child's acquire sees writer==child (AlreadyHeld),
    // not an exact-path Busy against the supervisor. Atomic under the dirlock
    // mutex. A failure here leaves nothing to reverse.
    if let Some(s) = &supervisor_id {
        state
            .lock_manager
            .transfer(s, id, &config.workspace_root)
            .map_err(EstablishLockFailure::ForwardTransferFailed)?;
    }

    // Acquire. On failure, reverse the forward transfer (if any) so the
    // supervisor keeps its lock rather than the discarded child id.
    let workspace = match try_acquire_workspace_lock(state, id, config, chain) {
        Ok(guard) => guard,
        Err(WorkspaceAcquireFailure::Busy { holder, conflict }) => {
            if let Some(s) = &supervisor_id {
                let _ = state.lock_manager.transfer(id, s, &config.workspace_root);
            }
            return Err(EstablishLockFailure::Busy { holder, conflict });
        }
        Err(WorkspaceAcquireFailure::Other(e)) => {
            if let Some(s) = &supervisor_id {
                let _ = state.lock_manager.transfer(id, s, &config.workspace_root);
            }
            return Err(EstablishLockFailure::AcquireFailed(e));
        }
    };

    // Rollback for failures AFTER this point (spawn/registration). Reversed by
    // `EstablishedLock`'s manual `Drop` before the workspace guard releases.
    let handoff = supervisor_id.map(|s| HandoffRollback {
        state,
        child: id.clone(),
        supervisor: s,
        ws: config.workspace_root.clone(),
        armed: true,
    });

    Ok(EstablishedLock { handoff, workspace })
}

/// Map an [`EstablishLockFailure`] to a client-facing [`ApiError`]. The dirlock
/// is already restored by [`establish_workspace_lock`] on every error variant,
/// so the caller only needs to roll back persistence (the agent dir). Only the
/// status code is chosen here; the message prose comes from the `Display` impl.
///
/// Note: the `ApiError::internal` arms log their message server-side and return
/// a generic `"internal error"` to the client (see `ApiError::internal`) -- the
/// prose there is for operators, not the HTTP body.
pub(crate) fn establish_lock_api_error(e: EstablishLockFailure) -> ApiError {
    use EstablishLockFailure::*;
    match &e {
        Busy { .. } => ApiError::conflict(e.to_string()),
        AcquireFailed(_) => ApiError::bad_request(e.to_string()),
        HandoffWithoutSupervisor | ForwardTransferFailed(_) => ApiError::internal(e.to_string()),
    }
}

/// Map an exec-gate WRITE failure (a carve-out refused because the supervisor
/// has an in-flight shell) to a client-facing conflict. The carve is retried by
/// the caller once the supervisor is quiescent.
pub(crate) fn exec_gate_failure(failure: kallip_runtime::ExecGateFailure) -> ApiError {
    use kallip_runtime::ExecGateFailure;
    let msg = match failure {
        ExecGateFailure::ForegroundExecInProgress => {
            "supervisor has an in-flight shell; wait for it to finish and retry".to_string()
        }
        ExecGateFailure::BgTasksRunning(n) => format!(
            "supervisor has {n} running background task(s); wait for them to finish and retry"
        ),
    };
    ApiError::conflict(msg)
}
