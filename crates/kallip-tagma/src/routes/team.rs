//! The team domain: declarative team management.
//!
//! Read-only diagnostic face: a three-way comparison of the declaration
//! (read from the request-named path), the lock mapping the CLI folds
//! into the request, and the live registry. It reports what converge
//! WOULD decide, never what it did.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;

use axum::Json;
use axum::extract::{Query, State};
use kallip_common::AgentId;
use kallip_common::declaration::{RoleDeclaration, TeamDeclaration, parse_declaration};
use kallip_common::protocol::{
    ApiError, DELEGATION_CARVE_OUT, RoleDisposition, TeamAction, TeamActionResult,
    TeamConvergeOutcome, TeamConvergeRequest, TeamConvergeResponse, TeamLockEntry, TeamPlanRow,
    TeamRejection, TeamRejectionKind, TeamRoleStatus, TeamRowOutcome, TeamStatusQuery,
    TeamStatusResponse,
};
use kallip_runtime::config::AgentConfig;
use tracing::warn;

use crate::state::SharedState;

/// The team-domain router: mounted at /team by the root router.
pub(crate) fn router() -> axum::Router<SharedState> {
    axum::Router::new()
        .route("/status", axum::routing::get(team_status))
        .route("/converge", axum::routing::post(team_converge))
}

/// A lock record as the CLI presents it: role-to-id, plus whether the id
/// is parked in the inactive area. The tagma probes that itself — the
/// CLI only vouches for the lock file's content.
struct LockPair {
    role: String,
    id: AgentId,
    in_inactive: bool,
    /// The id also sits in the archived area: a permanently retired
    /// body converge must never resurrect.
    in_archived: bool,
}

/// A live registry entry reduced to the comparison key.
#[derive(Clone)]
struct LiveEntry {
    id: AgentId,
    role: String,
}

/// `GET /team/status?file=<path>&lock=role:id,role:id` — the three-way
/// comparison behind `kallip team status`.
///
/// The declaration is read from the request-named path (tagma and CLI
/// share the machine and the parser). The lock arrives as content: the
/// tagma never holds or locates the lock file itself (the lock is a
/// CLI-side archive). The live set is the registry minus the root: the
/// root is boot-built and outside the declaration's reach (converge
/// skips it), so the status table mirrors converge's input set. Any
/// authenticated identity may read it — it leaks nothing the agents
/// list does not.
pub(crate) async fn team_status(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Query(query): Query<TeamStatusQuery>,
) -> Result<Json<TeamStatusResponse>, ApiError> {
    if query.file.is_empty() {
        return Err(ApiError::bad_request(
            "'file' must name the declaration file",
        ));
    }
    // The path is caller-named and read on the tagma's machine — sound
    // under the same single-machine trust model as every other route
    // (localhost bind, operator bearer). The response only distinguishes
    // readable/unreadable and valid/invalid: no io detail, no parser
    // chain, so the read surfaces nothing beyond that bit.
    let raw = std::fs::read_to_string(&query.file)
        .map_err(|_| ApiError::bad_request("cannot read the declaration file"))?;
    let declaration =
        parse_declaration(&raw).map_err(|_| ApiError::bad_request("invalid team declaration"))?;

    let pairs = match &query.lock {
        Some(raw) => parse_lock_pairs(raw)?,
        None => Vec::new(),
    };
    // One probe per lock id: is the recorded body parked in the inactive
    // area? That bit is what separates a restore from a fresh spawn. A
    // failed probe is logged and reads as "not parked" — the comparison
    // stays available, and the log line keeps the failure diagnosable.
    let lock = probe_lock_pairs(pairs);

    let registry = state.registry.read().await;
    let root_id = registry.root_agent().map(|(id, _)| id.clone());
    // The comparison set excludes the root (converge does the same); the
    // drift pool keeps it: a lock record pointing at the root's id must
    // read as drifted, not silently pass the check.
    let all_live: Vec<LiveEntry> = registry
        .iter()
        .map(|(id, entry)| LiveEntry {
            id: id.clone(),
            role: entry.identity().config.role.clone(),
        })
        .collect();
    let live: Vec<LiveEntry> = all_live
        .iter()
        .filter(|entry| root_id.as_ref().is_none_or(|root| root != &entry.id))
        .cloned()
        .collect();
    drop(registry);

    let roles = compare_team(&declaration, &lock, &live, &all_live);
    Ok(Json(TeamStatusResponse {
        declaration_path: query.file,
        roles,
    }))
}

/// Parse the `lock` query parameter: comma-separated `role:id` pairs.
///
/// The role may contain colons (ids are UUID spellings and cannot), so
/// each pair splits at the LAST colon. Refusals are 400s — missing
/// colons, blank or whitespace-padded roles (role names key everything:
/// a padded spelling would silently fork from its clean one), repeated
/// roles (the lock holds one record per role; last-wins would hide a
/// corrupt file), and non-UUID ids. The CLI renders the lock it just
/// read, so a refused pair is a client bug, not a comparison input.
fn parse_lock_pairs(raw: &str) -> Result<Vec<(String, AgentId)>, ApiError> {
    let mut pairs: Vec<(String, AgentId)> = Vec::new();
    for piece in raw.split(',') {
        let (role, id) = piece.rsplit_once(':').ok_or_else(|| {
            ApiError::bad_request(format!("malformed lock pair {piece:?}: expected role:id"))
        })?;
        if role.trim().is_empty() {
            return Err(ApiError::bad_request(format!(
                "malformed lock pair {piece:?}: empty role"
            )));
        }
        if role.trim() != role {
            return Err(ApiError::bad_request(format!(
                "malformed lock pair {piece:?}: role has leading or trailing whitespace"
            )));
        }
        if pairs.iter().any(|(seen, _)| seen == role) {
            return Err(ApiError::bad_request(format!(
                "malformed lock parameter: duplicate pair for role {role:?}"
            )));
        }
        if !kallip_common::agentid::is_uuid_format(id) {
            return Err(ApiError::bad_request(format!(
                "malformed lock pair {role:?}: id is not a UUID"
            )));
        }
        pairs.push((role.to_string(), AgentId::from(id.to_string())));
    }
    Ok(pairs)
}

