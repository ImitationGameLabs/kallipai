//! Row-order application of a planned converge, stopping at the first failure.

use std::collections::HashMap;
use std::str::FromStr;

use kallipai_adk::config::AgentConfig;
use kallipai_common::AgentId;
use kallipai_common::protocol::{
    DELEGATION_CARVE_OUT, TeamAction, TeamActionResult, TeamLockEntry, TeamRowOutcome,
};

use super::plan::{LiveBody, PlannedAction};
use super::upsert_mapping;
use crate::state::SharedState;

/// Execute the plan in row order, fail-fast: each action re-reads its
/// target first (the plan→execute window is converge's one
/// blind spot), and any violation or failure stops the batch with the
/// applied rows reported. Returns the result rows and whether the run
/// aborted early.
pub(crate) async fn execute_converge(
    state: &SharedState,
    plan: &[PlannedAction],
    mapping: &mut Vec<TeamLockEntry>,
    root: &Option<LiveBody>,
    force: bool,
    stamped: &str,
) -> (Vec<TeamActionResult>, bool) {
    let mut results = Vec::new();
    let mut spawned: HashMap<String, AgentId> = HashMap::new();
    // Spawns run under operator identity: converge was already authorized,
    // and validate_subagent_request accepts the operator as spawner for
    // any supervisor.
    let identity = crate::auth::Identity::Operator;
    let root_arg = root.as_ref().map(|r| (&r.id, r.workspace_root.as_path()));

    // Deactivations run first: a net-zero batch (spawn N, deactivate N)
    // passes the net capacity preflight, so the live per-spawn checks must
    // never see a transient over-limit count. One role has exactly one
    // plan row, so reordering cannot create dependencies between rows.
    let (deactivates, rest): (Vec<_>, Vec<_>) = plan
        .iter()
        .partition(|a| a.action == TeamAction::Deactivate);
    for a in deactivates.iter().chain(rest.iter()) {
        match a.action {
            TeamAction::Retain => {}
            TeamAction::Spawn => match spawn_action(state, a, identity.clone(), root_arg).await {
                Ok(id) => {
                    spawned.insert(a.role.clone(), id.clone());
                    upsert_mapping(mapping, &a.role, &id, stamped);
                    results.push(row_ok(a, Some(id.clone()), format!("spawned agent {id}")));
                }
                Err(detail) => {
                    results.push(row_failed(a, None, detail));
                    return (results, true);
                }
            },
            TeamAction::Restore => match restore_action(state, a).await {
                RestoreFallout::Restored { id, notes } => {
                    upsert_mapping(mapping, &a.role, &id, stamped);
                    let mut all = a.notes.clone();
                    all.extend(notes);
                    results.push(result_row(
                        a,
                        Some(id.clone()),
                        TeamRowOutcome::Applied,
                        format!("restored agent {id} identity-intact"),
                        all,
                    ));
                }
                RestoreFallout::Degraded { id, notes } => {
                    upsert_mapping(mapping, &a.role, &id, stamped);
                    spawned.insert(a.role.clone(), id.clone());
                    let mut all = a.notes.clone();
                    all.extend(notes);
                    results.push(result_row(
                        a,
                        Some(id.clone()),
                        TeamRowOutcome::Applied,
                        format!("restore degraded into a fresh spawn: agent {id}"),
                        all,
                    ));
                }
                RestoreFallout::Failed(detail) => {
                    results.push(row_failed(a, None, detail));
                    return (results, true);
                }
            },
            TeamAction::Adopt => {
                let Some(id) = a.target.clone() else { continue };
                match align_live(state, a).await {
                    Ok(notes) => {
                        upsert_mapping(mapping, &a.role, &id, stamped);
                        let mut all = a.notes.clone();
                        all.extend(notes);
                        let detail = if a.align.is_empty() {
                            format!("adopted live agent {id}")
                        } else {
                            format!("adopted live agent {id}; metadata aligned")
                        };
                        results.push(result_row(
                            a,
                            Some(id),
                            TeamRowOutcome::Applied,
                            detail,
                            all,
                        ));
                    }
                    Err(detail) => {
                        results.push(row_failed(a, Some(id), detail));
                        return (results, true);
                    }
                }
            }
            TeamAction::AlignMetadata => {
                let Some(id) = a.target.clone() else { continue };
                match align_live(state, a).await {
                    Ok(notes) => {
                        let mut all = a.notes.clone();
                        all.extend(notes);
                        results.push(result_row(
                            a,
                            Some(id),
                            TeamRowOutcome::Applied,
                            "metadata aligned to the declaration".to_string(),
                            all,
                        ));
                    }
                    Err(detail) => {
                        results.push(row_failed(a, Some(id), detail));
                        return (results, true);
                    }
                }
            }
            TeamAction::Deactivate => {
                let Some(id) = a.target.clone() else { continue };
                match deactivate_action(state, a, force).await {
                    Ok(notes) => {
                        upsert_mapping(mapping, &a.role, &id, stamped);
                        let mut all = a.notes.clone();
                        all.extend(notes);
                        results.push(result_row(
                            a,
                            Some(id.clone()),
                            TeamRowOutcome::Applied,
                            format!("deactivated agent {id} into the inactive area"),
                            all,
                        ));
                    }
                    Err(detail) => {
                        results.push(row_failed(a, Some(id), detail));
                        return (results, true);
                    }
                }
            }
        }
    }
    (results, false)
}

