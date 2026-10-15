//! Refusals: structural problems and busy targets reject a plan whole.

use std::collections::HashMap;
use std::str::FromStr;

use kallipai_common::AgentId;
use kallipai_common::protocol::{RoleDisposition, TeamAction, TeamRejection, TeamRejectionKind};

use super::plan::{LiveBody, PlannedAction};
use crate::routes::team::status::LockPair;
use crate::state::SharedState;

/// Preflight: the whole-batch refusal pass. Structural problems (an id
/// keyed under two roles, a declared root role, duplicate live bodies,
/// invalid spawn declarations, capacity) and busy/childed deactivation
/// targets reject with per-item guidance; only busy deactivation may be
/// forced, every other rejection stands, zero actions are applied.
pub(crate) fn preflight_converge(
    state: &SharedState,
    plan: &[PlannedAction],
    root: &Option<LiveBody>,
    lock: &[LockPair],
    live: &[LiveBody],
    force: bool,
) -> Vec<TeamRejection> {
    let mut rejections = Vec::new();

    // One id keyed under two roles poisons every later decision
    // (which binding is the identity's?); the lock must be rebuilt.
    let mut seen: HashMap<&AgentId, &str> = HashMap::new();
    for pair in lock {
        if let Some(prev) = seen.get(&pair.id) {
            rejections.push(TeamRejection {
                kind: TeamRejectionKind::LockAmbiguity,
                message: format!(
                    "lock maps agent {} to both {prev:?} and {:?} — run `kallip team lock-rebuild`",
                    pair.id, pair.role
                ),
            });
        } else {
            seen.insert(&pair.id, pair.role.as_str());
        }
    }

    // The root is boot-built and outside the declaration's reach.
    for a in plan {
        if a.role == "root" && a.declared.is_some() {
            rejections.push(TeamRejection {
                kind: TeamRejectionKind::RootRole,
                message: "declaration declares the reserved role \"root\": the root is boot-built and cannot converge; remove the role from the declaration"
                    .to_string(),
            });
        }
    }

    // Duplicate live bodies under one role: a human must retire or rename
    // one — converge never silently picks (that would drop the other's
    // identity binding). The plan rows carry the duplicate finding.
    for a in plan {
        if a.disposition == RoleDisposition::Duplicate {
            rejections.push(TeamRejection {
                kind: TeamRejectionKind::Duplicate,
                message: format!(
                "role {:?} has multiple live bodies — retire or rename one before converging (converge never picks for you)",
                a.role
                ),
            });
        }
    }

    // Deactivation safety: busy targets refuse unless --force (the
    // auditable escape); targets with live children refuse outright
    // (the same registry invariant remove enforces).
    for a in plan {
        if a.action != TeamAction::Deactivate {
            continue;
        }
        let body = a
            .target
            .as_ref()
            .and_then(|id| live.iter().find(|b| &b.id == id));
        if let Some(body) = body {
            if body.busy && !force {
                rejections.push(TeamRejection {
                    kind: TeamRejectionKind::Busy,
                    message: format!(
                    "role {:?} agent {} is busy — wait for it to go idle (re-run), or pass force to tear it down anyway (recorded as an escape)",
                    a.role, body.id
                ),
            });
            }
            if body.children > 0 {
                rejections.push(TeamRejection {
                    kind: TeamRejectionKind::LiveChildren,
                    message: format!(
                    "role {:?} agent {} has {} live subagent(s) — deactivate or remove them first",
                    a.role, body.id, body.children
                ),
            });
            }
        }
        // A stale inactive body under the same id would refuse the rename
        // mid-execution; surface it at preflight with guidance instead.
        if let Some(id) = &a.target
            && let Ok(dir) = kallipai_adk::persistence::inactive_dir(id)
            && dir.is_dir()
        {
            rejections.push(TeamRejection {
                kind: TeamRejectionKind::StaleInactive,
                message: format!(
                "deactivating agent {id} would overwrite a stale inactive body with the same id — resolve it manually first"
            ),
        });
        }
    }

    // Spawn validity: profile set present and known, permission class
    // parseable and grantable (downgrade-only against the root), skills
    // resolvable. A spawn that cannot succeed must fail here, not
    // mid-execution.
    let spawns: Vec<&PlannedAction> = plan
        .iter()
        .filter(|a| a.action == TeamAction::Spawn)
        .collect();
    if !spawns.is_empty() {
        let root_ok = root.as_ref().is_some_and(|r| !r.faulted);
        if !root_ok {
            rejections.push(TeamRejection {
                kind: TeamRejectionKind::SpawnRootDown,
                message:
                    "cannot spawn: the tagma root is not live (declaration roles spawn under it)"
                        .to_string(),
            });
        }
        let bundle = state.profiles.load();
        for a in &spawns {
            let Some(d) = &a.declared else { continue };
            match &d.profile_set {
                None => {
                    rejections.push(TeamRejection {
                        kind: TeamRejectionKind::SpawnProfileSet,
                        message: format!(
                            "role {:?} cannot spawn: the declaration names no profile_set",
                            a.role
                        ),
                    });
                }
                Some(set) if !bundle.config.sets.contains_key(set) => {
                    rejections.push(TeamRejection {
                        kind: TeamRejectionKind::SpawnProfileSet,
                        message: format!(
                            "role {:?}: unknown profile_set {set:?} (available: {})",
                            a.role,
                            bundle
                                .config
                                .sets
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    });
                }
                Some(_) => {}
            }
            if let Some(raw) = &d.permission_class {
                match kallipai_adk::config::PermissionClass::from_str(raw) {
                    Ok(want) => {
                        if let Some(r) = root
                            && want > r.permissions_class
                        {
                            rejections.push(TeamRejection {
                                kind: TeamRejectionKind::SpawnProfileClass,
                                message: format!(
                                "role {:?}: permission_class {raw:?} exceeds the root's own class — converge never grants above the supervisor",
                                a.role
                            ),
                            });
                        }
                    }
                    Err(_) => {
                        rejections.push(TeamRejection {
                            kind: TeamRejectionKind::SpawnProfileClass,
                            message: format!(
                                "role {:?}: permission_class {raw:?} is not a known spelling",
                                a.role
                            ),
                        });
                    }
                }
            }
            for skill in &d.skills {
                if let Err(e) = kallipai_adk::tools::load_skill(skill) {
                    rejections.push(TeamRejection {
                        kind: TeamRejectionKind::SpawnSkill,
                        message: format!(
                            "role {:?}: skill {skill:?} cannot be loaded ({e})",
                            a.role
                        ),
                    });
                }
            }
        }
    }

    // Capacity: the same caps Materialize enforces per-spawn, checked
    // batch-wide so an oversized converge fails before touching anything.
    let count = |verb| plan.iter().filter(|a| a.action == verb).count();
    let spawns_n = count(TeamAction::Spawn);
    let restores_n = count(TeamAction::Restore);
    let deactivates_n = count(TeamAction::Deactivate);
    let projected =
        live.len() as isize + spawns_n as isize + restores_n as isize - deactivates_n as isize;
    if projected > state.max_agents as isize {
        rejections.push(TeamRejection {
            kind: TeamRejectionKind::CapacityAgents,
            message: format!(
                "converge would hold {projected} agents over the limit of {} — shrink the declaration or raise the limit",
                state.max_agents
            ),
        });
    }
    if let Some(r) = root {
        let new_children = spawns_n as isize + restores_n as isize - deactivates_n as isize;
        if r.children as isize + new_children > state.max_subagents as isize {
            rejections.push(TeamRejection {
                kind: TeamRejectionKind::CapacityChildren,
                message: format!(
                "the root would hold {}/{} subagents after converge — deactivate roles or raise the limit",
                r.children as isize + new_children,
                state.max_subagents
            ),
        });
        }
    }
    rejections
}
