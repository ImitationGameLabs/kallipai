//! The declarative write face of the team domain.
//!
use std::collections::HashMap;
use std::str::FromStr;

use axum::Json;
use axum::extract::State;
use kallip_common::AgentId;
use kallip_common::declaration::{RoleDeclaration, TeamDeclaration, parse_declaration};
use kallip_common::protocol::{
    ApiError, DELEGATION_CARVE_OUT, RoleDisposition, TeamAction, TeamActionResult,
    TeamConvergeOutcome, TeamConvergeRequest, TeamConvergeResponse, TeamLockEntry, TeamPlanRow,
    TeamRejection, TeamRejectionKind, TeamRowOutcome,
};
use kallip_runtime::config::AgentConfig;

use super::status::{LiveEntry, LockPair, compare_team, parse_lock_pairs, probe_lock_pairs};
use crate::state::SharedState;

/// Registry snapshot of one live body, reduced to what converge's
/// planning and preflight read: identity fields plus the safety bits
/// (busy, children, faulted). Taken under a single read lock so the
/// whole plan is built from one consistent registry view.
pub(super) struct LiveBody {
    pub(super) id: AgentId,
    pub(super) role: String,
    pub(super) workspace_root: std::path::PathBuf,
    pub(super) description: String,
    pub(super) profile_set: Option<String>,
    pub(super) permissions_class: kallip_runtime::config::PermissionClass,
    pub(super) busy: bool,
    pub(super) children: usize,
    pub(super) faulted: bool,
}

/// Metadata alignment items a plan row would apply, resolved against
/// the declaration at plan time so execution is mechanical.
#[derive(Default)]
pub(super) struct AlignItems {
    pub(super) description: Option<String>,
    pub(super) profile_set: Option<String>,
    /// Downgrades only: converge never raises a class (an upgrade is
    /// an explicit operator action outside converge's scope).
    pub(super) permissions_class: Option<kallip_runtime::config::PermissionClass>,
}

impl AlignItems {
    fn is_empty(&self) -> bool {
        self.description.is_none() && self.profile_set.is_none() && self.permissions_class.is_none()
    }
}

/// One planned action — the plan's internal form. Rows render into
/// [`TeamPlanRow`] for the wire; execution
/// consumes this richer form directly.
pub(super) struct PlannedAction {
    pub(super) role: String,
    pub(super) action: TeamAction,
    /// Live body (adopt/align/deactivate) or lock-recorded id
    /// (restore). `None` for planned spawns.
    pub(super) target: Option<AgentId>,
    pub(super) notes: Vec<String>,
    /// The presence-level verdict that produced this row; preflight and
    /// the wire row both read it instead of parsing notes text.
    pub(super) disposition: RoleDisposition,
    pub(super) declared: Option<RoleDeclaration>,
    pub(super) align: AlignItems,
    /// True when the row's lock record was discarded (drift, archived,
    /// or nowhere on disk): the record must not survive into the
    /// response's lock mapping.
    pub(super) discard_record: bool,
}

impl PlannedAction {
    fn to_wire(&self) -> TeamPlanRow {
        TeamPlanRow {
            role: self.role.clone(),
            action: self.action,
            agent_id: self.target.clone(),
            notes: self.notes.clone(),
            disposition: self.disposition,
        }
    }
}

/// Build the registry snapshot converge plans against: the root body
/// (if live) plus every non-root entry, live and faulted.
async fn snapshot_live(state: &SharedState) -> (Option<LiveBody>, Vec<LiveBody>) {
    let registry = state.registry.read().await;
    let root_id = registry.root_agent().map(|(id, _)| id.clone());
    let mut root = None;
    let mut others = Vec::new();
    for (id, entry) in registry.iter() {
        let identity = entry.identity();
        let body = LiveBody {
            id: id.clone(),
            role: identity.config.role.clone(),
            description: identity.config.description.clone(),
            profile_set: identity.config.profile_set.clone(),
            permissions_class: identity.config.permissions_class,
            workspace_root: identity.config.workspace_root.clone(),
            busy: match entry.as_live() {
                Some(live) => !matches!(
                    live.agent.get_state(),
                    crate::state::AgentState::Idle | crate::state::AgentState::Parked
                ),
                None => false,
            },
            children: entry.subagent_ids().len(),
            faulted: entry.as_live().is_none(),
        };
        if root_id.as_ref() == Some(id) {
            root = Some(body);
        } else {
            others.push(body);
        }
    }
    (root, others)
}