fn row_ok(a: &PlannedAction, id: Option<AgentId>, detail: String) -> TeamActionResult {
    result_row(a, id, TeamRowOutcome::Applied, detail, a.notes.clone())
}

fn row_failed(a: &PlannedAction, id: Option<AgentId>, detail: String) -> TeamActionResult {
    result_row(a, id, TeamRowOutcome::Failed, detail, a.notes.clone())
}

#[allow(clippy::too_many_arguments)]
fn result_row(
    a: &PlannedAction,
    id: Option<AgentId>,
    outcome: TeamRowOutcome,
    detail: String,
    notes: Vec<String>,
) -> TeamActionResult {
    TeamActionResult {
        role: a.role.clone(),
        action: a.action,
        agent_id: id,
        outcome,
        detail,
        notes,
    }
}

/// How a restore action landed. A damaged restore turns into a fresh
/// spawn (the identity loss is loud, never silent); anything else that
/// cannot land stops the batch.
pub(crate) enum RestoreFallout {
    /// The inactive body came back identity-intact.
    Restored { id: AgentId, notes: Vec<String> },
    /// The body was unusable; a fresh replacement was spawned (the
    /// mapping binds the role to the new id).
    Degraded { id: AgentId, notes: Vec<String> },
    /// The action failed; the batch stops.
    Failed(String),
}

