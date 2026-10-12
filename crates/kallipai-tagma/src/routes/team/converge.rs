//! The declarative write face of the team domain.
//!

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use kallipai_common::AgentId;
use kallipai_common::declaration::parse_declaration;
use kallipai_common::protocol::{
    ApiError, TeamAction, TeamConvergeOutcome, TeamConvergeRequest, TeamConvergeResponse,
    TeamInactiveAgent, TeamLockEntry,
};

use super::status::{parse_lock_pairs, probe_lock_pairs};
use crate::state::SharedState;

// The planning, preflight and execution halves live in submodules. The
// imports and re-exports below carry this module's paths and `pub(super)`
// visibility for `team` and its tests, which reach the items here.
use plan::{authorize_converge, snapshot_live};

mod execute;
mod plan;
mod preflight;

pub(super) use execute::execute_converge;
pub(super) use plan::{PlannedAction, plan_converge};
pub(super) use preflight::preflight_converge;

// Test-only names, reached through the suite's `use super::converge::*`.
#[cfg(test)]
pub(super) use execute::{RestoreFallout, restore_action};
#[cfg(test)]
pub(super) use plan::{AlignItems, LiveBody};

/// `POST /team/converge` — the declarative write face: plan (three-way
/// diff), preflight (whole-batch refusal on structural problems), then
/// execute (fail-fast; every action re-reads its target before landing).
///
/// The plan is built from one registry snapshot; execution re-reads every
/// target under its own lock, so the only race window is between an
/// action's re-read and its landing — and a violation there stops the
/// batch with the applied rows reported. A crash mid-execution is
/// absorbed by re-running: converge re-plans from reality, and applied
/// rows re-derive to no-ops, so converging again repairs a crashed run.
pub(super) async fn team_converge(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<TeamConvergeRequest>,
) -> Result<Json<TeamConvergeResponse>, ApiError> {
    if req.file.is_empty() {
        return Err(ApiError::bad_request(
            "'file' must name the declaration file",
        ));
    }
    // The declaration is read in this process: a caller-relative path
    // would resolve against the tagma's cwd, not the caller's shell.
    if !std::path::Path::new(&req.file).is_absolute() {
        return Err(ApiError::bad_request(
            "declaration file path must be absolute; the tagma resolves no caller-relative paths",
        ));
    }
    authorize_converge(&state, auth.identity()).await?;
    // Global mutual exclusion: a second converge is refused, never queued —
    // a collision means look at the current state, not wait behind a stale
    // plan. `try_lock` cannot wedge: the guard drops when the run ends.
    let _permit = state
        .converge
        .try_lock()
        .map_err(|_| ApiError::conflict("converge already in progress"))?;

    // Same read/parse contract as the status face (shared parser, shared
    // refusal shapes), and the same lock-pair spelling.
    let raw = std::fs::read_to_string(&req.file)
        .map_err(|_| ApiError::bad_request("cannot read the declaration file"))?;
    let declaration =
        parse_declaration(&raw).map_err(|_| ApiError::bad_request("invalid team declaration"))?;
    let pairs = match &req.lock {
        Some(raw) => parse_lock_pairs(raw)?,
        None => Vec::new(),
    };
    let lock = probe_lock_pairs(pairs);
    let (root, live) = snapshot_live(&state).await;

    let plan = plan_converge(&declaration, &lock, root.as_ref(), &live);

    // Seed the response mapping with every input record that survived
    // planning (drifted/archived/missing records are out, annotated on
    // their rows). Full-set semantics: dormant records stay.
    let plan_by_role: HashMap<&str, &PlannedAction> =
        plan.iter().map(|a| (a.role.as_str(), a)).collect();
    let stamped = kallipai_common::timefmt::format_utc(kallipai_common::timefmt::now_epoch());
    let mut mapping: Vec<TeamLockEntry> = lock
        .iter()
        .filter(|p| {
            plan_by_role
                .get(p.role.as_str())
                .is_none_or(|a| !a.discard_record)
        })
        .map(|p| TeamLockEntry {
            role: p.role.clone(),
            id: p.id.clone(),
            converged_at: stamped.clone(),
        })
        .collect();

    let rejections = preflight_converge(&state, &plan, &root, &lock, &live, req.force);
    if !rejections.is_empty() {
        return Ok(Json(TeamConvergeResponse {
            declaration_path: req.file,
            dry_run: req.dry_run,
            outcome: TeamConvergeOutcome::Rejected,
            plan: plan.iter().map(PlannedAction::to_wire).collect(),
            results: Vec::new(),
            rejections,
            lock: mapping,
            inactive_agents: None,
        }));
    }

    if req.dry_run {
        // The dry-run mapping is the post-converge set minus planned
        // spawns, whose ids are only minted at execution.
        project_mapping(&mut mapping, &plan, None, &stamped);
        return Ok(Json(TeamConvergeResponse {
            declaration_path: req.file,
            dry_run: true,
            outcome: TeamConvergeOutcome::Planned,
            plan: plan.iter().map(PlannedAction::to_wire).collect(),
            results: Vec::new(),
            rejections: Vec::new(),
            lock: mapping,
            inactive_agents: None,
        }));
    }

    let (results, aborted) =
        execute_converge(&state, &plan, &mut mapping, &root, req.force, &stamped).await;
    // Post-execution snapshot, inside the same mutex window: a body this
    // run deactivated is already in the inactive area and IS in the
    // listing, matching the lock the response carries. Scan failure
    // degrades to None (advisory consumer) — never a 500.
    let inactive_agents = fill_inactive_agents();
    let outcome = if aborted {
        TeamConvergeOutcome::Aborted
    } else {
        TeamConvergeOutcome::Applied
    };
    Ok(Json(TeamConvergeResponse {
        declaration_path: req.file,
        dry_run: req.dry_run,
        outcome,
        plan: plan.iter().map(PlannedAction::to_wire).collect(),
        results,
        rejections: Vec::new(),
        lock: mapping,
        inactive_agents,
    }))
}

