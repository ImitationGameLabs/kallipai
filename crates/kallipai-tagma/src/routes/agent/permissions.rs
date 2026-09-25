//! Subagent permission validation: supervisor checks and the
//! requested-vs-granted permission-class resolution.

use kallipai_common::agentid::AgentId;
use kallipai_common::policy::ExecPolicy;
use kallipai_common::protocol::ApiError;
use kallipai_runtime::config::{DelegationMode, PermissionClass, PermissionProfile};

/// Validate supervisor constraints for a subagent creation request.
///
/// Returns `(PermissionProfile, ExecPolicy, PermissionClass)` for the
/// new subagent if valid. The subagent inherits the supervisor's exec-policy
/// overrides (cloned), so monotonic strictness holds at creation. The classify
/// preset is tagma-global, so it is not part of the per-agent inheritance.
///
/// `requested_class` is the explicit class from the spawn request (already
/// parsed from the wire string by the caller — the field is required on the
/// wire). It is treated as a downgrade and is rejected with `forbidden` if
/// it exceeds the supervisor's own granted class. This class
/// invariant is enforced explicitly by the tagma as the trusted reference
/// monitor.
///
/// Lock ordering: `registry` RwLock is held when calling this function.
/// Inside, `exec_policy.read()` acquires the per-agent `std::sync::RwLock`.
pub(crate) fn validate_subagent_request(
    registry: &crate::state::AgentRegistry,
    identity: &crate::auth::Identity,
    supervisor_id: &AgentId,
    workspace_root: &std::path::Path,
    requested_class: PermissionClass,
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

    // Class invariant: the child's granted permission class is
    // explicit and can only be a downgrade of its supervisor's own
    // granted class. The decision is delegated to
    // `resolve_granted_class`, a pure function unit-tested in isolation.
    let supervisor_class = supervisor.identity.config.permissions_class;
    let granted = resolve_granted_class(supervisor_class, requested_class)?;

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

/// Parse the required `permission_class` wire string (lowercase `"normal"` /
/// `"guest"`) into a typed class. A client spelling error is a `400 Bad
/// Request` here — distinct from the `403 Forbidden` the reference monitor
/// returns for a class that parses fine but exceeds the supervisor's.
pub(crate) fn parse_requested_class(raw: &str) -> Result<PermissionClass, ApiError> {
    use std::str::FromStr;
    PermissionClass::from_str(raw).map_err(|e| ApiError::bad_request(e.to_string()))
}

/// Pure reference-monitor decision for the class invariant, separated
/// from `validate_subagent_request` so it can be unit-tested without building
/// a full `Agent`/registry. Returns the class to actually grant.
///
/// The class is always an explicit request, and a grant can only ever be a
/// **downgrade**: anything above the supervisor's own granted class is
/// rejected with `forbidden`, never silently clamped, so a caller mistake
/// surfaces loudly. Because the gate compares granted classes, a supervisor
/// that was itself downgraded can no longer grant a child above itself —
/// the intended "weak supervisor can never escalate" property.
pub(crate) fn resolve_granted_class(
    supervisor_class: PermissionClass,
    requested: PermissionClass,
) -> Result<PermissionClass, ApiError> {
    if requested > supervisor_class {
        return Err(ApiError::forbidden(format!(
            "requested permission class {requested} exceeds supervisor's {supervisor_class}"
        )));
    }
    Ok(requested)
}