/// Spawn one declared role under the tagma root: derived workspace
/// (`<root workspace>/team/<role>`), declaration prompt and skills,
/// carve-out delegation. The reference monitor inside `spawn_subagent`
/// re-checks role uniqueness under the write lock, so a same-role body
/// that raced in during the plan→execute window fails this action and
/// stops the batch.
async fn spawn_action(
    state: &SharedState,
    a: &PlannedAction,
    identity: crate::auth::Identity,
    root: Option<(&AgentId, &std::path::Path)>,
) -> Result<AgentId, String> {
    let Some((root_id, root_ws)) = root else {
        return Err("the tagma root is not live".to_string());
    };
    let Some(d) = &a.declared else {
        return Err("spawn row without a declaration entry".to_string());
    };
    let ws = root_ws.join("team").join(&a.role);
    // The workspace must exist before AgentConfig pins it and the body
    // registers; creation is idempotent for already-present roots.
    crate::routes::agent::ensure_workspace_dir(&ws)?;
    let mut config = AgentConfig::load(d.prompt.clone(), d.skills.clone(), Some(ws.clone()))
        .map_err(|e| e.to_string())?;
    config.role = a.role.clone();
    config.description = d.description.clone();
    config.delegation_mode = DELEGATION_CARVE_OUT
        .parse::<kallipai_adk::config::DelegationMode>()
        .map_err(|e| e.to_string())?;
    config.profile_set = d.profile_set.clone();
    let requested_class = crate::routes::agent::parse_requested_class(
        d.permission_class.as_deref().unwrap_or("normal"),
    )
    .map_err(|e| e.to_string())?;
    crate::routes::agent::spawn_subagent(
        state,
        &identity,
        crate::routes::agent::SubagentSpawn {
            supervisor_id: root_id.clone(),
            config,
            requested_class,
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// Restore the lock-recorded body from the inactive area: reactivate the
/// directory, align declared metadata onto it (same round), run the boot
/// restore path, register under a role-uniqueness re-check.
pub(crate) async fn restore_action(state: &SharedState, a: &PlannedAction) -> RestoreFallout {
    let Some(id) = a.target.clone() else {
        return RestoreFallout::Failed("restore row without a lock id".to_string());
    };
    let (dir, mut meta) = match crate::routes::agent::restore_reactivate(state, &id, &a.role).await
    {
        Ok(v) => v,
        Err(crate::routes::agent::RestoreStepErr::Degraded(notes)) => {
            // The core reports the damage neutrally; only converge
            // replaces the body, so the replacement context lands here.
            let mut loss = vec!["IDENTITY LOSS".to_string()];
            loss.extend(notes);
            return degraded(state, a, &id, loss).await;
        }
        Err(crate::routes::agent::RestoreStepErr::Failed(detail)) => {
            return RestoreFallout::Failed(format!("{detail}; batch stopped"));
        }
    };
    // Same-round alignment on the reactivated body, before the config rebuild:
    // the restored agent comes up already conforming to the declaration.
    let mut notes = Vec::new();
    if let Some(d) = &a.declared {
        let mut desc = None;
        let mut pset = None;
        let mut class = None;
        if !d.description.is_empty() && d.description != meta.description {
            desc = Some(d.description.clone());
            meta.description = d.description.clone();
            notes.push("aligned description".to_string());
        }
        if let Some(want) = &d.profile_set
            && meta.profile_set.as_deref() != Some(want.as_str())
        {
            pset = Some(want.clone());
            meta.profile_set = Some(want.clone());
            notes.push("aligned profile_set".to_string());
        }
        if let Some(want) = &d.permission_class {
            match kallipai_adk::config::PermissionClass::from_str(want) {
                Ok(want_class) if want_class < meta.permissions_class => {
                    class = Some(want_class);
                    meta.permissions_class = want_class;
                    notes.push("downgraded permission_class".to_string());
                }
                Ok(want_class) if want_class > meta.permissions_class => {
                    notes.push(
                        "cannot auto-align permission_class (converge never raises permissions)"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        let meta_write = desc.is_some() || pset.is_some() || class.is_some();
        if meta_write
            && let Err(e) = kallipai_adk::persistence::rewrite_meta(
                &dir,
                None,
                desc.as_deref(),
                pset.as_deref(),
                class,
            )
        {
            let mut loss = vec![format!(
                "IDENTITY LOSS: aligned metadata could not be written ({e:#}); spawned a fresh replacement"
            )];
            if let Err(park) = kallipai_adk::persistence::deactivate_agent_dir(&id) {
                loss.push(format!(
                    "old body re-park failed; manual cleanup needed ({park:#})"
                ));
            }
            return degraded(state, a, &id, loss).await;
        }
        if d.prompt.is_some() {
            notes.push(
                "declaration prompt is not injected on restore (identity-intact)".to_string(),
            );
        }
        if !d.skills.is_empty() {
            notes.push("declared skills apply at spawn; restored context unchanged".to_string());
        }
    }
    match crate::routes::agent::restore_boot(state, &id, &dir, meta, &a.role).await {
        Ok(()) => RestoreFallout::Restored { id, notes },
        Err(crate::routes::agent::RestoreStepErr::Degraded(loss)) => {
            let mut wrapped = vec!["IDENTITY LOSS".to_string()];
            wrapped.extend(loss);
            degraded(state, a, &id, wrapped).await
        }
        Err(crate::routes::agent::RestoreStepErr::Failed(detail)) => {
            RestoreFallout::Failed(format!("{detail}; batch stopped"))
        }
    }
}

/// The damaged-restore landing: a fresh spawn replaces the unusable
/// body; the lock record rebinds; the old body is left exactly as is.
async fn degraded(
    state: &SharedState,
    a: &PlannedAction,
    old_id: &AgentId,
    mut notes: Vec<String>,
) -> RestoreFallout {
    notes.push(format!("the previous body {old_id} was not reused"));
    let root = {
        let registry = state.registry.read().await;
        registry
            .root_agent()
            .map(|(id, e)| (id.clone(), e.identity().config.workspace_root.clone()))
    };
    match spawn_action(
        state,
        a,
        crate::auth::Identity::Operator,
        root.as_ref().map(|(id, ws)| (id, ws.as_path())),
    )
    .await
    {
        Ok(new_id) => RestoreFallout::Degraded { id: new_id, notes },
        Err(detail) => RestoreFallout::Failed(format!(
            "restore degraded to a fresh spawn, and the spawn failed: {detail}"
        )),
    }
}

/// Align one live body to its declaration row (adopt rows and
/// align-metadata rows share this): persist-first meta rewrite under the
/// registry write lock, in-memory config flip, ProfileReset signal for a
/// changed set. Blind-spot re-read: the body must still be live under
/// this role.
async fn align_live(state: &SharedState, a: &PlannedAction) -> Result<Vec<String>, String> {
    let Some(id) = a.target.clone() else {
        return Err("alignment row without a live target".to_string());
    };
    // The blind-spot re-read runs even for an empty align list: adopt
    // rows rebind the lock mapping to this id, so a target that
    // vanished in the plan→execute window must stop the batch, not
    // report Applied against a dead agent.
    if !state.registry.read().await.contains_key(&id) {
        return Err(format!(
            "agent {id} vanished in the plan→execute window; batch stopped"
        ));
    }
    if a.align.is_empty() {
        return Ok(Vec::new());
    }
    let mut signal = None;
    let mut notes = Vec::new();
    {
        let mut registry = state.registry.write().await;
        let Some(entry) = registry.get_mut(&id) else {
            return Err(format!(
                "agent {id} vanished in the plan→execute window; batch stopped"
            ));
        };
        if entry.identity().config.role != a.role {
            return Err(format!(
                "agent {id} no longer holds role {:?}; batch stopped",
                a.role
            ));
        }
        let dir = entry
            .identity()
            .agent_dir
            .clone()
            .ok_or_else(|| "agent has no on-disk directory".to_string())?;
        // Persist first (disk is the source of truth across restarts),
        // then memory — the same commit shape as the PUT routes.
        kallipai_adk::persistence::rewrite_meta(
            &dir,
            None,
            a.align.description.as_deref(),
            a.align.profile_set.as_deref(),
            a.align.permissions_class,
        )
        .map_err(|e| format!("metadata write failed: {e:#}"))?;
        if let Some(d) = &a.align.description {
            entry.identity_mut().config.description = d.clone();
            notes.push("aligned description".to_string());
        }
        if let Some(s) = &a.align.profile_set {
            entry.identity_mut().config.profile_set = Some(s.clone());
            notes.push("aligned profile_set".to_string());
            if let Some(live) = entry.as_live() {
                let bundle = state.profiles.load();
                match bundle.config.sets.get(s) {
                    Some(set) => {
                        signal = Some((
                            kallipai_adk::ProfileReset {
                                set: set.clone(),
                                registry: bundle.registry.clone(),
                            },
                            live.agent.pending_profile_reset.clone(),
                            live.agent.notify.clone(),
                        ));
                    }
                    None => {
                        return Err(format!(
                            "profile set {s:?} vanished in the plan→execute window; batch stopped"
                        ));
                    }
                }
            }
        }
        if let Some(c) = a.align.permissions_class {
            entry.identity_mut().config.permissions_class = c;
            notes.push("downgraded permission_class".to_string());
        }
    }
    if let Some((reset, cell, notify)) = signal {
        crate::routes::profiles::signal_profile_reset(reset, &cell, &notify);
    }
    state.invalidate();
    Ok(notes)
}

/// Park one live body into the inactive area: plan→execute window busy/children
/// re-read, unregister, the shared teardown tail, then the rename.
/// `force` interrupts a busy target first and says so in the notes (the
/// auditable escape).
async fn deactivate_action(
    state: &SharedState,
    a: &PlannedAction,
    force: bool,
) -> Result<Vec<String>, String> {
    let Some(id) = a.target.clone() else {
        return Err("deactivation row without a live target".to_string());
    };
    match crate::routes::agent::deactivate_core(state, &id, force).await {
        Ok(notes) => Ok(notes),
        Err(crate::routes::agent::DeactivateErr::Busy(id)) => Err(format!(
            "agent {id} turned busy in the plan→execute window; batch stopped (re-run, or pass force)"
        )),
        Err(crate::routes::agent::DeactivateErr::HasSubagents(id)) => Err(format!(
            "agent {id} gained live subagents in the plan→execute window; batch stopped"
        )),
        Err(crate::routes::agent::DeactivateErr::Failed(detail)) => {
            Err(format!("{detail}; batch stopped"))
        }
    }
}