/// The three-way diff: declaration vs lock vs live registry.
///
/// One row per role in the union of the three sets — declaration order
/// first (the operator's own ordering), then undeclared roles by name.
/// Presence-level: rows compare existence, not field values; field
/// alignment is converge's plan-segment business. `all_live` (root
/// included) feeds only the drift check, so a lock record pointing at
/// the root's id reads as drifted instead of passing silently.
fn compare_team(
    declaration: &TeamDeclaration,
    lock: &[LockPair],
    live: &[LiveEntry],
    all_live: &[LiveEntry],
) -> Vec<TeamRoleStatus> {
    let lock_by_role: HashMap<&str, &LockPair> =
        lock.iter().map(|pair| (pair.role.as_str(), pair)).collect();
    let mut live_by_role: BTreeMap<&str, Vec<AgentId>> = BTreeMap::new();
    for entry in live {
        live_by_role
            .entry(entry.role.as_str())
            .or_default()
            .push(entry.id.clone());
    }

    let mut rows = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for role in &declaration.roles {
        seen.insert(role.name.as_str());
        rows.push(build_row(
            &role.name,
            Some(role),
            lock_by_role.get(role.name.as_str()).copied(),
            live_by_role.get(role.name.as_str()).map(Vec::as_slice),
            all_live,
        ));
    }
    let undeclared: BTreeSet<&str> = lock_by_role
        .keys()
        .copied()
        .chain(live_by_role.keys().copied())
        .filter(|name| !seen.contains(name))
        .collect();
    for name in undeclared {
        rows.push(build_row(
            name,
            None,
            lock_by_role.get(name).copied(),
            live_by_role.get(name).map(Vec::as_slice),
            all_live,
        ));
    }
    rows
}

/// Resolve one role's row: gather the three views, then apply the
/// disposition rules. Order matters: unmanaged exempts first (a
/// hands-off role is never a duplicate finding), then duplicates (the
/// whole-batch rejection), then the declared/undeclared splits.
fn build_row(
    role: &str,
    declaration: Option<&RoleDeclaration>,
    lock: Option<&LockPair>,
    live: Option<&[AgentId]>,
    all_live: &[LiveEntry],
) -> TeamRoleStatus {
    let live = live.unwrap_or_default();
    let lock_drift = lock.is_some_and(|pair| {
        all_live
            .iter()
            .any(|entry| entry.id == pair.id && entry.role != role)
    });
    let disposition = if declaration.is_some_and(|declared| declared.unmanaged) {
        if live.is_empty() {
            RoleDisposition::Retain
        } else {
            RoleDisposition::Exempt
        }
    } else if live.len() > 1 {
        RoleDisposition::Duplicate
    } else if declaration.is_some() {
        match (live.is_empty(), lock) {
            (false, Some(_)) => RoleDisposition::Active,
            (false, None) => RoleDisposition::Adopt,
            (true, Some(pair)) if pair.in_inactive => RoleDisposition::Restore,
            (true, _) => RoleDisposition::Spawn,
        }
    } else if !live.is_empty() {
        RoleDisposition::Deactivate
    } else {
        RoleDisposition::Retain
    };
    TeamRoleStatus {
        role: role.to_string(),
        declaration: declaration.cloned(),
        lock_id: lock.map(|pair| pair.id.clone()),
        lock_drift,
        lock_inactive: lock.is_some_and(|pair| pair.in_inactive),
        root_conflict: declaration.is_some() && role == "root",
        live: live.to_vec(),
        disposition,
    }
}

/// Probe each lock pair's id against the disk areas: parked in the
/// inactive area (restorable), retired in the archived area (never
/// reusable), or neither. A failed probe logs and reads as "not parked"
/// so the comparison stays available; the log line keeps the failure
/// diagnosable. Shared by the status face and converge — one probe
/// semantics, never two.
fn probe_lock_pairs(pairs: Vec<(String, AgentId)>) -> Vec<LockPair> {
    pairs
        .into_iter()
        .map(|(role, id)| {
            let in_inactive = match kallip_runtime::persistence::inactive_dir(&id) {
                Ok(dir) => dir.is_dir(),
                Err(err) => {
                    warn!(agent = %id, error = %err, "inactive-area probe failed");
                    false
                }
            };
            let in_archived = match kallip_runtime::persistence::archived_dir(&id) {
                Ok(dir) => dir.is_dir(),
                Err(err) => {
                    warn!(agent = %id, error = %err, "archived-area probe failed");
                    false
                }
            };
            LockPair {
                in_inactive,
                in_archived,
                role,
                id,
            }
        })
        .collect()
}

// --- converge: the declarative write face -------------------------------

/// Registry snapshot of one live body, reduced to what converge's
/// planning and preflight read: identity fields plus the safety bits
/// (busy, children, faulted). Taken under a single read lock so the
/// whole plan is built from one consistent registry view.
struct LiveBody {
    id: AgentId,
    role: String,
    workspace_root: std::path::PathBuf,
    description: String,
    profile_set: Option<String>,
    permissions_class: kallip_runtime::config::PermissionClass,
    busy: bool,
    children: usize,
    faulted: bool,
}

