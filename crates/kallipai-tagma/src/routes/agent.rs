//! The agent-management HTTP face: handlers for creating, listing,
//! interrupting, and removing agents — thin shells over `crate::lifecycle`
//! (where the spawn domain now lives) and the registry. `permissions` stays
//! here as the permission-endpoint domain.
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallipai_adk::config::{
    AgentConfig, DelegationMode, PermissionClass, permission_class_from_env,
};
use kallipai_adk::persistence;
use kallipai_common::agentid::AgentId;
use kallipai_common::authtoken::MintedToken;
use kallipai_common::facts::EndpointHealth;
use kallipai_common::policy::ExecPolicy;
use kallipai_common::protocol::ApiError;
use serde::Deserialize;
use tracing::{debug, error, info, warn};

use kallipai_common::protocol::{
    CreateAgentRequest, CreateAgentResponse, ListAgentsQuery, ProfileSetUpdateRequest,
    RelocateRequest, Relocated, UpdateActivityRequest, UpdateAgentMetadataRequest,
    WorkspaceRootUpdateRequest, WorkspaceRootUpdated,
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
        .parse::<kallipai_adk::config::DelegationMode>()
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
    Json(ListAgentsResponse {
        agents: summaries,
        upstream_facts: state
            .endpoint_health
            .snapshot(kallipai_common::timefmt::now_epoch()),
    })
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

/// `PUT /agents/{id}/workspace-root` — retarget a deactivated agent's
/// workspace record.
///
/// Deactivated-only by design: the body must live in the inactive area, and a
/// live body for the same id is a double-body hazard that refuses the
/// route. The orchestration around this mutation is the relocation
/// runbook: deactivate, move the files (externally — git semantics stay
/// out of the tagma), this route, then re-restore. Validation fails
/// closed before the single meta write; once it lands the new path is
/// the truth and rollback is the same runbook executed the other way.
///
/// The `POST /agents/{id}/relocate` orchestration runs the same validation
/// through [`retarget_parked_workspace`], so the primitive and the
/// one-command form cannot drift.
pub async fn update_workspace_root(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(body): Json<WorkspaceRootUpdateRequest>,
) -> Result<Json<WorkspaceRootUpdated>, ApiError> {
    authorize_relocation(&state, auth.identity()).await?;

    let (old, new) = retarget_parked_workspace(&state, &id, &body.workspace_root, false).await?;
    Ok(Json(WorkspaceRootUpdated {
        agent_id: id.to_string(),
        old_workspace_root: old,
        workspace_root: new,
    }))
}

/// `POST /agents/{id}/relocate` — the one-command workspace move: park
/// the agent, retarget the recorded workspace root, restore. With
/// `with_files`, a plain directory move runs after the park (rename
/// semantics: the source disappears; cross-device moves fail without a
/// copy fallback), so the destructive step never touches a live agent.
///
/// Every step probes the current shape before acting, so a retried
/// request resumes from wherever the previous one stopped. A refusal at
/// the park gate (busy, live subagents) leaves the agent untouched; a
/// failure past the gate leaves it deactivated (cold), never
/// half-restored, and a failed restore re-parks the body — identity is
/// never replaced here.
pub async fn relocate_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(body): Json<RelocateRequest>,
) -> Result<Json<Relocated>, ApiError> {
    authorize_relocation(&state, auth.identity()).await?;
    let mut steps = Vec::new();

    // Resolve the current workspace record first: the live body's meta
    // when one exists, the deactivated body's meta otherwise. Both the
    // move step and the response's old-path field key off it.
    let current_meta = if persistence::agent_dir(&id)
        .map_err(ApiError::internal)?
        .is_dir()
    {
        persistence::read_meta(&id).map_err(ApiError::internal)?
    } else {
        let inactive = persistence::inactive_dir(&id).map_err(ApiError::internal)?;
        if !inactive.is_dir() {
            return Err(ApiError::not_found(
                "no live or deactivated body for this agent id",
            ));
        }
        persistence::read_meta_from_dir(&inactive).map_err(ApiError::internal)?
    };
    let old_workspace_root = current_meta.workspace_root.display().to_string();

    // Park: a live body goes through the shared deactivate gates (busy
    // and live-subagent refusals); an already-deactivated body skips
    // ahead. A live directory with no registry entry (a torn-down
    // leftover) parks directly — the gates are registry-shaped and there
    // is nothing left to interrupt.
    if persistence::agent_dir(&id)
        .map_err(ApiError::internal)?
        .is_dir()
    {
        let live_in_registry = {
            let registry = state.registry.read().await;
            registry.get(&id).is_some()
        };
        if live_in_registry {
            deactivate_core(&state, &id, false)
                .await
                .map_err(|e| match e {
                    DeactivateErr::Busy(id) => ApiError::conflict(format!(
                        "agent {id} is busy; interrupt it first (kallip subagent interrupt)"
                    )),
                    DeactivateErr::HasSubagents(id) => ApiError::conflict(format!(
                        "agent {id} has live subagents; deactivate or remove them first"
                    )),
                    DeactivateErr::Failed(detail) => ApiError::conflict(detail),
                })?;
            steps.push("deactivated into the inactive area".to_string());
        } else {
            persistence::deactivate_agent_dir(&id)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            state.invalidate();
            steps.push("parked the live body (no registry entry)".to_string());
        }
    } else {
        let inactive = persistence::inactive_dir(&id).map_err(ApiError::internal)?;
        if !inactive.is_dir() {
            return Err(ApiError::not_found(
                "no live or deactivated body for this agent id",
            ));
        }
        steps.push("already deactivated; resuming".to_string());
    }

    if body.with_files {
        let target = std::path::PathBuf::from(&body.workspace_root);
        // Canonical compare first: a target that already IS the recorded
        // root (moved, recorded, or the same path spelled differently)
        // needs no move.
        if target.canonicalize().ok().as_ref() == Some(&current_meta.workspace_root) {
            steps.push(format!(
                "move: target {} already holds the workspace",
                target.display()
            ));
        } else {
            let src_there = current_meta.workspace_root.is_dir();
            let dst_there = target.exists();
            match (src_there, dst_there) {
                (true, false) => {
                    std::fs::rename(&current_meta.workspace_root, &target).map_err(|e| {
                        ApiError::bad_request(format!(
                            "cannot move {} to {}: {e}",
                            current_meta.workspace_root.display(),
                            target.display()
                        ))
                    })?;
                    steps.push(format!(
                        "moved {} to {}",
                        current_meta.workspace_root.display(),
                        target.display()
                    ));
                }
                (false, true) => {
                    steps.push("move: source gone, target present (already moved)".to_string());
                }
                (true, true) => {
                    return Err(ApiError::bad_request(format!(
                        "both the recorded workspace {} and the target {} exist; \
                         a plain move needs the target absent (resolve manually)",
                        current_meta.workspace_root.display(),
                        target.display()
                    )));
                }
                (false, false) => {
                    return Err(ApiError::bad_request(format!(
                        "neither the recorded workspace {} nor the target {} exists; \
                         nothing to move",
                        current_meta.workspace_root.display(),
                        target.display()
                    )));
                }
            }
        }
    }

    // Retarget the record (shared validation; the relocate form also
    // refuses an empty target directory — the files-not-moved mistake).
    let (old_record, new_record) =
        retarget_parked_workspace(&state, &id, &body.workspace_root, true).await?;
    if old_record == new_record {
        steps.push(format!("workspace root already {new_record}"));
    } else {
        steps.push(format!("workspace root: {old_record} -> {new_record}"));
    }

    // Restore: the same boot path the tagma's own restart uses. The
    // system prompt is rebuilt from the new path, so the agent's prefix
    // cache does not carry over — expected, not a fault.
    let role = current_meta.role.clone();
    let (dir, meta) = restore_reactivate(&state, &id, &role)
        .await
        .map_err(restore_err)?;
    restore_boot(&state, &id, &dir, meta, &role)
        .await
        .map_err(restore_err)?;
    steps.push("restored".to_string());

    Ok(Json(Relocated {
        agent_id: id.to_string(),
        old_workspace_root,
        workspace_root: new_record,
        steps,
    }))
}

