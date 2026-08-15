//! Subagent permission validation: supervisor/ceiling checks and the
//! requested-vs-granted permission-class resolution.

use kallip_common::agentid::AgentId;
use kallip_common::policy::ExecPolicy;
use kallip_common::protocol::ApiError;
use kallip_runtime::config::{DelegationMode, PermissionClass, PermissionProfile};

/// Validate supervisor constraints for a subagent creation request.
///
/// Returns `(PermissionProfile, ExecPolicy, PermissionClass)` for the
/// new subagent if valid. The subagent inherits the supervisor's exec-policy
/// overrides (cloned), so monotonic strictness holds at creation. The classify
/// preset is tagma-global, so it is not part of the per-agent inheritance.
///
/// `requested_class` is the optional explicit downgrade from the spawn request
/// (already parsed from the wire string by the caller). When `None`, the child
/// is granted its model tier's ceiling (`ceiling_for_tier`); otherwise the
/// requested class is treated as a downgrade and is rejected with `forbidden` if
/// it exceeds the tier ceiling or the supervisor's own granted class. This is
/// the §2.3 ceiling invariant, enforced explicitly by the tagma as the trusted
/// reference monitor (depth monotonicity alone does NOT imply it — the tier 0/1
/// and 2/3 plateaus).
///
/// Lock ordering: `registry` RwLock is held when calling this function.
/// Inside, `exec_policy.read()` acquires the per-agent `std::sync::RwLock`.
pub(crate) fn validate_subagent_request(
    registry: &crate::state::AgentRegistry,
    identity: &crate::auth::Identity,
    supervisor_id: &AgentId,
    workspace_root: &std::path::Path,
    requested_class: Option<PermissionClass>,
    requested_mode: DelegationMode,
) -> Result<(PermissionProfile, ExecPolicy, PermissionClass), ApiError> {
    let supervisor_entry = registry.require_supervisor(identity, supervisor_id)?;
    // A faulted supervisor has no running task and no policy to inherit -- it
    // cannot host a new subagent.
    let supervisor = supervisor_entry
        .as_live()
        .ok_or_else(|| ApiError::conflict("supervisor is faulted; cannot spawn subagents"))?;

    // FullHandoff exclusivity: a full-handoff child takes the supervisor's
    // entire workspace write-lock, so it cannot coexist with any other child
    // (the carve topology and restore's concurrent sibling restore both require
    // the supervisor to have a single child while a full-handoff one lives).
    let existing = &supervisor.subagent_ids;
    if requested_mode == DelegationMode::FullHandoff && !existing.is_empty() {
        return Err(ApiError::conflict(
            "a full-handoff subagent requires the supervisor to have no other \
             subagents; remove them first",
        ));
    }
    for cid in existing {
        if let Some(child) = registry.get(cid)
            && child.identity().config.delegation_mode == DelegationMode::FullHandoff
        {
            return Err(ApiError::conflict(
                "supervisor already has a full-handoff subagent; remove it \
                 before spawning others",
            ));
        }
    }

    let supervisor_perms = &supervisor.identity.config.permissions;
    if supervisor_perms.max_depth == 0 {
        return Err(ApiError::forbidden(
            "supervisor has no remaining delegation depth",
        ));
    }
    let subagent_ws = workspace_root
        .canonicalize()
        .map_err(|e| ApiError::bad_request(format!("invalid workspace_root: {e}")))?;
    if !subagent_ws.starts_with(&supervisor_perms.workspace_root) {
        return Err(ApiError::forbidden(
            "workspace_root must be within supervisor's workspace",
        ));
    }
    // FullHandoff transfers the supervisor's ENTIRE workspace write-lock to the
    // child via an exact-path `transfer(supervisor, child, ws)`. A proper
    // subdirectory would leave the supervisor holding its own root, so the
    // transfer is a `NotOwner` no-op and the child silently carves out the
    // subdirectory -- while the registry records FullHandoff and still enforces
    // its exclusivity, invisibly losing the handoff contract. Require identity.
    if requested_mode == DelegationMode::FullHandoff
        && subagent_ws != supervisor_perms.workspace_root
    {
        return Err(ApiError::bad_request(
            "full_handoff requires the subagent workspace to be the supervisor's \
             entire workspace root",
        ));
    }

    let permissions = PermissionProfile::subagent(subagent_ws, supervisor_perms.max_depth);

    // Ceiling invariant (`.draft/design/agent-sandbox.md` §2.3): the child's
    // granted permission class cannot exceed its model tier's ceiling, nor its
    // supervisor's granted class. The decision is delegated to
    // `resolve_granted_class`, a pure function unit-tested in isolation (the
    // depth monotonicity alone does NOT imply the ceiling monotonicity — tier
    // 0/1 share Normal, 2/3 share Guest — so these are explicit checks).
    let ceiling = PermissionClass::ceiling_for_tier(permissions.depth());
    let supervisor_class = supervisor.identity.config.permissions_class;
    let granted = resolve_granted_class(ceiling, supervisor_class, requested_class)?;

    // FullHandoff transfers the supervisor's workspace WRITE-lock to the child,
    // so the child must be Normal (a Guest is readonly: it skips the workspace
    // lock entirely, so a Guest FullHandoff would silently lose the lock on
    // reactivation -- release_all(child) clears it and the Guest never
    // re-acquires). Reject the combination up front.
    if requested_mode == DelegationMode::FullHandoff && granted != PermissionClass::Normal {
        return Err(ApiError::bad_request(
            "full-handoff requires permission_class normal (a Guest is readonly and \
             cannot hold the workspace write-lock)",
        ));
    }

    // The exec-policy is inherited from the supervisor (monotone: the tagma
    // validates the child stays at least as strict on the PUT path). The classify
    // preset is tagma-global, not per-agent, so it is not inherited here.
    let exec_policy = supervisor
        .agent
        .exec_policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    Ok((permissions, exec_policy, granted))
}

