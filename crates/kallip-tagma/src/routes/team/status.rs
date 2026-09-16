use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use axum::Json;
use axum::extract::{Query, State};
use kallip_common::AgentId;
use kallip_common::declaration::{RoleDeclaration, TeamDeclaration, parse_declaration};
use kallip_common::protocol::{
    ApiError, RoleDisposition, TeamRoleStatus, TeamStatusQuery, TeamStatusResponse,
};
use tracing::warn;

use crate::state::SharedState;

/// A lock record as the CLI presents it: role-to-id, plus whether the id
/// is parked in the inactive area. The tagma probes that itself — the
/// CLI only vouches for the lock file's content.
pub(super) struct LockPair {
    pub(super) role: String,
    pub(super) id: AgentId,
    pub(super) in_inactive: bool,
    /// The id also sits in the archived area: a permanently retired
    /// body converge must never resurrect.
    pub(super) in_archived: bool,
}

/// A live registry entry reduced to the comparison key.
#[derive(Clone)]
pub(super) struct LiveEntry {
    pub(super) id: AgentId,
    pub(super) role: String,
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
pub(super) async fn team_status(
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
pub(super) fn parse_lock_pairs(raw: &str) -> Result<Vec<(String, AgentId)>, ApiError> {
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
pub(super) fn compare_team(
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
pub(super) fn probe_lock_pairs(pairs: Vec<(String, AgentId)>) -> Vec<LockPair> {
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
