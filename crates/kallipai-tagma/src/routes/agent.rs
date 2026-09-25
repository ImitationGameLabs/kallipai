//! The agent-management HTTP face: handlers for creating, listing,
//! interrupting, and removing agents — thin shells over `crate::lifecycle`
//! (where the spawn domain now lives) and the registry. `permissions` stays
//! here as the permission-endpoint domain.
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallipai_common::agentid::AgentId;
use kallipai_common::authtoken::MintedToken;
use kallipai_common::policy::ExecPolicy;
use kallipai_common::protocol::ApiError;
use kallipai_runtime::config::{
    AgentConfig, DelegationMode, PermissionClass, permission_class_from_env,
};
use kallipai_runtime::persistence;
use serde::Deserialize;
use tracing::{debug, error, info, warn};

use kallipai_common::protocol::{
    CreateAgentRequest, CreateAgentResponse, ListAgentsQuery, ProfileSetUpdateRequest,
    UpdateActivityRequest, UpdateAgentMetadataRequest,
};

use super::ListAgentsResponse;
use crate::state::{AgentState, AgentSummary, SharedState};
use crate::token::AGENT;

/// `POST /agents` — spawn a **subagent** under an existing supervisor.
///
/// The tagma's single root agent is tagma-managed (eagerly created at startup
/// by [`ensure_root_agent`]); it cannot be created over HTTP. A request with no
/// `created_by` is rejected with `409 Conflict`. Subagent creation requires
/// operator or direct-supervisor privilege and is gated by the per-supervisor
/// `max_subagents` limit.
pub async fn create_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // The root is tagma-managed; clients spawn subagents with `created_by` set.
    let Some(supervisor_id) = req.created_by else {
        return Err(ApiError::conflict(
            "root agent is managed by the tagma; spawn a subagent with 'created_by' set",
        ));
    };

    let mut config = {
        let ws = std::path::PathBuf::from(&req.workspace_root);
        AgentConfig::load(req.prompt, req.skills, Some(ws))
            .map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    if let Some(rounds) = req.max_tool_rounds {
        match rounds {
            kallipai_common::protocol::MaxToolRounds::Unlimited => {
                config.set_max_tool_rounds(usize::MAX);
            }
            kallipai_common::protocol::MaxToolRounds::Limited(n) => {
                if n == 0 {
                    return Err(ApiError::bad_request(
                        "max_tool_rounds must be greater than zero",
                    ));
                }
                config.set_max_tool_rounds(n);
            }
        }
    }
    config.role = req.role.clone();
    config.description = req.description.clone();
    config.delegation_mode = req
        .delegation_mode
        .as_deref()
        .unwrap_or(kallipai_common::protocol::DELEGATION_CARVE_OUT)
        .parse::<kallipai_runtime::config::DelegationMode>()
        .map_err(ApiError::bad_request)?;
    config.profile_set = Some(req.profile_set.clone());
    // Fleet discipline: a subagent spawn must carry a non-empty role so a
    // superior can tell its subagents apart.
    if config.role.trim().is_empty() {
        return Err(ApiError::bad_request(
            "subagent requires a non-empty 'role'",
        ));
    }

    // Reject any workspace that overlaps the tagma data tree BEFORE reserving
    // the subagent slot, so a rejected workspace leaves no dangling slot.
    // (`validate_subagent_request` already confines a subagent's workspace within
    // its supervisor's, which is itself disjoint, so this is a backstop here; it
    // is load-bearing for the tagma-owned root, where it is checked in
    // `ensure_root_agent`.)
    persistence::ensure_workspace_disjoint(&config.workspace_root)
        .map_err(|e| ApiError::conflict(e.to_string()))?;

    // Parse the class request once, before taking the lock, so a bad
    // spelling is a cheap client-side 400 rather than a held-write-lock
    // rejection. The tagma is the reference monitor: the value is accepted
    // only as a downgrade inside validate_subagent_request.
    let requested_class = parse_requested_class(&req.permission_class)?;
    let id = spawn_subagent(
        &state,
        auth.identity(),
        SubagentSpawn {
            supervisor_id,
            config,
            requested_class,
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(CreateAgentResponse { id })))
}
/// The shared subagent-spawn core behind `POST /agents` and converge's
/// spawn action: mint the identity, run the reference monitor (role
/// uniqueness, supervisor validation, slot reservation — all under one
/// registry write lock so a concurrent same-role spawn cannot slip
/// through the gap), then hand the prepared config to
/// [`crate::lifecycle::Materialize`]. Callers own the request-shaped
/// config build and the permission-class parse; everything below them
/// is lifecycle mechanics this function owns, so the two entry points
/// cannot drift on reservation or rollback semantics.
pub(crate) struct SubagentSpawn {
    /// The supervisor the new agent is created under. Converge always
    /// uses the tagma root; the HTTP face passes the request's
    /// `created_by`.
    pub supervisor_id: AgentId,
    /// Request-shaped config: workspace, role, description,
    /// delegation mode, profile set, skills, prompt. `agent_id`,
    /// `created_by`, `permissions`, and `permissions_class` are
    /// filled in here.
    pub config: AgentConfig,
    /// The parsed permission class request — a caller spelling error
    /// is a cheap `400` at the parse site, not a held-write-lock
    /// rejection here.
    pub requested_class: PermissionClass,
}

