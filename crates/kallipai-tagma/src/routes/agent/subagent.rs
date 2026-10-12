//! Subagent spawn and profile-set rebinding.

use axum::Json;
use axum::extract::{Path, State};
use kallipai_adk::config::{AgentConfig, PermissionClass};
use kallipai_adk::persistence;
use kallipai_common::agentid::AgentId;
use kallipai_common::authtoken::MintedToken;
use kallipai_common::protocol::{ApiError, ProfileSetUpdateRequest};
use tracing::info;

use super::validate_subagent_request;
use crate::state::{AgentSummary, SharedState};
use crate::token::AGENT;

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
        crate::routes::profiles::signal_profile_reset(reset, &cell, &notify);
    }
    info!(agent = %id, set = %body.profile_set, "agent rebound to profile set");
    Ok(Json(summary))
}