/// Parse the optional `permission_class` wire string (lowercase `"normal"` /
/// `"guest"`) into a typed class. A client spelling error is a `400 Bad
/// Request` here — distinct from the `403 Forbidden` the reference monitor
/// returns for a class that parses fine but exceeds the ceiling/supervisor.
pub(crate) fn parse_requested_class(
    raw: &Option<String>,
) -> Result<Option<PermissionClass>, ApiError> {
    use std::str::FromStr;
    match raw {
        None => Ok(None),
        Some(s) => Ok(Some(
            PermissionClass::from_str(s).map_err(|e| ApiError::bad_request(e.to_string()))?,
        )),
    }
}

/// Pure reference-monitor decision for the §2.3 ceiling invariant, separated
/// from `validate_subagent_request` so it can be unit-tested without building
/// a full `Agent`/registry. Returns the class to actually grant.
///
/// - `None` requested -> grant the tier `ceiling` (historical default).
/// - An explicit request is a **downgrade only**: anything above the ceiling or
///   the supervisor's own granted class is rejected with `forbidden`, never
///   silently clamped, so a caller mistake surfaces loudly. Because the gate
///   compares granted (not ceiling) classes, a supervisor that was itself
///   downgraded can no longer grant a child at its tier's default ceiling — the
///   intended "weak model can never escalate" property.
pub(crate) fn resolve_granted_class(
    ceiling: PermissionClass,
    supervisor_class: PermissionClass,
    requested: Option<PermissionClass>,
) -> Result<PermissionClass, ApiError> {
    let granted = requested.unwrap_or(ceiling);
    if granted > ceiling {
        return Err(ApiError::forbidden(format!(
            "requested permission class {granted} exceeds tier ceiling {ceiling}"
        )));
    }
    if granted > supervisor_class {
        return Err(ApiError::forbidden(format!(
            "requested permission class {granted} exceeds supervisor's {supervisor_class}"
        )));
    }
    Ok(granted)
}