pub(crate) async fn spawn_subagent(
    state: &SharedState,
    identity: &crate::auth::Identity,
    req: SubagentSpawn,
) -> Result<AgentId, ApiError> {
    let id = AgentId::random();
    // Mint a fresh 256-bit `sk-agent-…` token. The plaintext goes into the agent
    // shell env (`KALLIPAI_AUTH_TOKEN`); only its SHA-256 is indexed for auth lookup.
    let token = MintedToken::generate(AGENT);
    let mut config = req.config;
    config.agent_id = Some(id.clone());

    // Reject any workspace that overlaps the tagma data tree BEFORE reserving
    // the subagent slot, so a rejected workspace leaves no dangling slot.
    // (`validate_subagent_request` already confines a subagent's workspace within
    // its supervisor's, which is itself disjoint, so this is a backstop here; it
    // is load-bearing for the tagma-owned root, where it is checked in
    // `ensure_root_agent`.)
    persistence::ensure_workspace_disjoint(&config.workspace_root)
        .map_err(|e| ApiError::conflict(e.to_string()))?;

    // Subagent head: validate supervisor + delegation constraints and pre-reserve
    // the slot under write lock to eliminate TOCTOU. The tagma-global preset
    // applies to every agent, so only the per-agent exec-policy is resolved here.
    let exec_policy = {
        let mut registry = state.registry.write().await;
        // Roles are the readable addressing alias across the registry, so a
        // duplicate would make role addressing ambiguous; reject before the
        // slot is reserved, naming the holder. Inside the same write lock as
        // the reservation: a separate read-lock pass could race a concurrent
        // same-role spawn through the gap between the two locks.
        if let Some(holder) = registry
            .iter()
            .find(|(_, e)| e.identity().config.role == config.role)
            .map(|(id, _)| id.clone())
        {
            return Err(ApiError::conflict(format!(
                "role '{}' is already held by agent {holder}",
                config.role
            )));
        }
        let (permissions, exec, permission_class) = validate_subagent_request(
            &registry,
            identity,
            &req.supervisor_id,
            &config.workspace_root,
            req.requested_class,
            config.delegation_mode,
        )?;
        // Check per-agent subagent limit and pre-reserve the slot.
        let supervisor = registry
            .get_mut(&req.supervisor_id)
            .ok_or_else(|| ApiError::not_found("supervisor not found"))?;
        if supervisor.subagent_ids().len() >= state.max_subagents {
            return Err(ApiError::unavailable(format!(
                "supervisor has {}/{max} subagents, cannot create more",
                supervisor.subagent_ids().len(),
                max = state.max_subagents
            )));
        }
        // Pre-reserve: push the new ID so concurrent requests see the updated count.
        supervisor.subagent_ids_mut().push(id.clone());
        config.created_by = Some(req.supervisor_id.clone());
        config.permissions = permissions;
        config.permissions_class = permission_class;
        exec
    };

    crate::lifecycle::Materialize {
        state,
        id,
        token,
        config,
        exec_policy,
        rollback_supervisor: Some(req.supervisor_id),
    }
    .run()
    .await
}