/// Diff one declared role against its live body: what converge would
/// align, and what it refuses to touch silently. Alignment is
/// field-scoped and conservative — empty declaration fields mean
/// "declaration silent", not "clear this".
fn alignment_of(declared: &RoleDeclaration, body: &LiveBody) -> (AlignItems, Vec<String>) {
    let mut items = AlignItems::default();
    let mut notes = Vec::new();
    if !declared.description.is_empty() && declared.description != body.description {
        items.description = Some(declared.description.clone());
    }
    if let Some(want) = &declared.profile_set
        && body.profile_set.as_deref() != Some(want.as_str())
    {
        items.profile_set = Some(want.clone());
    }
    if let Some(want) = &declared.permission_class {
        match kallip_runtime::config::PermissionClass::from_str(want) {
            Ok(want_class) if want_class < body.permissions_class => {
                items.permissions_class = Some(want_class);
            }
            Ok(want_class) if want_class > body.permissions_class => {
                notes.push(format!(
                    "cannot auto-align permission_class ({want} exceeds the live body's class; converge never raises permissions)"
                ));
            }
            Ok(_) => {}
            Err(_) => {
                notes.push(format!(
                    "declaration permission_class {want:?} is not a known spelling; left unchanged"
                ));
            }
        }
    }
    (items, notes)
}

