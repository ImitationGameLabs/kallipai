//! Default-set transfer and set removal, with the reference scan that
//! decides whether an agent would be stranded.

use super::shared::{persist_config, validate_config};
use super::source::require_local_source;
use super::wire::masked_config;
use crate::auth::AuthIdentity;
use crate::state::SharedState;
use axum::Json;
use axum::extract::{Path, Query, State};
use kallipai_common::protocol::{ApiError, DeleteSetResponse, SetDefaultRequest, SetReference};
use serde::Deserialize;
use std::sync::Arc;
use tracing::info;

/// PUT /profiles/default — transfer the default-set marker to an existing set.
///
/// Operator-only. Persisted and swapped like PUT /profiles; running agents
/// are unaffected — new spawns and restores without a recorded binding
/// resolve against the new default.
pub async fn set_default_profile_set(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Json(body): Json<SetDefaultRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    require_local_source(&state)?;
    let bundle = state.profiles.load();
    if !bundle.config.sets.contains_key(&body.default) {
        return Err(ApiError::bad_request(format!(
            "unknown set '{}'; available: {}",
            body.default,
            bundle
                .config
                .sets
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let mut config = bundle.config.clone();
    config.default = body.default.clone();
    persist_config(&config);
    state.profiles.store(Arc::new(crate::state::ProfileBundle {
        config: config.clone(),
        registry: bundle.registry.clone(),
    }));
    info!(default = %config.default, "default profile set transferred");
    Ok(Json(masked_config(&config)?))
}

#[derive(Deserialize)]
pub(crate) struct DeleteSetQuery {
    pub force: Option<bool>,
}

/// Scan the registry for agents bound to `name`: the reference list plus
/// whether any of them is the root (`created_by = None`).
async fn scan_references(state: &SharedState, name: &str) -> (Vec<SetReference>, bool) {
    let registry = state.registry.read().await;
    let mut refs = Vec::new();
    let mut root = false;
    for (id, entry) in registry.iter() {
        let cfg = &entry.identity().config;
        if cfg.profile_set.as_deref() != Some(name) {
            continue;
        }
        if cfg.created_by.is_none() {
            root = true;
        }
        refs.push(SetReference {
            id: id.clone(),
            role: cfg.role.clone(),
        });
    }
    (refs, root)
}

/// Gate for the delete sweep: a post-interrupt reference scan blocks the
/// delete when it shows a reference outside the interrupted set, or the root
/// landed on the set. Interrupted records keep their binding (the dangling
/// state the delivery gate rejects), so "new" means outside the interrupted
/// set — not merely non-empty.
pub(super) fn references_grew(
    interrupted: &[SetReference],
    recheck: &[SetReference],
    root_after: bool,
) -> bool {
    root_after
        || recheck
            .iter()
            .any(|r| !interrupted.iter().any(|o| o.id == r.id))
}

/// DELETE /profiles/sets/{name} — remove a set, interrupting bound agents.
///
/// Refuses the default set (transfer it first) and the root's set (the root
/// has no supervisor to rebind it). Other referenced sets need `?force=true`:
/// each bound agent is interrupted (faulted binders are skipped — their
/// record is already in the terminal state an interrupt produces),
/// references are re-validated (the interrupt-to-persist window can admit
/// a new binding), and only a sweep free of NEW references persists.
/// Interrupted agents keep their now-dangling record — delivery rejects
/// them with a rebind hint until rebound (PUT /agents/{id}/profile-set).
pub async fn delete_profile_set(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Path(name): Path<String>,
    Query(q): Query<DeleteSetQuery>,
) -> Result<Json<DeleteSetResponse>, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    require_local_source(&state)?;
    let bundle = state.profiles.load();
    if !bundle.config.sets.contains_key(&name) {
        return Err(ApiError::not_found(format!("unknown set '{name}'")));
    }
    if name == bundle.config.default {
        return Err(ApiError::conflict(
            "the default set cannot be deleted; transfer it first (PUT /profiles/default)",
        ));
    }
    let (references, root_bound) = scan_references(&state, &name).await;
    if root_bound {
        return Err(ApiError::conflict(
            "the root agent is bound to this set; rebind it first (PUT /agents/{id}/profile-set)",
        ));
    }
    let interrupted = if references.is_empty() {
        Vec::new()
    } else {
        if q.force != Some(true) {
            let ids = references
                .iter()
                .map(|r| r.id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ApiError::conflict(format!(
                "set '{name}' is still bound by: {ids}; pass force=true to interrupt and remove"
            )));
        }
        for r in &references {
            // A faulted binder has no live round to interrupt — its
            // terminal state (a dangling record the delivery gate
            // rejects until a rebind) is exactly what interrupting a
            // live agent produces, so skip straight to it. Restored
            // records all register as Faulted, so without this skip a
            // referenced set could never be force-deleted after a
            // restart.
            let is_live = {
                let registry = state.registry.read().await;
                registry.get(&r.id).is_some_and(|e| e.as_live().is_some())
            };
            if is_live {
                crate::routes::agent::interrupt_core(&state, &r.id).await?;
            }
        }
        // The interrupt-to-persist window can admit a new binding —
        // re-validate before the write, or a spawn landing in the gap would
        // strand on a set that no longer exists. Interrupted records keep
        // their binding (the dangling state the delivery gate rejects), so
        // "new" means a reference outside the interrupted set.
        let (recheck, root_after) = scan_references(&state, &name).await;
        if references_grew(&references, &recheck, root_after) {
            return Err(ApiError::conflict(
                "new bindings arrived during the sweep; retry the delete",
            ));
        }
        references
    };
    let mut config = bundle.config.clone();
    config.sets.remove(&name);
    let registry = validate_config(&config)?;
    persist_config(&config);
    state.profiles.store(Arc::new(crate::state::ProfileBundle {
        config: config.clone(),
        registry,
    }));
    info!(set = %name, interrupted = interrupted.len(), "profile set removed");
    Ok(Json(DeleteSetResponse {
        removed: name,
        interrupted,
    }))
}