/// Ensure the tagma's single root agent exists. Called once at startup, after
/// [`restore_agents`](crate::lifecycle::restore_agents) and before the router
/// accepts connections, so the root is always present for clients. Idempotent:
/// if restore already brought the root back (the common case), this is a no-op.
///
/// The root's config is env-driven via [`AgentConfig::load`] — notably
/// `KALLIPAI_WORKSPACE_ROOT` and `KALLIPAI_MAX_TOOL_ROUNDS` — and its permission
/// class comes from `KALLIPAI_ROOT_AGENT_PERMISSION_CLASS`. The root is persisted
/// by the normal spawn path, so its id is stable across restarts.
pub async fn ensure_root_agent(state: &SharedState) -> anyhow::Result<()> {
    {
        let registry = state.registry.read().await;
        if registry.root_agent().is_some() {
            return Ok(());
        }
    }
    // A root on disk that did not make it into the registry means its restore
    // failed (it is registered faulted, with the reason in the startup log).
    // Minting a fresh root here would fork the fleet into two roots with no
    // traceable lineage, so refuse to start instead and point at the failure.
    // In the normal boot order restore_agents already aborted on contradictory
    // disk state (duplicate roots) and a faulted root exited early above; this
    // guard also covers an agents directory that cannot be read at all.
    let disk_root = persistence::find_disk_root().map_err(|e| {
        anyhow::anyhow!("cannot verify whether a disk root exists — refusing to mint: {e:#}")
    })?;
    if let Some(id) = disk_root {
        anyhow::bail!(
            "disk root agent {id} was not restored — refusing to mint a second \\
             root; see the restore failure in the startup log and resolve it \\
             (fix the underlying fault or remove the agent directory)"
        );
    }
    let id = AgentId::random();
    let token = MintedToken::generate(AGENT);
    let mut config = AgentConfig::load(None, Vec::new(), None)?;
    config.agent_id = Some(id.clone());
    config.permissions_class = permission_class_from_env();
    // Bind the root to the default set by name (the root is the default
    // set's consumer by definition; restore re-derives an unbound root
    // record the same way). With no sets configured yet the binding stays
    // empty and Materialize falls back to the unconfigured placeholder.
    let default_set = state.profiles.load().config.default.clone();
    config.profile_set = (!default_set.is_empty()).then_some(default_set);
    config.role = "root".to_string();
    config.description = "Tagma-owned root agent".to_string();
    // Reject any workspace that overlaps the tagma data tree (e.g. a tagma
    // launched from inside its own data dir). Surfaced as a startup error via
    // the `?` in `main.rs`.
    persistence::ensure_workspace_disjoint(&config.workspace_root)
        .map_err(|e| anyhow::anyhow!("root workspace overlaps the data tree: {e}"))?;
    let created = crate::lifecycle::Materialize {
        state,
        id,
        token,
        config,
        exec_policy: ExecPolicy::default(),
        rollback_supervisor: None,
    }
    .run()
    .await?;
    info!(agent = %created, "created tagma root agent");
    Ok(())
}

/// `GET /agents/root` — return the tagma's single root agent. Always present
/// after startup (see [`ensure_root_agent`]); a missing root is a startup
/// invariant violation surfaced as `500`, never `404` (a 404 would invite
/// clients back into the check-then-create pattern this refactor removes).
pub async fn get_root_agent(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
) -> Result<Json<AgentSummary>, ApiError> {
    let conversation_id = state.external.get().and_then(|p| p.conversation_id());
    let registry = state.registry.read().await;
    let (id, entry) = registry
        .root_agent()
        .ok_or_else(|| ApiError::internal("root agent missing — startup invariant violated"))?;
    let mut summary = state.summarize(id, entry);
    summary.conversation_id = conversation_id;
    Ok(Json(summary))
}