/// Both restore-core failures mean the same thing to relocate: the body
/// stays parked (or is reported as needing manual cleanup) and the
/// command can be retried; identity is never replaced.
fn restore_err(e: RestoreStepErr) -> ApiError {
    match e {
        RestoreStepErr::Degraded(notes) => ApiError::conflict(notes.join("; ")),
        RestoreStepErr::Failed(detail) => ApiError::conflict(detail),
    }
}

/// Shared validation and single meta write behind a workspace retarget:
/// eligibility (a deactivated body in the inactive area, no live double),
/// boundary checks (the target exists as a directory, workspace
/// disjointness, the supervisor nesting predicate, a lock-conflict probe,
/// the worktree gitdir self-consistency check), then the one atomic truth
/// switch. A target that already equals the recorded root is a no-op, so
/// the call is idempotent.
///
/// `require_nonempty` is the relocate-only foolproofing: a fresh empty
/// directory at the target almost always means the files were not moved
/// yet, so the orchestrated form refuses before the record flips; the
/// primitive stays permissive for deliberate fresh-directory use.
async fn retarget_parked_workspace(
    state: &SharedState,
    id: &AgentId,
    new_root: &str,
    require_nonempty: bool,
) -> Result<(String, String), ApiError> {
    // Eligibility: a deactivated body must exist, and no live double.
    let inactive = persistence::inactive_dir(id).map_err(ApiError::internal)?;
    if !inactive.is_dir() {
        return Err(ApiError::not_found("no deactivated body for this agent id"));
    }
    if persistence::agent_dir(id)
        .map_err(ApiError::internal)?
        .is_dir()
    {
        return Err(ApiError::conflict(
            "agent has a live body; only deactivated agents can be relocated",
        ));
    }

    let meta = persistence::read_meta_from_dir(&inactive).map_err(ApiError::internal)?;
    let old = meta.workspace_root.display().to_string();

    // Root bodies are excluded: no supervisor means the nesting predicate
    // has no anchor, and a root workspace change is a deployment change.
    let Some(supervisor_id) = meta.created_by.clone() else {
        return Err(ApiError::bad_request(
            "the root agent's workspace is not relocatable",
        ));
    };

    // Canonical domain from here on. canonicalize succeeds for plain files
    // too, so the directory check is explicit (a file target must not
    // reach the write point).
    let new_canon = std::path::PathBuf::from(new_root)
        .canonicalize()
        .map_err(|_| ApiError::bad_request("workspace_root must be an existing directory"))?;
    if !new_canon.is_dir() {
        return Err(ApiError::bad_request(
            "workspace_root must be an existing directory",
        ));
    }
    persistence::ensure_workspace_disjoint(&new_canon)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    // Walk the supervisor chain once: the immediate supervisor's recorded
    // workspace anchors the nesting predicate, and the full id chain lets
    // the lock pre-check read ancestor locks as delegation — exactly as
    // the restore-time acquire will see them.
    let mut chain_ids = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut current = Some(supervisor_id.clone());
    let mut supervisor_ws = None;
    while let Some(link) = current {
        if !visited.insert(link.clone()) {
            return Err(ApiError::internal("circular supervisor chain"));
        }
        let (created_by, ws) = resolve_ancestor(state, &link).await?;
        if supervisor_ws.is_none() {
            supervisor_ws = Some(ws);
        }
        chain_ids.push(link);
        current = created_by;
    }
    let supervisor_ws = supervisor_ws.expect("created_by was Some, so the chain is non-empty");
    if !new_canon.starts_with(&supervisor_ws) {
        return Err(ApiError::bad_request(format!(
            "workspace outside supervisor boundary ({})",
            supervisor_ws.display()
        )));
    }

    // Lock pre-check: an early, well-labeled version of the collision the
    // restore-time acquire would raise. Not enforcement — the in-memory
    // lock set can change between here and restore; restore remains the
    // authority and its failure semantics cover the race.
    // Guest deactivated bodies skip the probe: restore never acquires a lock
    // for a Guest, so a probe hit there would be a false positive.
    if meta.permissions_class == PermissionClass::Normal
        && let Some((holder, conflict)) = state
            .lock_manager
            .probe_conflict(id, &new_canon, &chain_ids)
            .map_err(ApiError::internal)?
    {
        return Err(ApiError::conflict(format!(
            "workspace {new_canon:?} would collide with the lock held by {holder} on {conflict:?}"
        )));
    }

    // Worktree self-consistency: the NEW workspace carrying a .git file
    // must point at a live gitdir. `git worktree move` rewrites the file;
    // a dangling pointer means the workspace and its git registration
    // disagree, and the move is not done.
    let git_file = new_canon.join(".git");
    if git_file.is_file() {
        let raw = std::fs::read_to_string(&git_file).map_err(ApiError::internal)?;
        let Some(gitdir) = raw.lines().find_map(|l| l.strip_prefix("gitdir: ")) else {
            return Err(ApiError::bad_request(
                "body carries a malformed .git file (no gitdir line); finish `git worktree move` first",
            ));
        };
        let gitdir_path = std::path::PathBuf::from(gitdir.trim());
        let resolved = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            new_canon.join(gitdir_path)
        };
        if !resolved.is_dir() {
            return Err(ApiError::bad_request(
                "body carries a .git file whose gitdir does not resolve; finish `git worktree move` first",
            ));
        }
    }

    // All checks above still ran: an already-settled target only skips
    // the write itself (an idempotent retry), never the validation.
    if new_canon == meta.workspace_root {
        let settled = new_canon.display().to_string();
        return Ok((settled.clone(), settled));
    }

    if require_nonempty
        && new_canon
            .read_dir()
            .map_err(ApiError::internal)?
            .next()
            .is_none()
    {
        return Err(ApiError::bad_request(
            "target workspace is empty; move the agent's files there first",
        ));
    }

    // The single truth switch: one atomic write flips old to new.
    let updated =
        persistence::update_agent_workspace(&inactive, &new_canon).map_err(ApiError::internal)?;
    Ok((old, updated.workspace_root.display().to_string()))
}

