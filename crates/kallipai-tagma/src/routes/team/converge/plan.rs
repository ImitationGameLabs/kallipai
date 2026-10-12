//! Converge planning: the registry snapshot, the three-way diff and the plan rows.

use std::collections::HashMap;
use std::str::FromStr;

use kallipai_common::AgentId;
use kallipai_common::declaration::{RoleDeclaration, TeamDeclaration};
use kallipai_common::protocol::{ApiError, RoleDisposition, TeamAction, TeamPlanRow};

use crate::routes::team::status::{LiveEntry, LockPair, compare_team};
use crate::state::SharedState;

/// Registry snapshot of one live body, reduced to what converge's
/// planning and preflight read: identity fields plus the safety bits
/// (busy, children, faulted). Taken under a single read lock so the
/// whole plan is built from one consistent registry view.
pub(crate) struct LiveBody {
    pub(crate) id: AgentId,
    pub(crate) role: String,
    pub(crate) workspace_root: std::path::PathBuf,
    pub(crate) description: String,
    pub(crate) profile_set: Option<String>,
    pub(crate) permissions_class: kallipai_adk::config::PermissionClass,
    pub(crate) busy: bool,
    pub(crate) children: usize,
    pub(crate) faulted: bool,
}

/// Metadata alignment items a plan row would apply, resolved against
/// the declaration at plan time so execution is mechanical.
#[derive(Default)]
pub(crate) struct AlignItems {
    pub(crate) description: Option<String>,
    pub(crate) profile_set: Option<String>,
    /// Downgrades only: converge never raises a class (an upgrade is
    /// an explicit operator action outside converge's scope).
    pub(crate) permissions_class: Option<kallipai_adk::config::PermissionClass>,
}

impl AlignItems {
    pub(super) fn is_empty(&self) -> bool {
        self.description.is_none() && self.profile_set.is_none() && self.permissions_class.is_none()
    }
}

/// One planned action — the plan's internal form. Rows render into
/// [`TeamPlanRow`] for the wire; execution
/// consumes this richer form directly.
pub(crate) struct PlannedAction {
    pub(crate) role: String,
    pub(crate) action: TeamAction,
    /// Live body (adopt/align/deactivate) or lock-recorded id
    /// (restore). `None` for planned spawns.
    pub(crate) target: Option<AgentId>,
    pub(crate) notes: Vec<String>,
    /// The presence-level verdict that produced this row; preflight and
    /// the wire row both read it instead of parsing notes text.
    pub(crate) disposition: RoleDisposition,
    pub(crate) declared: Option<RoleDeclaration>,
    pub(crate) align: AlignItems,
    /// True when the row's lock record was discarded (drift, archived,
    /// or nowhere on disk): the record must not survive into the
    /// response's lock mapping.
    pub(crate) discard_record: bool,
}

impl PlannedAction {
    pub(super) fn to_wire(&self) -> TeamPlanRow {
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
pub(super) async fn snapshot_live(state: &SharedState) -> (Option<LiveBody>, Vec<LiveBody>) {
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
        match kallipai_adk::config::PermissionClass::from_str(want) {
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
pub(crate) fn plan_converge(
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
pub(super) async fn authorize_converge(
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