/// Verify the caller's bearer matches the named agent id: `204` on match,
/// `401` otherwise. Lets a trusted peer service (e.g. `kallipai-cron`) confirm an
/// `(agent_id, token)` pair a client presented, without that service holding the
/// agent-token index itself. The bearer resolves via the standard
/// `crate::auth::AuthIdentity` path; an operator bearer does not match any agent
/// id.
pub async fn verify_agent(
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, ApiError> {
    match auth.identity() {
        crate::auth::Identity::Agent { id: aid } if *aid == id => Ok(StatusCode::NO_CONTENT),
        _ => Err(ApiError::unauthorized("agent id does not match token")),
    }
}

/// Any authenticated identity (operator or agent) may list agents.
/// The response contains no secrets (only IDs, workspace paths, and state).
/// `?created_by=<id>` optionally restricts the result to a superior's direct
/// subagents. Today the unfiltered list is already unrestricted (any identity
/// sees every agent), so this filter adds no new leakage; revisit the exposure
/// if listing is ever scoped per-caller.
pub async fn list_agents(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Query(query): Query<ListAgentsQuery>,
) -> Json<ListAgentsResponse> {
    let registry = state.registry.read().await;
    let summaries: Vec<AgentSummary> = registry
        .iter()
        .filter(|(_, entry)| {
            query
                .created_by
                .as_ref()
                .is_none_or(|sup| entry.identity().config.created_by.as_ref() == Some(sup))
        })
        .map(|(id, entry)| state.summarize(id, entry))
        .collect();
    Json(ListAgentsResponse { agents: summaries })
}

/// `PUT /agents/{id}/metadata` — update `role` and/or `description`.
///
/// Caller must be the direct supervisor (or operator); a grandparent cannot
/// relabel a grandchild. `None` fields are left unchanged; `role: Some(s)` must
/// be non-empty (role can be changed but not cleared — `description` can be
/// cleared with `Some("")`).
///
/// Persist-first-then-memory, both under one registry **write-lock**. The lock
/// serializes the whole op, which is what makes it correct: `rewrite_meta` is a
/// read-modify-write of `meta.json`, so without the write-lock two concurrent
/// PUTs (e.g. one setting role, one setting description) would lose an update,
/// and a concurrent `remove_agent` could archive the dir mid-write. The
/// write-lock held across one tiny JSON `atomic_write` briefly stalls concurrent
/// readers; that is acceptable for a rare mutation. Crash-safe — restore reads
/// meta as the source of truth.
pub async fn update_metadata(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(body): Json<UpdateAgentMetadataRequest>,
) -> Result<Json<AgentSummary>, ApiError> {
    // An explicit role set must not be empty (role is change-only, never clearable).
    if let Some(role) = &body.role
        && role.trim().is_empty()
    {
        return Err(ApiError::bad_request("'role' must not be empty"));
    }
    if body.role.is_none() && body.description.is_none() {
        return Err(ApiError::bad_request("no fields to update"));
    }

    let mut registry = state.registry.write().await;
    registry.require_direct_supervisor(auth.identity(), &id)?;
    // A rename must not collide with another agent's role (addressing stays
    // unambiguous); scanned under the same write-lock as the update, excluding
    // the renaming agent itself.
    if let Some(new_role) = &body.role
        && let Some(holder) = registry
            .iter()
            .find(|(other, e)| *other != &id && e.identity().config.role == *new_role)
            .map(|(other, _)| other.clone())
    {
        return Err(ApiError::conflict(format!(
            "role '{new_role}' is already held by agent {holder}"
        )));
    }
    let entry = registry
        .get_mut(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    let agent_dir = entry
        .identity()
        .agent_dir
        .clone()
        .ok_or_else(|| ApiError::internal("agent has no on-disk directory to update"))?;

    // Persist first (disk is the source of truth across restarts), then memory.
    persistence::rewrite_meta(
        &agent_dir,
        body.role.as_deref(),
        body.description.as_deref(),
        None,
        None, // permissions_class: not updatable through this route
    )
    .map_err(ApiError::internal)?;
    if let Some(role) = &body.role {
        entry.identity_mut().config.role = role.clone();
    }
    if let Some(desc) = &body.description {
        entry.identity_mut().config.description = desc.clone();
    }
    // role/description land in the projected roster summary; wake the
    // snapshot pumps — this raw get_mut flip bypasses the stores'
    // embedded senders, so the explicit call is the invalidation.
    state.invalidate();
    let summary = state.summarize(&id, entry);
    Ok(Json(summary))
}

/// `PUT /agents/{id}/activity` — the agent reports its current activity.
///
/// Self-only (or operator): the agent sets its own activity; a supervisor does
/// not (it observes via `list`). Writes the live cell — a registry **read-lock**
/// is enough because the cell is an interior-mutable `Arc<Mutex<String>>`.
/// Truncated to [`MAX_ACTIVITY_CHARS`] on a char boundary; an empty string
/// clears it (the bridge also auto-clears on terminal events).
pub async fn update_activity(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(body): Json<UpdateActivityRequest>,
) -> Result<StatusCode, ApiError> {
    let registry = state.registry.read().await;
    registry.require_self_or_operator(auth.identity(), &id)?;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    let live = entry
        .as_live()
        .ok_or_else(|| ApiError::conflict("agent is faulted; cannot report activity"))?;
    let mut activity = body.activity;
    truncate_chars(&mut activity, MAX_ACTIVITY_CHARS);
    *live
        .agent
        .activity
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = activity;
    Ok(StatusCode::NO_CONTENT)
}

/// Request body for `PUT /agents/{id}/duty`.
#[derive(Debug, Deserialize)]
pub struct UpdateDutyRequest {
    /// Duty status (serialized as "onduty"/"offduty").
    pub status: crate::duty::DutyStatus,
}

/// `PUT /agents/{id}/duty` — set an agent's duty status (on/off).
///
/// When off-duty, external messages are buffered to the agent's inbox instead
/// of being delivered. This is the manual override; the scheduling engine
/// drives it automatically. If an active work schedule exists, the engine
/// may override this manual setting on the next transition. Operator-only.
pub async fn update_duty(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(body): Json<UpdateDutyRequest>,
) -> Result<Json<AgentSummary>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    let duty_status = body.status;

    // Build the summary under the read lock, then drop it before any
    // await that might need a write lock (enqueue_prompt's reactivation
    // path calls registry.write() — holding the read lock here would
    // self-deadlock).
    let summary = {
        let registry = state.registry.read().await;
        let entry = registry
            .get(&id)
            .ok_or_else(|| ApiError::not_found("agent not found"))?;
        let mut s = state.summarize(&id, entry);
        s.duty = duty_status;
        s
    };

    state.duty.set(id.clone(), duty_status);
    info!(id = %id, duty = ?duty_status, "duty status updated");

    // When transitioning to OnDuty, notify the agent so it pulls buffered
    // messages from its inbox. No separate flush step needed: the agent's
    // notify arm calls pull_undelivered() which drains all in one atomic call.
    if duty_status == crate::duty::DutyStatus::OnDuty {
        let notify = {
            let registry = state.registry.read().await;
            registry
                .get(&id)
                .and_then(|e| e.as_live())
                .map(|l| l.agent.notify.clone())
        };
        if let Some(notify) = notify {
            notify.notify_one();
        }
    }
    Ok(Json(summary))
}

/// Maximum activity length (chars). Longer inputs are truncated, not rejected,
/// so a report never fails the agent's turn.
const MAX_ACTIVITY_CHARS: usize = 256;

/// Truncate `s` to at most `max` chars in place, on a char boundary.
fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() > max {
        s.truncate(s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len()));
    }
}