/// The converge plan: the status-face diff vocabulary (via `compare_team` rows)
/// enriched into executable verbs, in the same declaration-order-then-
/// undeclared ordering the status face prints, so the table an operator
/// reads and the plan converge executes cannot drift apart.
pub(super) fn plan_converge(
    declaration: &TeamDeclaration,
    lock: &[LockPair],
    root: Option<&LiveBody>,
    live: &[LiveBody],
) -> Vec<PlannedAction> {
    let entry_live: Vec<LiveEntry> = live
        .iter()
        .map(|b| LiveEntry {
            id: b.id.clone(),
            role: b.role.clone(),
        })
        .collect();
    // The drift pool keeps the root (the invariant the status face
    // documents): a lock record pointing at the root's id must read as
    // drifted, never as "nowhere on disk".
    let mut drift_pool: Vec<LiveEntry> = entry_live.clone();
    if let Some(r) = root {
        drift_pool.push(LiveEntry {
            id: r.id.clone(),
            role: r.role.clone(),
        });
    }
    let status_rows = compare_team(declaration, lock, &entry_live, &drift_pool);
    let lock_by_role: HashMap<&str, &LockPair> =
        lock.iter().map(|p| (p.role.as_str(), p)).collect();
    let mut live_by_role: HashMap<&str, Vec<&LiveBody>> = HashMap::new();
    for b in live {
        live_by_role.entry(b.role.as_str()).or_default().push(b);
    }

    let mut plan = Vec::new();
    for row in &status_rows {
        let pair = lock_by_role.get(row.role.as_str()).copied();
        let bodies: Vec<&LiveBody> = live_by_role
            .get(row.role.as_str())
            .cloned()
            .unwrap_or_default();
        let mut action = PlannedAction {
            role: row.role.clone(),
            action: TeamAction::Retain,
            target: None,
            notes: Vec::new(),
            declared: row.declaration.clone(),
            align: AlignItems::default(),
            disposition: row.disposition,
            discard_record: false,
        };
        // Record hygiene runs first on every row carrying a lock record:
        // drifted, archived, and nowhere-on-disk records are discarded
        // from the mapping with a visible note.
        if row.lock_drift {
            if let Some(p) = pair {
                action.notes.push(format!(
                    "lock record discarded: id {} is live under another role",
                    p.id
                ));
                action.discard_record = true;
            }
        } else if let Some(p) = pair {
            if p.in_archived {
                action.notes.push(format!(
                    "lock record discarded: id {} sits in the archived area — retired bodies are never resurrected",
                    p.id
                ));
                action.discard_record = true;
            } else if !p.in_inactive && bodies.iter().all(|b| b.id != p.id) {
                action.notes.push(format!(
                    "lock record discarded: id {} is not on disk in any area",
                    p.id
                ));
                action.discard_record = true;
            }
        }
        match row.disposition {
            RoleDisposition::Active => {
                let body = bodies[0];
                let supersedes = pair.is_some_and(|p| p.id != body.id);
                if supersedes {
                    // The lock names a different body than the one alive
                    // under the role: reality wins, the live body is
                    // recorded, the parked one stays parked.
                    action.action = TeamAction::Adopt;
                    action.target = Some(body.id.clone());
                    if let Some(p) = pair
                        && p.in_inactive
                    {
                        action.notes.push(format!(
                            "lock record role→{} superseded by the live body; the recorded body stays parked in the inactive area",
                            p.id
                        ));
                    }
                } else if action.discard_record {
                    // Drifted/archived record under a live body: keep the
                    // body, drop the record, adopt the body into the mapping.
                    action.action = TeamAction::Adopt;
                    action.target = Some(body.id.clone());
                } else {
                    // In-sync presence: align metadata or retain.
                    if let Some(d) = &action.declared {
                        let (items, notes) = alignment_of(d, body);
                        action.notes.extend(notes);
                        action.align = items;
                    }
                    action.action = if action.align.is_empty() {
                        TeamAction::Retain
                    } else {
                        TeamAction::AlignMetadata
                    };
                }
            }
            RoleDisposition::Adopt => {
                action.action = TeamAction::Adopt;
                action.target = Some(bodies[0].id.clone());
            }
            RoleDisposition::Restore => {
                action.action = TeamAction::Restore;
                action.target = pair.map(|p| p.id.clone());
            }
            RoleDisposition::Spawn => {
                action.action = TeamAction::Spawn;
            }
            RoleDisposition::Deactivate => {
                action.action = TeamAction::Deactivate;
                action.target = Some(bodies[0].id.clone());
            }
            RoleDisposition::Exempt => {
                action.notes.push(
                    "unmanaged role: exempt from converge (visible, never touched)".to_string(),
                );
            }
            RoleDisposition::Duplicate => {
                action.notes.push(format!(
                    "{} live bodies carry this role — the whole batch is rejected until resolved",
                    bodies.len()
                ));
            }
            RoleDisposition::Retain => {}
        }
        // Adopt/align annotations: the prompt field and declared skills
        // have no live-body basis and are never rewritten automatically.
        if matches!(action.action, TeamAction::Adopt | TeamAction::AlignMetadata)
            && let Some(d) = &action.declared
        {
            if d.prompt.is_some() {
                action.notes.push(
                    "declaration prompt is not applied to a live body (no alignment basis; spawns only)"
                        .to_string(),
                );
            }
            if !d.skills.is_empty() {
                action.notes.push(
                    "declared skills apply at spawn; the live body's context is unchanged"
                        .to_string(),
                );
            }
        }
        plan.push(action);
    }
    plan
}