/// Create a workspace directory, idempotently. Spawn needs the directory
/// to exist before registration; restore recreates a vanished workspace
/// (including parents) rather than waking an agent that cannot write
/// its own workspace.
pub(crate) fn ensure_workspace_dir(ws: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(ws)
        .map_err(|e| format!("cannot create workspace {}: {e}", ws.display()))
}

/// The id-targeted deactivate sequence shared by converge's batch face and
/// the relocate orchestration: refuse a busy (non-idle, non-parked-wait)
/// agent, refuse live subagents, unregister, tear down, and park the body
/// in the inactive area. `force` interrupts a busy target first.
///
/// The refusal details stay uncontextualized so each caller can phrase
/// them for its own face (a converge row says "batch stopped"; relocate
/// says "interrupt it first").
pub(crate) enum DeactivateErr {
    /// The target is mid-round: `{0}` is the agent id.
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
    let entry = {
        let mut registry = state.registry.write().await;
        let Some(entry) = registry.get(id) else {
            return Err(DeactivateErr::Failed(format!(
                "agent {id} vanished mid-operation"
            )));
        };
        if let Some(live) = entry.as_live() {
            let busy = !matches!(
                live.agent.get_state(),
                crate::state::AgentState::Idle | crate::state::AgentState::Parked
            );
            if busy && !force {
                return Err(DeactivateErr::Busy(id.to_string()));
            }
            if busy {
                notes.push(
                    "force escape: the busy target was interrupted before teardown".to_string(),
                );
            }
        }
        if !entry.subagent_ids().is_empty() {
            return Err(DeactivateErr::HasSubagents(id.to_string()));
        }
        registry.unregister(id).ok_or_else(|| {
            DeactivateErr::Failed(format!("agent {id} vanished during unregister"))
        })?
    };
    if force {
        // Interrupt before teardown so an in-flight round aborts cleanly
        // instead of racing the cancel.
        let _ = interrupt_core(state, id).await;
    }
    teardown_agent(state, id, entry).await;
    persistence::deactivate_agent_dir(id).map_err(|e| {
        DeactivateErr::Failed(format!(
            "agent {id} is unregistered but the park failed: {e:#}"
        ))
    })?;
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
    {
        let registry = state.registry.read().await;
        if registry.get(id).is_some() {
            return Err(RestoreStepErr::Failed(format!(
                "agent {id} came alive mid-operation"
            )));
        }
        if let Some((holder, _)) = registry
            .iter()
            .find(|(_, e)| e.identity().config.role == role)
        {
            return Err(RestoreStepErr::Failed(format!(
                "role {role:?} is now held by agent {holder}"
            )));
        }
    }
    let inactive_dir = match persistence::inactive_dir(id) {
        Ok(dir) => dir,
        Err(e) => {
            return Err(RestoreStepErr::Degraded(vec![format!(
                "inactive body unreadable: {e:#}"
            )]));
        }
    };
    let meta = match persistence::read_meta_from_dir(&inactive_dir) {
        Ok(meta) => meta,
        Err(e) => {
            return Err(RestoreStepErr::Degraded(vec![format!(
                "the inactive body's metadata is unreadable ({e:#})"
            )]));
        }
    };
    // A deactivated body whose recorded workspace vanished must fail
    // rather than wake unable to write its own workspace.
    if let Err(e) = ensure_workspace_dir(&meta.workspace_root) {
        return Err(RestoreStepErr::Failed(e));
    }
    if let Err(e) = persistence::reactivate_agent_dir(id) {
        return Err(RestoreStepErr::Failed(format!(
            "could not move the inactive body back to the live area: {e:#}"
        )));
    }
    let dir = match persistence::agent_dir(id) {
        Ok(dir) => dir,
        Err(e) => {
            return Err(RestoreStepErr::Failed(format!(
                "live dir unresolved: {e:#}"
            )));
        }
    };
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
                let mut detail = "registry changed during the restore (role or id taken); the body was re-parked".to_string();
                if parked.is_err() {
                    detail.push_str(" (re-park failed; manual cleanup needed)");
                }
                return Err(RestoreStepErr::Failed(detail));
            }
            registry.register(rid, crate::state::RegistryEntry::Live(entry));
            drop(registry);
            state.invalidate();
            Ok(())
        }
        Err(e) => {
            // The body cannot come up: park it back (best effort) so the
            // failure surfaces with the identity intact and parked. A
            // failed re-park is surfaced in the notes, never swallowed.
            let mut loss = vec![format!("restore failed ({e:#}); the body was re-parked")];
            if let Err(park) = persistence::deactivate_agent_dir(id) {
                loss.push(format!(
                    "old body re-park failed; manual cleanup needed ({park:#})"
                ));
            }
            Err(RestoreStepErr::Degraded(loss))
        }
    }
}