/// Metadata alignment items a plan row would apply, resolved against
/// the declaration at plan time so execution is mechanical.
#[derive(Default)]
struct AlignItems {
    description: Option<String>,
    profile_set: Option<String>,
    /// Downgrades only: converge never raises a class (an upgrade is
    /// an explicit operator action outside converge's scope).
    permissions_class: Option<kallip_runtime::config::PermissionClass>,
}

impl AlignItems {
    fn is_empty(&self) -> bool {
        self.description.is_none() && self.profile_set.is_none() && self.permissions_class.is_none()
    }
}

/// One planned action — the plan's internal form. Rows render into
/// [`TeamPlanRow`] for the wire; execution
/// consumes this richer form directly.
struct PlannedAction {
    role: String,
    action: TeamAction,
    /// Live body (adopt/align/deactivate) or lock-recorded id
    /// (restore). `None` for planned spawns.
    target: Option<AgentId>,
    notes: Vec<String>,
    /// The presence-level verdict that produced this row; preflight and
    /// the wire row both read it instead of parsing notes text.
    disposition: RoleDisposition,
    declared: Option<RoleDeclaration>,
    align: AlignItems,
    /// True when the row's lock record was discarded (drift, archived,
    /// or nowhere on disk): the record must not survive into the
    /// response's lock mapping.
    discard_record: bool,
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
fn plan_converge(
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
pub(crate) async fn team_converge(
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
fn project_mapping(
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
fn preflight_converge(
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
async fn execute_converge(
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
enum RestoreFallout {
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
async fn restore_action(state: &SharedState, a: &PlannedAction) -> RestoreFallout {
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

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_testkit::DevDir;

    fn dev_tempdir(label: &str) -> DevDir {
        DevDir::new(label)
    }

    fn live(role: &str) -> LiveEntry {
        LiveEntry {
            id: AgentId::random(),
            role: role.to_string(),
        }
    }

    fn lock_pair(role: &str, id: &AgentId, in_inactive: bool) -> LockPair {
        LockPair {
            role: role.to_string(),
            id: id.clone(),
            in_inactive,
            in_archived: false,
        }
    }

    fn declaration(toml_text: &str) -> TeamDeclaration {
        parse_declaration(toml_text).unwrap()
    }

    fn disposition_of(rows: &[TeamRoleStatus], role: &str) -> RoleDisposition {
        rows.iter()
            .find(|row| row.role == role)
            .unwrap_or_else(|| panic!("no row for role {role}"))
            .disposition
    }

    #[test]
    fn an_in_sync_role_is_active() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let body = live("dev");
        let lock = vec![lock_pair("dev", &body.id, false)];
        let rows = compare_team(
            &declaration,
            &lock,
            std::slice::from_ref(&body),
            std::slice::from_ref(&body),
        );
        assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Active);
        assert!(!rows[0].lock_drift);
        assert_eq!(rows[0].live, vec![body.id.clone()]);
        assert_eq!(rows[0].lock_id.as_ref(), Some(&body.id));
    }

    #[test]
    fn declared_live_without_a_lock_is_adopted() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let body = live("dev");
        let rows = compare_team(
            &declaration,
            &[],
            std::slice::from_ref(&body),
            std::slice::from_ref(&body),
        );
        assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Adopt);
    }

    #[test]
    fn an_absent_role_restores_when_the_lock_id_is_inactive() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let parked = AgentId::random();
        let lock = vec![lock_pair("dev", &parked, true)];
        let rows = compare_team(&declaration, &lock, &[], &[]);
        assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Restore);
        assert!(rows[0].lock_inactive);
    }

    #[test]
    fn an_absent_role_spawns_when_there_is_nothing_to_restore() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let gone = AgentId::random();
        let rows = compare_team(&declaration, &[lock_pair("dev", &gone, false)], &[], &[]);
        assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Spawn);
        // No lock at all spawns too.
        let rows = compare_team(&declaration, &[], &[], &[]);
        assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Spawn);
    }

    #[test]
    fn an_undeclared_live_role_deactivates() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let stray = live("stray");
        let rows = compare_team(
            &declaration,
            &[],
            std::slice::from_ref(&stray),
            std::slice::from_ref(&stray),
        );
        assert_eq!(disposition_of(&rows, "stray"), RoleDisposition::Deactivate);
    }

    #[test]
    fn an_unmanaged_role_is_exempt_while_live_and_retained_when_absent() {
        let declaration = declaration("[[role]]\nname = \"guest1\"\nunmanaged = true\n");
        let body = live("guest1");
        let rows = compare_team(
            &declaration,
            &[],
            std::slice::from_ref(&body),
            std::slice::from_ref(&body),
        );
        assert_eq!(disposition_of(&rows, "guest1"), RoleDisposition::Exempt);
        let rows = compare_team(&declaration, &[], &[], &[]);
        assert_eq!(disposition_of(&rows, "guest1"), RoleDisposition::Retain);
    }

    #[test]
    fn duplicate_live_bodies_are_flagged_for_manual_resolution() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let bodies = vec![live("dev"), live("dev")];
        let rows = compare_team(&declaration, &[], &bodies, &bodies);
        assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Duplicate);
        assert_eq!(rows[0].live.len(), 2);
    }

    #[test]
    fn a_lock_only_role_is_retained() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let parked = AgentId::random();
        let lock = vec![lock_pair("scout", &parked, true)];
        let rows = compare_team(&declaration, &lock, &[], &[]);
        assert_eq!(disposition_of(&rows, "scout"), RoleDisposition::Retain);
        // Row order: declaration roles first, undeclared after, by name.
        let names: Vec<&str> = rows.iter().map(|row| row.role.as_str()).collect();
        assert_eq!(names, vec!["dev", "scout"]);
    }

    #[test]
    fn a_lock_id_live_under_another_role_is_drift() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let body = live("scout");
        let lock = vec![lock_pair("dev", &body.id, false)];
        let rows = compare_team(
            &declaration,
            &lock,
            std::slice::from_ref(&body),
            std::slice::from_ref(&body),
        );
        let row = rows.iter().find(|row| row.role == "dev").unwrap();
        assert!(row.lock_drift);
        // The role has no live body of its own and the lock id is not
        // parked: spawn, with the drift flagged.
        assert_eq!(row.disposition, RoleDisposition::Spawn);
    }

    #[test]
    fn rows_follow_declaration_order_then_undeclared_names() {
        let declaration = declaration("[[role]]\nname = \"zeta\"\n\n[[role]]\nname = \"alpha\"\n");
        let bodies = vec![live("mid")];
        let rows = compare_team(&declaration, &[], &bodies, &bodies);
        let names: Vec<&str> = rows.iter().map(|row| row.role.as_str()).collect();
        assert_eq!(names, vec!["zeta", "alpha", "mid"]);
    }

    #[test]
    fn an_unmanaged_role_stays_exempt_even_with_duplicate_bodies() {
        let declaration = declaration("[[role]]\nname = \"guest1\"\nunmanaged = true\n");
        let bodies = vec![live("guest1"), live("guest1")];
        let rows = compare_team(&declaration, &[], &bodies, &bodies);
        assert_eq!(disposition_of(&rows, "guest1"), RoleDisposition::Exempt);
    }

    #[test]
    fn a_lock_record_pointing_at_the_root_drifts() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let root = live("root");
        let lock = vec![lock_pair("dev", &root.id, false)];
        // The root sits in the drift pool (all_live) but not in the
        // comparison set (live).
        let rows = compare_team(&declaration, &lock, &[], std::slice::from_ref(&root));
        let row = rows.iter().find(|row| row.role == "dev").unwrap();
        assert!(row.lock_drift);
        assert!(row.live.is_empty());
    }

    #[test]
    fn a_declared_root_role_is_flagged_as_a_dead_end() {
        let declaration = declaration("[[role]]\nname = \"root\"\n");
        let rows = compare_team(&declaration, &[], &[], &[]);
        let row = rows.iter().find(|row| row.role == "root").unwrap();
        assert!(row.root_conflict);
        // Presence level still says spawn; the flag is what shows the
        // declaration cannot converge that way.
        assert_eq!(row.disposition, RoleDisposition::Spawn);
    }

    #[test]
    fn lock_pairs_parse_loose_roles_and_strict_ids() {
        let pairs = parse_lock_pairs("dev:11111111-1111-4111-8111-111111111111").unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "dev");
        // The role may carry colons; the split anchors on the last.
        let pairs = parse_lock_pairs(
            "kallip:dev:11111111-1111-4111-8111-111111111111,scout:22222222-2222-4222-8222-222222222222",
        )
        .unwrap();
        assert_eq!(pairs[0].0, "kallip:dev");
        assert_eq!(pairs[1].0, "scout");
        assert!(parse_lock_pairs("dev-no-id").is_err());
        assert!(parse_lock_pairs(":11111111-1111-4111-8111-111111111111").is_err());
        assert!(parse_lock_pairs("dev:not-a-uuid").is_err());
    }

    #[test]
    fn a_whitespace_padded_lock_role_is_refused() {
        let id = "11111111-1111-4111-8111-111111111111";
        assert!(parse_lock_pairs(&format!(" dev:{id}")).is_err());
        assert!(parse_lock_pairs(&format!("dev :{id}")).is_err());
        assert!(parse_lock_pairs(&format!("   :{id}")).is_err());
    }

    #[test]
    fn a_repeated_lock_role_is_refused_rather_than_last_won() {
        let err = parse_lock_pairs(
            "dev:11111111-1111-4111-8111-111111111111,dev:22222222-2222-4222-8222-222222222222",
        )
        .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("duplicate"));
    }

    // -- converge planning (plan_converge, project_mapping) --

    use crate::test_helpers::{make_profile_bundle, make_state};

    /// A live body with plausible defaults: idle, childless, healthy,
    /// Normal class, no profile. Tests mutate specific fields.
    fn body(role: &str) -> LiveBody {
        LiveBody {
            id: AgentId::random(),
            role: role.to_string(),
            workspace_root: std::path::PathBuf::from("/tmp/kallip-team-tests/unused"),
            description: String::new(),
            profile_set: None,
            permissions_class: kallip_runtime::config::PermissionClass::default(),
            busy: false,
            children: 0,
            faulted: false,
        }
    }

    fn converge_plan(
        declaration: &TeamDeclaration,
        lock: &[LockPair],
        bodies: &[LiveBody],
    ) -> Vec<PlannedAction> {
        plan_converge(declaration, lock, None, bodies)
    }
    fn converge_plan_with_root(
        declaration: &TeamDeclaration,
        lock: &[LockPair],
        root: &LiveBody,
        bodies: &[LiveBody],
    ) -> Vec<PlannedAction> {
        plan_converge(declaration, lock, Some(root), bodies)
    }

    fn row_of<'a>(plan: &'a [PlannedAction], role: &str) -> &'a PlannedAction {
        plan.iter()
            .find(|a| a.role == role)
            .unwrap_or_else(|| panic!("no plan row for role {role}"))
    }

    fn action_of(plan: &[PlannedAction], role: &str) -> TeamAction {
        row_of(plan, role).action
    }

    fn target_of(plan: &[PlannedAction], role: &str) -> AgentId {
        row_of(plan, role)
            .target
            .clone()
            .unwrap_or_else(|| panic!("role {role} has no target"))
    }

    #[test]
    fn converge_plan_retains_an_in_sync_role() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let b = body("dev");
        let lock = vec![lock_pair("dev", &b.id, false)];
        let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&b));
        assert_eq!(action_of(&plan, "dev"), TeamAction::Retain);
        assert!(row_of(&plan, "dev").target.is_none());
    }

    #[test]
    fn converge_plan_aligns_metadata_a_live_body_drifted_from() {
        let declaration = declaration(
            "[[role]]\nname = \"dev\"\ndescription = \"builds the thing\"\npermission_class = \"guest\"\n",
        );
        let b = body("dev");
        let lock = vec![lock_pair("dev", &b.id, false)];
        let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&b));
        let row = row_of(&plan, "dev");
        assert_eq!(row.action, TeamAction::AlignMetadata);
        assert_eq!(row.align.description.as_deref(), Some("builds the thing"));
        assert_eq!(
            row.align.permissions_class,
            Some(kallip_runtime::config::PermissionClass::Guest)
        );
    }

    #[test]
    fn converge_plan_never_raises_a_permission_class() {
        let declaration = declaration("[[role]]\nname = \"dev\"\npermission_class = \"normal\"\n");
        let mut b = body("dev");
        b.permissions_class = kallip_runtime::config::PermissionClass::Guest;
        let lock = vec![lock_pair("dev", &b.id, false)];
        let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&b));
        let row = row_of(&plan, "dev");
        assert!(row.align.permissions_class.is_none());
        assert!(row.notes.iter().any(|n| n.contains("never raise")));
    }

    #[test]
    fn converge_plan_adopts_a_live_body_the_lock_does_not_know() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let b = body("dev");
        let plan = converge_plan(&declaration, &[], std::slice::from_ref(&b));
        assert_eq!(action_of(&plan, "dev"), TeamAction::Adopt);
        assert_eq!(target_of(&plan, "dev"), b.id);
    }

    #[test]
    fn converge_plan_supersedes_a_lock_pointing_at_a_parked_body() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let parked = AgentId::random();
        let b = body("dev");
        let lock = vec![lock_pair("dev", &parked, true)];
        let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&b));
        assert_eq!(action_of(&plan, "dev"), TeamAction::Adopt);
        assert_eq!(target_of(&plan, "dev"), b.id);
        let row = row_of(&plan, "dev");
        assert!(row.notes.iter().any(|n| n.contains("superseded")));
        // Supersede is not a discard: the record was never wrong about
        // being on disk, project_mapping replaces it with the live body.
        assert!(!row.discard_record);
    }

    #[test]
    fn converge_plan_restores_a_parked_body_and_spawns_an_empty_role() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n\n[[role]]\nname = \"scout\"\n");
        let parked = AgentId::random();
        let lock = vec![lock_pair("dev", &parked, true)];
        let plan = converge_plan(&declaration, &lock, &[]);
        assert_eq!(action_of(&plan, "dev"), TeamAction::Restore);
        assert_eq!(target_of(&plan, "dev"), parked);
        assert_eq!(action_of(&plan, "scout"), TeamAction::Spawn);
    }

    #[test]
    fn converge_plan_deactivates_a_stray_live_role() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let stray = body("stray");
        let plan = converge_plan(&declaration, &[], std::slice::from_ref(&stray));
        assert_eq!(action_of(&plan, "stray"), TeamAction::Deactivate);
        assert_eq!(target_of(&plan, "stray"), stray.id);
    }

    #[test]
    fn converge_plan_discards_archived_and_nowhere_lock_records() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n\n[[role]]\nname = \"scout\"\n");
        let retired = AgentId::random();
        let gone = AgentId::random();
        let lock = vec![
            LockPair {
                role: "dev".to_string(),
                id: retired,
                in_inactive: false,
                in_archived: true,
            },
            lock_pair("scout", &gone, false),
        ];
        let plan = converge_plan(&declaration, &lock, &[]);
        // Archived bodies are never resurrected: spawn a fresh one and
        // drop the record from the mapping.
        let dev = row_of(&plan, "dev");
        assert_eq!(dev.action, TeamAction::Spawn);
        assert!(dev.discard_record);
        assert!(dev.notes.iter().any(|n| n.contains("archived")));
        // A record whose id is nowhere on disk is discarded too.
        let scout = row_of(&plan, "scout");
        assert!(scout.discard_record);
        assert!(scout.notes.iter().any(|n| n.contains("not on disk")));
    }

    // -- converge mapping projection + preflight refusals --

    fn planned(role: &str, action: TeamAction, target: Option<AgentId>) -> PlannedAction {
        PlannedAction {
            role: role.to_string(),
            action,
            disposition: RoleDisposition::Retain,
            target,
            notes: Vec::new(),
            declared: None,
            align: AlignItems::default(),
            discard_record: false,
        }
    }

    #[test]
    fn project_mapping_keeps_known_targets_and_skips_spawnless_rows() {
        let stamped = "2026-01-01T00:00:00Z";
        let parked = AgentId::random();
        let adopted = AgentId::random();
        let aligning = AgentId::random();
        let plan = vec![
            planned("dev", TeamAction::Restore, Some(parked.clone())),
            planned("scout", TeamAction::Adopt, Some(adopted.clone())),
            planned(
                "reviewer",
                TeamAction::AlignMetadata,
                Some(aligning.clone()),
            ),
            planned("guest1", TeamAction::Retain, None),
            planned("pilot", TeamAction::Spawn, None),
        ];
        let mut mapping = Vec::new();
        // Dry run: no spawned ids exist yet, so the spawn row projects
        // nothing; align/retain rows contribute no new bindings.
        project_mapping(&mut mapping, &plan, None, stamped);
        let bound: Vec<(&str, &AgentId)> =
            mapping.iter().map(|e| (e.role.as_str(), &e.id)).collect();
        assert_eq!(bound, vec![("dev", &parked), ("scout", &adopted)]);
        // Post-execution: a spawned id lands under its role.
        let spawned_id = AgentId::random();
        let mut spawned = HashMap::new();
        spawned.insert("pilot".to_string(), spawned_id.clone());
        project_mapping(&mut mapping, &plan, Some(&spawned), stamped);
        assert_eq!(mapping.len(), 3);
        assert!(
            mapping
                .iter()
                .any(|e| e.role == "pilot" && e.id == spawned_id)
        );
    }

    #[test]
    fn preflight_rejects_a_lock_id_keyed_under_two_roles() {
        let state = make_state();
        let shared = AgentId::random();
        let declaration = declaration("[[role]]\nname = \"dev\"\n\n[[role]]\nname = \"scout\"\n");
        let plan = converge_plan(&declaration, &[], &[]);
        let lock = vec![
            lock_pair("dev", &shared, true),
            lock_pair("scout", &shared, true),
        ];
        let rejections = preflight_converge(&state, &plan, &None, &lock, &[], false);
        assert!(
            rejections
                .iter()
                .any(|r| r.kind == TeamRejectionKind::LockAmbiguity
                    && r.message.contains("lock rebuild"))
        );
    }

    #[test]
    fn preflight_rejects_a_declaration_of_the_root_role() {
        let state = make_state();
        let declaration = declaration("[[role]]\nname = \"root\"\n");
        let plan = converge_plan(&declaration, &[], &[]);
        let rejections = preflight_converge(&state, &plan, &None, &[], &[], false);
        assert!(
            rejections
                .iter()
                .any(|r| r.kind == TeamRejectionKind::RootRole)
        );
    }

    #[test]
    fn preflight_rejects_duplicate_live_bodies_under_one_role() {
        let state = make_state();
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let bodies = vec![body("dev"), body("dev")];
        let plan = converge_plan(&declaration, &[], &bodies);
        let rejections = preflight_converge(&state, &plan, &None, &[], &bodies, false);
        assert!(
            rejections
                .iter()
                .any(|r| r.kind == TeamRejectionKind::Duplicate)
        );
    }

    #[test]
    fn preflight_rejects_a_busy_deactivation_target_until_forced() {
        let state = make_state();
        // An empty declaration: the stray is the only plan row, so the
        // only rejection possible is the busy/childed one under test.
        let declaration = declaration("");
        let mut stray = body("stray");
        stray.busy = true;
        let bodies = vec![stray];
        let plan = converge_plan(&declaration, &[], &bodies);
        let rejections = preflight_converge(&state, &plan, &None, &[], &bodies, false);
        assert!(rejections.iter().any(|r| r.kind == TeamRejectionKind::Busy));
        // Force is the auditable escape: the same plan passes.
        let rejections = preflight_converge(&state, &plan, &None, &[], &bodies, true);
        assert!(
            rejections.is_empty(),
            "unexpected rejections: {rejections:?}"
        );
    }

    #[test]
    fn preflight_rejects_a_childed_deactivation_target_even_when_forced() {
        let state = make_state();
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let mut stray = body("stray");
        stray.children = 2;
        let bodies = vec![stray];
        let plan = converge_plan(&declaration, &[], &bodies);
        let rejections = preflight_converge(&state, &plan, &None, &[], &bodies, true);
        assert!(
            rejections
                .iter()
                .any(|r| r.kind == TeamRejectionKind::LiveChildren)
        );
    }

    #[test]
    fn preflight_rejects_spawns_without_a_usable_profile_set() {
        let state = make_state();
        let root = body("root");
        let declaration = declaration(
            "[[role]]\nname = \"scout\"\n\n[[role]]\nname = \"pilot\"\nprofile_set = \"no-such-set\"\n",
        );
        let plan = converge_plan(&declaration, &[], &[]);
        let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
        assert!(
            rejections
                .iter()
                .any(|r| r.kind == TeamRejectionKind::SpawnProfileSet)
        );
        assert!(
            rejections
                .iter()
                .any(|r| r.message.contains("unknown profile_set"))
        );
    }

    #[test]
    fn preflight_rejects_a_spawn_class_spelling_it_does_not_know() {
        let state = make_state();
        let root = body("root");
        let declaration = declaration(
            "[[role]]\nname = \"pilot\"\nprofile_set = \"default\"\npermission_class = \"sudo\"\n",
        );
        let plan = converge_plan(&declaration, &[], &[]);
        let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
        assert!(
            rejections
                .iter()
                .any(|r| r.kind == TeamRejectionKind::SpawnProfileClass)
        );
    }

    #[test]
    fn preflight_rejects_a_spawn_class_above_the_root() {
        let state = make_state();
        let mut root = body("root");
        root.permissions_class = kallip_runtime::config::PermissionClass::Guest;
        let declaration = declaration(
            "[[role]]\nname = \"pilot\"\nprofile_set = \"default\"\npermission_class = \"normal\"\n",
        );
        let plan = converge_plan(&declaration, &[], &[]);
        let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
        assert!(
            rejections
                .iter()
                .any(|r| r.message.contains("exceeds the root"))
        );
    }

    #[test]
    fn preflight_rejects_a_converge_over_capacity() {
        use kallip_common::authtoken::TokenHash;
        use kallip_common::policy::PolicyPreset;
        let profiles = make_profile_bundle();
        let state: crate::state::SharedState =
            std::sync::Arc::new(crate::state::AppState::with_limits(
                TokenHash::of("op-token"),
                1,
                1,
                5,
                profiles,
                PolicyPreset::Default,
                kallip_runtime::token_budget::TokenBudget::unlimited(),
                None,
            ));
        let root = body("root");
        let declaration = declaration(
            "[[role]]\nname = \"a\"\nprofile_set = \"default\"\n\n[[role]]\nname = \"b\"\nprofile_set = \"default\"\n",
        );
        let plan = converge_plan(&declaration, &[], &[]);
        let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
        assert!(
            rejections
                .iter()
                .any(|r| r.kind == TeamRejectionKind::CapacityAgents)
        );
        assert!(
            rejections
                .iter()
                .any(|r| r.kind == TeamRejectionKind::CapacityChildren)
        );
    }
    #[test]
    fn preflight_allows_a_pure_deactivation_plan() {
        let state = make_state();
        let root = body("root");
        let dev = AgentId::random();
        let plan = vec![planned("dev", TeamAction::Deactivate, Some(dev))];
        // A shrink-only plan nets negative children; the signed net
        // must not underflow, and the root reads under its cap after.
        let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
        assert!(
            rejections.is_empty(),
            "unexpected rejections: {rejections:?}"
        );
    }

    #[test]
    fn converge_plan_discards_a_drifted_lock_record() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let misplaced = body("scout");
        let lock = vec![lock_pair("dev", &misplaced.id, false)];
        let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&misplaced));
        let dev = row_of(&plan, "dev");
        // Nothing live under the role and the recorded id is live
        // elsewhere: spawn fresh, the stale record is dropped.
        assert_eq!(dev.action, TeamAction::Spawn);
        assert!(dev.discard_record);
        assert!(
            dev.notes
                .iter()
                .any(|n| n.contains("live under another role"))
        );
    }

    #[test]
    fn converge_plan_drifts_a_lock_record_pointing_at_the_root() {
        let declaration = declaration("[[role]]\nname = \"dev\"\n");
        let root = body("root");
        let lock = vec![lock_pair("dev", &root.id, false)];
        let plan = converge_plan_with_root(&declaration, &lock, &root, &[]);
        let dev = row_of(&plan, "dev");
        assert_eq!(dev.action, TeamAction::Spawn);
        assert!(dev.discard_record);
        assert!(
            dev.notes
                .iter()
                .any(|n| n.contains("live under another role"))
        );
        // The drift-pool invariant: the notes never claim the row is
        // nowhere on disk.
        assert!(!dev.notes.iter().any(|n| n.contains("not on disk")));
    }

    #[tokio::test]
    async fn restore_degrades_loudly_when_the_body_meta_is_unreadable() {
        let state = crate::test_helpers::make_state();
        let id = AgentId::random();
        // A parked body whose meta cannot be read: the restore path must
        // degrade loudly (a fresh-spawn attempt surfaces), never land as
        // if nothing happened.
        let dir = kallip_runtime::persistence::inactive_dir(&id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), "not valid meta").unwrap();
        let row = planned("dev", TeamAction::Restore, Some(id));
        match restore_action(&state, &row).await {
            RestoreFallout::Failed(detail) => {
                assert!(detail.contains("restore degraded to a fresh spawn"));
                assert!(detail.contains("the spawn failed"));
            }
            RestoreFallout::Restored { .. } | RestoreFallout::Degraded { .. } => {
                panic!("restore must not land when the body meta is unreadable")
            }
        }
    }

    #[tokio::test]
    async fn window_violation_stops_the_batch_at_the_failed_deactivate() {
        let state = crate::test_helpers::make_state();
        let aligned = AgentId::random();
        let vanished = AgentId::random();
        let plan = vec![
            planned("dev", TeamAction::AlignMetadata, Some(aligned)),
            planned("scout", TeamAction::Deactivate, Some(vanished)),
        ];
        let mut mapping = Vec::new();
        let root: Option<LiveBody> = None;
        let (results, aborted) = execute_converge(
            &state,
            &plan,
            &mut mapping,
            &root,
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        assert!(aborted);
        // Deactivates run first, so the vanished deactivate is row 0: it
        // fails and the fail-fast stop leaves the align row unexecuted.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, TeamRowOutcome::Failed);
        assert!(
            results[0]
                .detail
                .contains("vanished in the plan→execute window")
        );
    }

    #[tokio::test]
    async fn window_violation_stops_the_batch_at_an_empty_align_row() {
        let state = crate::test_helpers::make_state();
        let vanished = AgentId::random();
        // An adopt row carries no align fields: the empty-align shortcut
        // must still re-read the target, or a vanished agent reports
        // Applied and the lock mapping rebinds to a dead id.
        let plan = vec![planned("dev", TeamAction::Adopt, Some(vanished))];
        let mut mapping = Vec::new();
        let root: Option<LiveBody> = None;
        let (results, aborted) = execute_converge(
            &state,
            &plan,
            &mut mapping,
            &root,
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        assert!(aborted);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, TeamRowOutcome::Failed);
        assert!(
            results[0]
                .detail
                .contains("vanished in the plan→execute window")
        );
        assert!(mapping.is_empty());
    }
    // ---- execute-layer harness: a counting spawn stub drives the
    // converge spawn/restore paths without a real runtime ----

    /// Spawn stub for the execution-layer tests: counts calls, returns
    /// a fresh entry per call, and fails on demand so both spawn
    /// outcomes are drivable.
    fn spawn_stub(
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail: bool,
    ) -> crate::lifecycle::SpawnFn {
        std::sync::Arc::new(move |_args: crate::lifecycle::SpawnArgs| {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let fresh = crate::test_helpers::make_entry_with_rx(None, "stub-token".to_string());
            let crate::state::AgentEntry {
                identity, agent, ..
            } = fresh.0;
            let _keep_rx_open = fresh.1;
            Box::pin(async move {
                if fail {
                    anyhow::bail!("stub spawn failure");
                }
                Ok((agent, identity))
            })
        })
    }

    #[tokio::test]
    async fn converge_spawn_row_spawns_binds_the_mapping_and_reports() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = crate::test_helpers::make_state_with_spawn(spawn_stub(calls.clone(), false));
        let declaration = declaration("[[role]]\nname = \"dev\"\nprofile_set = \"default\"\n");
        let plan = converge_plan(&declaration, &[], &[]);
        let root = body("root");
        // AgentConfig::load resolves the derived workspace, so the
        // directory must exist before the row executes.
        std::fs::create_dir_all(root.workspace_root.join("team").join("dev")).unwrap();
        // spawn_subagent re-reads the supervisor from the registry.
        let (root_entry, _rx) =
            crate::test_helpers::make_entry_with_rx(None, "root-token".to_string());
        state.registry.write().await.register(
            root.id.clone(),
            crate::state::RegistryEntry::Live(root_entry),
        );
        let mut mapping = Vec::new();
        let (results, aborted) = execute_converge(
            &state,
            &plan,
            &mut mapping,
            &Some(root),
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        assert!(!aborted, "results: {results:?}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, TeamRowOutcome::Applied);
        let id = results[0]
            .agent_id
            .clone()
            .expect("spawn reports the new id");
        assert_eq!(mapping.len(), 1);
        assert_eq!(mapping[0].role, "dev");
        assert_eq!(mapping[0].id, id);
    }

    #[tokio::test]
    async fn converge_spawn_failure_fails_loudly_and_stops_the_batch() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = crate::test_helpers::make_state_with_spawn(spawn_stub(calls.clone(), true));
        let declaration = declaration("[[role]]\nname = \"dev\"\nprofile_set = \"default\"\n");
        let plan = converge_plan(&declaration, &[], &[]);
        let root = body("root");
        std::fs::create_dir_all(root.workspace_root.join("team").join("dev")).unwrap();
        let (root_entry, _rx) =
            crate::test_helpers::make_entry_with_rx(None, "root-token".to_string());
        state.registry.write().await.register(
            root.id.clone(),
            crate::state::RegistryEntry::Live(root_entry),
        );
        let mut mapping = Vec::new();
        let (results, aborted) = execute_converge(
            &state,
            &plan,
            &mut mapping,
            &Some(root),
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        assert!(aborted);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, TeamRowOutcome::Failed);
        // The stub's cause is logged, not surfaced: `ApiError::internal`
        // sanitizes the row detail to the generic message.
        assert!(results[0].detail.contains("internal error"));
        assert!(!results[0].detail.contains("stub spawn failure"));
        assert!(mapping.is_empty());
    }

    #[tokio::test]
    async fn converge_restore_degrades_into_a_fresh_spawn_and_rebinds() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = crate::test_helpers::make_state_with_spawn(spawn_stub(calls.clone(), false));
        // The degraded fallback spawns under the registry's live root.
        let mut root_entry =
            crate::test_helpers::make_entry_with_rx(None, "root-token".to_string()).0;
        let root_ws_guard = dev_tempdir("conv-root");
        let root_ws = root_ws_guard.path().to_path_buf();
        root_entry.identity.config.workspace_root = root_ws.clone();
        let root_id = AgentId::random();
        state
            .registry
            .write()
            .await
            .register(root_id, crate::state::RegistryEntry::Live(root_entry));
        std::fs::create_dir_all(root_ws.join("team").join("dev")).unwrap();
        let old = AgentId::random();
        let declaration = declaration("[[role]]\nname = \"dev\"\nprofile_set = \"default\"\n");
        let lock = vec![lock_pair("dev", &old, true)];
        let plan = converge_plan(&declaration, &lock, &[]);
        // The recorded body never reached the inactive area: the restore
        // degrades into a fresh spawn instead of resurrecting it.
        let mut mapping = vec![TeamLockEntry {
            role: "dev".to_string(),
            id: old.clone(),
            converged_at: "2025-12-01T00:00:00Z".to_string(),
        }];
        let root: Option<LiveBody> = None;
        let (results, aborted) = execute_converge(
            &state,
            &plan,
            &mut mapping,
            &root,
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        assert!(!aborted, "results: {results:?}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, TeamRowOutcome::Applied);
        assert!(results[0].detail.contains("degraded into a fresh spawn"));
        assert!(
            results[0]
                .notes
                .iter()
                .any(|n| n.contains("was not reused"))
        );
        let new_id = results[0]
            .agent_id
            .clone()
            .expect("degraded spawn reports the new id");
        assert_ne!(new_id, old);
        assert_eq!(mapping[0].id, new_id);
        assert!(state.registry.read().await.get(&old).is_none());
    }
}