pub async fn remove_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, ApiError> {
    let entry = {
        let mut registry = state.registry.write().await;
        // The tagma-owned singleton root is non-removable while *live* — clients
        // target subagents. A *faulted* root (e.g. a restore failure) is
        // removable so the operator can recover through the API: archiving it
        // frees the slot, and the next tagma restart re-creates a fresh root
        // (`ensure_root_agent`). Key on identity + liveness, not on
        // `created_by == None`, so faulted duplicate roots also stay removable.
        if registry
            .root_agent()
            .is_some_and(|(root_id, entry)| root_id == &id && entry.as_live().is_some())
        {
            return Err(ApiError::conflict(
                "root agent is tagma-managed and cannot be removed",
            ));
        }
        registry.require_superior(auth.identity(), &id)?;
        let Some(entry) = registry.get(&id) else {
            return Err(ApiError::not_found("agent not found"));
        };
        // Live agents must be quiescent-ish and have no subagents: busy is
        // the only rejected state (interrupt aborts the in-flight round;
        // removal then succeeds). Waiting/retrying park in the outer loop
        // with no round token to interrupt, so removal — whose lifecycle
        // cancel is honored in every state — is their deterministic exit,
        // not a dead-end 409 pointing at a no-op interrupt. Faulted agents
        // skip the state check; the no-subagents check still applies.
        if let Some(live) = entry.as_live()
            && live.agent.get_state() == AgentState::Busy
        {
            return Err(ApiError::conflict("agent is busy, interrupt it first"));
        }
        if !entry.subagent_ids().is_empty() {
            return Err(ApiError::conflict(
                "agent has active subagents, remove or interrupt them first",
            ));
        }
        // Unregister under the same write lock — should always succeed since
        // `get` above confirmed the agent exists. Defensive fallback in case
        // the invariant is violated by a future refactor.
        match registry.unregister(&id) {
            Some(e) => e,
            None => {
                return Err(ApiError::internal("agent vanished during removal"));
            }
        }
    };

    // The shared teardown tail (see `teardown_agent`) returns the
    // workspace lock to the supervisor, releases the child's locks,
    // and cancels the task; the only removal-specific act left here
    // is the final archive.
    teardown_agent(&state, &id, entry).await;
    if let Err(e) = persistence::archive_agent_dir(&id) {
        warn!(id = %id, "agent dir archive failed: {e:#}");
    }
    info!(id = %id, "archived agent");
    Ok(StatusCode::NO_CONTENT)
}