/// The operator or the root agent — the same mutation authority as the
/// converge face (see `authorize_converge`). A parked agent has no
/// registry entry, so chain-based authorization is unavailable and the
/// declaration owner is the right residual holder.
async fn authorize_relocation(
    state: &SharedState,
    identity: &crate::auth::Identity,
) -> Result<(), ApiError> {
    match identity {
        crate::auth::Identity::Operator => Ok(()),
        crate::auth::Identity::Agent { id } => {
            let registry = state.registry.read().await;
            if registry
                .root_agent()
                .is_some_and(|(root_id, _)| root_id == id)
            {
                Ok(())
            } else {
                Err(ApiError::forbidden(
                    "workspace relocation is reserved for the operator or the root agent",
                ))
            }
        }
    }
}

/// Resolve one supervisor-chain link to its (created_by, workspace_root)
/// pair: live registry first, then the active and inactive on-disk bodies
/// (a parked supervisor is a legitimate anchor). A link missing on every
/// surface is a 500 — chain corruption, not absence.
async fn resolve_ancestor(
    state: &SharedState,
    id: &AgentId,
) -> Result<(Option<AgentId>, std::path::PathBuf), ApiError> {
    {
        let registry = state.registry.read().await;
        if let Some(entry) = registry.get(id) {
            let cfg = &entry.identity().config;
            return Ok((cfg.created_by.clone(), cfg.workspace_root.clone()));
        }
    }
    // Active surface: present-but-corrupt meta is chain corruption (a 500
    // naming the surface), NOT absence — never silently fall through to
    // the inactive face, which could resolve a stale twin.
    let active = persistence::agent_dir(id).map_err(ApiError::internal)?;
    if active.is_dir() {
        match persistence::read_meta_from_dir(&active) {
            Ok(meta) => {
                return Ok((meta.created_by, meta.workspace_root));
            }
            Err(e) => {
                return Err(ApiError::internal(format!(
                    "supervisor {id} active meta unreadable: {e}"
                )));
            }
        }
    }
    // Inactive surface: same discrimination.
    let parked = persistence::inactive_dir(id).map_err(ApiError::internal)?;
    if parked.is_dir() {
        match persistence::read_meta_from_dir(&parked) {
            Ok(meta) => {
                return Ok((meta.created_by, meta.workspace_root));
            }
            Err(e) => {
                return Err(ApiError::internal(format!(
                    "supervisor {id} inactive meta unreadable: {e}"
                )));
            }
        }
    }
    Err(ApiError::internal(format!(
        "supervisor {id} metadata not found in registry, active or inactive areas"
    )))
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
            Ok(kallipai_adk::dirlock::TransferOutcome::Transferred) => {}
            Ok(kallipai_adk::dirlock::TransferOutcome::NotOwner) => {
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
    // A dead enrollment token refuses new bindings: handing an agent a
    // chain through a known-dead token just burns a failover chain at
    // first use. A successful refresh (the signal consumer) clears the
    // poison.
    state
        .profile_source
        .load_full()
        .source()
        .ensure_usable()
        .await
        .map_err(|error| ApiError::conflict(error.to_string()))?;
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
            kallipai_adk::ProfileReset {
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
