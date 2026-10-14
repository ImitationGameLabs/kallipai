//! Workspace placement: relocation, parked retargeting and ancestor
//! resolution.

use axum::Json;
use axum::extract::{Path, State};
use kallipai_adk::config::PermissionClass;
use kallipai_adk::persistence;
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::{
    ApiError, RelocateRequest, Relocated, WorkspaceRootUpdateRequest, WorkspaceRootUpdated,
};

use super::park::{
    DeactivateErr, RestoreStepErr, deactivate_core, restore_boot, restore_reactivate,
};
use crate::state::SharedState;

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

/// Reserved for the operator or the root agent — the same mutation authority
/// the converge face grants (see `crate::auth::is_operator_or_root`).
async fn authorize_relocation(
    state: &SharedState,
    identity: &crate::auth::Identity,
) -> Result<(), ApiError> {
    if crate::auth::is_operator_or_root(state, identity).await {
        return Ok(());
    }
    Err(ApiError::forbidden(
        "workspace relocation is reserved for the operator or the root agent",
    ))
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