/// The shared teardown tail of every path that ends an agent's
/// lifecycle — `remove_agent` (archive) and converge's deactivate
/// (inactive-area park) — extracted so the two cannot drift: the
/// FullHandoff lock transfer back to the supervisor, the release of
/// every directory write-lock, the duty and inbox clearing, and the
/// graceful cancel+shutdown of the live task. The caller has already
/// unregistered the entry under the write lock and owns what happens
/// to the directory afterwards; this only tears the running agent
/// down. Faulted entries have no task: the helper logs and moves on,
/// matching the removal path's behavior.
pub(crate) async fn teardown_agent(
    state: &SharedState,
    id: &AgentId,
    entry: crate::state::RegistryEntry,
) {
    // FullHandoff: return the workspace lock to the supervisor before the
    // child's locks are released. The transfer reassigns the ws entry from the
    // child to the supervisor; `release_all(child)` below then clears any OTHER
    // locks the child held (the ws entry is now the supervisor's, so it is
    // untouched). `NotOwner` (supervisor gone / lock already released) is
    // expected on racy removal -- logged, not fatal.
    if entry.identity().config.delegation_mode == DelegationMode::FullHandoff
        && let Some(supervisor_id) = entry.identity().config.created_by.as_ref()
    {
        match state.lock_manager.transfer(
            id,
            supervisor_id,
            &entry.identity().config.workspace_root,
        ) {
            Ok(kallipai_runtime::dirlock::TransferOutcome::Transferred) => {}
            Ok(kallipai_runtime::dirlock::TransferOutcome::NotOwner) => {
                debug!(
                    child = %id,
                    supervisor = %supervisor_id,
                    "full-handoff transfer-back was NotOwner (lock already released or supervisor gone)"
                );
            }
            Err(e) => {
                warn!(child = %id, supervisor = %supervisor_id, "full-handoff transfer-back failed: {e}");
            }
        }
    }
    // Release all of this agent's directory write-locks (coupled to task death,
    // not registry removal — see DirLockManager invariants). A no-op for
    // faulted entries, which never acquired locks.
    state.lock_manager.release_all(id);
    // Clear the duty status entry — the agent is gone.
    state.duty.remove(id);
    if let Some(store) = state.inboxes.get() {
        store.clear_for(id).await;
    }

    match entry {
        crate::state::RegistryEntry::Live(live) => {
            // Signal graceful cancellation; the agent persists on its way out.
            live.agent.cancel.cancel();

            // The agent is idle, so its tasks finish in milliseconds: the agent task
            // persists and returns (dropping its sender), and the bridge exits on
            // channel-close (see `crate::bridge::bridge_task`). Await real completion
            // under a bound; force-abort only if a task is stuck.
            let bound = Duration::from_secs(crate::shutdown::REMOVE_AGENT_SHUTDOWN_TIMEOUT_SECS);
            if !live.agent.shutdown(bound).await {
                error!(id = %id, "agent did not shut down in time, force-aborted");
            }
        }
        crate::state::RegistryEntry::Faulted(_) => {
            // No task to cancel or await.
            info!(id = %id, "tearing down faulted agent (no task to shut down)");
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
        round.cancel();
    }
    if let Some(prompt_tx) = parked_interrupt
        && prompt_tx
            .try_send("[system] you were interrupted while parked in the outer loop (waiting or retrying). The wait/retry fuse was dropped; finish with break, or continue if the interruption was unintended.".to_string())
            .is_err()
    {
        return Err(ApiError::conflict("agent prompt queue is full"));
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

/// PUT /agents/{id}/profile-set — rebind the agent to a named profile set.
///
/// Operator or a superior only; the name must resolve in the current
/// registry (unknown names list the available sets). The record persists to
/// meta.json first, then memory. A live agent additionally gets a
/// ProfileReset signal — its failover chain swaps on the next wake-up;
/// parked/faulted agents only get the record change (restore resolves it
/// when they next come up).
pub async fn update_profile_set(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(body): Json<ProfileSetUpdateRequest>,
) -> Result<Json<AgentSummary>, ApiError> {
    let bundle = state.profiles.load();
    let Some(set) = bundle.config.sets.get(&body.profile_set) else {
        return Err(ApiError::bad_request(format!(
            "unknown profile set '{}'; available: {}",
            body.profile_set,
            bundle
                .config
                .sets
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    let mut registry = state.registry.write().await;
    registry.require_superior(auth.identity(), &id)?;
    let entry = registry
        .get_mut(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    let agent_dir = entry
        .identity()
        .agent_dir
        .clone()
        .ok_or_else(|| ApiError::internal("agent has no on-disk directory to update"))?;
    // Persist first (disk is the source of truth across restarts), then memory.
    persistence::rewrite_meta(&agent_dir, None, None, Some(&body.profile_set), None)
        .map_err(ApiError::internal)?;
    entry.identity_mut().config.profile_set = Some(body.profile_set.clone());
    // A live agent swaps its failover chain on its next wake-up — signal
    // outside the registry write-lock (the cell is a sync mutex; the apply
    // path keeps the same separation). Parked/faulted agents resolve the
    // new binding at restore.
    let mut signal = None;
    let summary = state.summarize(&id, entry);
    if let Some(live) = entry.as_live() {
        signal = Some((
            kallipai_runtime::ProfileReset {
                set: set.clone(),
                registry: bundle.registry.clone(),
            },
            live.agent.pending_profile_reset.clone(),
            live.agent.notify.clone(),
        ));
    }
    drop(registry);
    if let Some((reset, cell, notify)) = signal {
        super::profiles::signal_profile_reset(reset, &cell, &notify);
    }
    info!(agent = %id, set = %body.profile_set, "agent rebound to profile set");
    Ok(Json(summary))
}

mod permissions;
#[cfg(test)]
pub(crate) use permissions::resolve_granted_class;
pub(crate) use permissions::{parse_requested_class, validate_subagent_request};
#[cfg(test)]
mod tests;