/// The post-execution inactive-area snapshot: scan mapped to the wire
/// shape, degrading to `None` on a scan failure (the consumer is
/// advisory; see the response field's doc). Extracted so the degrade
/// path is testable without a full converge run.
pub(super) fn fill_inactive_agents() -> Option<Vec<TeamInactiveAgent>> {
    match kallipai_adk::persistence::scan_inactive() {
        Ok(entries) => Some(
            entries
                .into_iter()
                .map(|(id, meta)| TeamInactiveAgent {
                    id,
                    role: meta.role,
                })
                .collect::<Vec<_>>(),
        ),
        Err(e) => {
            tracing::warn!("inactive-agent scan failed while filling the converge response: {e:#}");
            None
        }
    }
}

/// Upsert one role→id binding into the mapping (the record replaces any
/// earlier binding for the role).
fn upsert_mapping(mapping: &mut Vec<TeamLockEntry>, role: &str, id: &AgentId, stamped: &str) {
    if let Some(entry) = mapping.iter_mut().find(|e| e.role == role) {
        entry.id = id.clone();
        entry.converged_at = stamped.to_string();
    } else {
        mapping.push(TeamLockEntry {
            role: role.to_string(),
            id: id.clone(),
            converged_at: stamped.to_string(),
        });
    }
}

/// Fold a plan into the mapping without executing it: keep/adopt/align
/// rows contribute their known ids; spawn rows contribute only when a
/// post-execution id map is supplied (dry runs never have one).
pub(super) fn project_mapping(
    mapping: &mut Vec<TeamLockEntry>,
    plan: &[PlannedAction],
    spawned: Option<&HashMap<String, AgentId>>,
    stamped: &str,
) {
    for a in plan {
        match a.action {
            TeamAction::Spawn => {
                if let Some(map) = spawned
                    && let Some(id) = map.get(&a.role)
                {
                    upsert_mapping(mapping, &a.role, id, stamped);
                }
            }
            TeamAction::Restore | TeamAction::Adopt | TeamAction::Deactivate => {
                if let Some(id) = &a.target {
                    upsert_mapping(mapping, &a.role, id, stamped);
                }
            }
            TeamAction::AlignMetadata | TeamAction::Retain => {}
        }
    }
}