/// Converge is an operator action, or the root's own (the declaration
/// author). Any other identity is refused: converge parks and restores
/// arbitrary team members, which is not a privilege a plain agent holds.
async fn authorize_converge(
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
                    "converge is reserved for the operator or the root agent",
                ))
            }
        }
    }
}

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
    let stamped = kallip_common::timefmt::format_utc(kallip_common::timefmt::now_epoch());
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
        }));
    }

    let (results, aborted) =
        execute_converge(&state, &plan, &mut mapping, &root, req.force, &stamped).await;
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
    }))
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

/// Preflight: the whole-batch refusal pass. Structural problems (an id
/// keyed under two roles, a declared root role, duplicate live bodies,
/// invalid spawn declarations, capacity) and busy/childed deactivation
/// targets reject with per-item guidance; only busy deactivation may be
/// forced, every other rejection stands, zero actions are applied.
pub(super) fn preflight_converge(
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
                    "lock maps agent {} to both {prev:?} and {:?} — run `kallip team lock rebuild`",
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
                    "role {:?} agent {} is busy — wait for it to go idle (re-run), or pass force to interrupt it (recorded as an escape)",
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
            && let Ok(dir) = kallip_runtime::persistence::inactive_dir(id)
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
                match kallip_runtime::config::PermissionClass::from_str(raw) {
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
                if let Err(e) = kallip_runtime::tools::load_skill(skill) {
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

/// Execute the plan in row order, fail-fast: each action re-reads its
/// target first (the plan→execute window is converge's one
/// blind spot), and any violation or failure stops the batch with the
/// applied rows reported. Returns the result rows and whether the run
/// aborted early.
pub(super) async fn execute_converge(
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
pub(super) enum RestoreFallout {
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
    let mut config = AgentConfig::load(d.prompt.clone(), d.skills.clone(), Some(ws.clone()))
        .map_err(|e| e.to_string())?;
    config.role = a.role.clone();
    config.description = d.description.clone();
    config.delegation_mode = DELEGATION_CARVE_OUT
        .parse::<kallip_runtime::config::DelegationMode>()
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
pub(super) async fn restore_action(state: &SharedState, a: &PlannedAction) -> RestoreFallout {
    let Some(id) = a.target.clone() else {
        return RestoreFallout::Failed("restore row without a lock id".to_string());
    };
    // Blind-spot re-read: the id must still be absent and the role free.
    {
        let registry = state.registry.read().await;
        if registry.get(&id).is_some() {
            return RestoreFallout::Failed(format!(
                "agent {id} came alive in the plan→execute window; batch stopped, nothing applied for this row"
            ));
        }
        if let Some((holder, _)) = registry
            .iter()
            .find(|(_, e)| e.identity().config.role == a.role)
        {
            return RestoreFallout::Failed(format!(
                "role {:?} is now held by agent {holder}; batch stopped",
                a.role
            ));
        }
    }
    let inactive_dir = match kallip_runtime::persistence::inactive_dir(&id) {
        Ok(dir) => dir,
        Err(e) => {
            return degraded(
                state,
                a,
                &id,
                vec![format!("inactive body unreadable: {e:#}")],
            )
            .await;
        }
    };
    let mut meta = match kallip_runtime::persistence::read_meta_from_dir(&inactive_dir) {
        Ok(meta) => meta,
        Err(e) => {
            return degraded(
                state,
                a,
                &id,
                vec![format!(
                    "IDENTITY LOSS: the inactive body's metadata is unreadable ({e:#}); spawned a fresh replacement"
                )],
            )
            .await;
        }
    };
    if let Err(e) = kallip_runtime::persistence::reactivate_agent_dir(&id) {
        return RestoreFallout::Failed(format!(
            "could not move the inactive body back to the live area: {e:#}"
        ));
    }
    let dir = match kallip_runtime::persistence::agent_dir(&id) {
        Ok(dir) => dir,
        Err(e) => return RestoreFallout::Failed(format!("live dir unresolved: {e:#}")),
    };
    // Same-round alignment on the parked body, before the config rebuild:
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
            match kallip_runtime::config::PermissionClass::from_str(want) {
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
            && let Err(e) = kallip_runtime::persistence::rewrite_meta(
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
            if let Err(park) = kallip_runtime::persistence::deactivate_agent_dir(&id) {
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
    // Run the boot restore path over the reactivated body.
    match crate::lifecycle::restore_inactive(
        state.shutdown.clone(),
        state.clone(),
        kallip_runtime::persistence::PendingRestore {
            agent_id: id.clone(),
            agent_dir: dir.clone(),
            meta,
        },
    )
    .await
    {
        Ok((rid, entry)) => {
            // Register under the write lock with the same re-checks; a
            // raced violation is undone (task torn down, body re-parked)
            // so the abort leaves no ghost.
            let mut registry = state.registry.write().await;
            let violated = registry.get(&id).is_some()
                || registry
                    .iter()
                    .any(|(_, e)| e.identity().config.role == a.role);
            if violated {
                drop(registry);
                crate::routes::agent::teardown_agent(
                    state,
                    &rid,
                    crate::state::RegistryEntry::Live(entry),
                )
                .await;
                let parked = kallip_runtime::persistence::deactivate_agent_dir(&id);
                let mut detail = "registry changed in the plan→execute window (role or id taken); batch stopped, the body was re-parked".to_string();
                if parked.is_err() {
                    detail.push_str(" (re-park failed; manual cleanup needed)");
                }
                return RestoreFallout::Failed(detail);
            }
            registry.register(rid, crate::state::RegistryEntry::Live(entry));
            drop(registry);
            state.invalidate();
            RestoreFallout::Restored { id, notes }
        }
        Err(e) => {
            // The body cannot come up — park it back (best effort) and
            // spawn a fresh replacement. A failed re-park is surfaced in
            // the notes, never swallowed.
            let mut loss = vec![format!(
                "IDENTITY LOSS: restore failed ({e:#}); spawned a fresh replacement"
            )];
            if let Err(park) = kallip_runtime::persistence::deactivate_agent_dir(&id) {
                loss.push(format!(
                    "old body re-park failed; manual cleanup needed ({park:#})"
                ));
            }
            degraded(state, a, &id, loss).await
        }
    }
}

/// The damaged-restore landing: a fresh spawn replaces the unusable
/// body; the lock record rebinds; the old body is left exactly as is.
pub(super) async fn degraded(
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
        kallip_runtime::persistence::rewrite_meta(
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
                            kallip_runtime::ProfileReset {
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
    let mut notes = Vec::new();
    let entry = {
        let mut registry = state.registry.write().await;
        let Some(entry) = registry.get(&id) else {
            return Err(format!(
                "agent {id} vanished in the plan→execute window; batch stopped"
            ));
        };
        if let Some(live) = entry.as_live() {
            let busy = !matches!(
                live.agent.get_state(),
                crate::state::AgentState::Idle | crate::state::AgentState::Parked
            );
            if busy && !force {
                return Err(format!(
                    "agent {id} turned busy in the plan→execute window; batch stopped (re-run, or pass force)"
                ));
            }
            if busy {
                notes.push(
                    "force escape: the busy target was interrupted before teardown".to_string(),
                );
            }
        }
        if !entry.subagent_ids().is_empty() {
            return Err(format!(
                "agent {id} gained live subagents in the plan→execute window; batch stopped"
            ));
        }
        registry
            .unregister(&id)
            .ok_or_else(|| format!("agent {id} vanished during unregister"))?
    };
    if force {
        // Interrupt before teardown so an in-flight round aborts cleanly
        // instead of racing the cancel.
        let _ = crate::routes::agent::interrupt_core(state, &id).await;
    }
    crate::routes::agent::teardown_agent(state, &id, entry).await;
    kallip_runtime::persistence::deactivate_agent_dir(&id)
        .map_err(|e| format!("agent {id} is unregistered but the park failed: {e:#}"))?;
    state.invalidate();
    Ok(notes)
}
