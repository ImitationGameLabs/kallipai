//! Profile management API: read, update, and apply model profiles online.
//!
//! GET /profiles — return the current profile configuration with api_keys masked.
//! PUT /profiles — merge tri-state wire api_keys, validate, persist to disk, and
//!   hot-swap the in-memory registry.
//!   Does NOT affect running agents — new agents pick up the swap at spawn.
//! POST /profiles/apply — push the current registry to all live agents via a
//!   pending-reset cell; each agent rebuilds its failover state on its next
//!   wake-up. An unbound root also picks up a derived default-set binding.
//! PUT /profiles/default — transfer the default-set marker to an existing set.
//! DELETE /profiles/sets/{name} — remove a set; bound agents are interrupted
//!   (force=true) and keep a dangling record the delivery gate rejects with
//!   a rebind hint.
//!
//! The handlers live in submodules grouped by what they act on: `read` (the
//! masked snapshot and the source refresh), `wire` (the wire shapes and the
//! tri-state merge), `source` (source switching and the selection
//! write-through), `apply` (the live broadcast and the degraded re-pull),
//! `shared` (validation, persistence, reset) and `sets` (default-set
//! transfer, set removal). The re-exports below keep the names callers
//! outside the module use resolving under `crate::routes::profiles::`;
//! `put_profiles` stays here as the assembly point.

mod apply;
mod read;
mod sets;
mod shared;
mod source;
mod wire;

pub use apply::apply_profiles;
pub use read::{get_profiles, refresh_profile_source};
pub use sets::{delete_profile_set, set_default_profile_set};

pub(crate) use apply::spawn_degraded_repull_task;
pub(crate) use shared::signal_profile_reset;
pub(crate) use wire::ProfileConfigWire;

use crate::auth::AuthIdentity;
use crate::profile_source::ProfileSourceKind;
use crate::state::SharedState;
use axum::Json;
use axum::extract::State;
use kallipai_common::protocol::ApiError;
use shared::{dangling_bindings, persist_config, validate_config};
use source::{require_local_source, switch_profile_source, update_profile_selection};
use std::sync::Arc;
use tracing::{info, warn};
use wire::{masked_config, merge_wire};

/// PUT /profiles — validate, persist, and hot-swap the profile registry.
///
/// Accepts a wire config where each endpoint's `api_key` is tri-state: null keeps
/// the live key, a string replaces it, and the masked form echoed back counts as
/// "keep" (round-trip safe). `base_url` follows the same null-keeps rule; an
/// empty string resets it to the family default. Merged against the live
/// config, validated by building backends + a trial registry (fail-fast). On
/// success, writes to disk and swaps the `ArcSwap`; running agents are
/// unaffected until an explicit apply.
pub async fn put_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Json(mut wire): Json<ProfileConfigWire>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    // A switch request is a PUT whose source.mode differs from the live
    // kind, or a rebind: same mode naming another platform (the
    // requested polis differs from the active origin). Both take the
    // dedicated branch (the profiles payload is ignored; the switch
    // resolves and validates the target face, moves the pin, and
    // carries a selection natively; the same dangling-bindings gate
    // runs, so the wire's force field covers it).
    // A same-mode source member carrying a collection selection
    // is the selection write-through (same gate); any other
    // same-mode member (or none) is a normal edit PUT and the
    // source guard applies as before.
    if let Some(requested) = wire.source.take() {
        let current = state.profile_source.load_full().source().kind();
        let rebind_to_other_platform = requested.mode == current
            && current == ProfileSourceKind::Proxy
            && requested.polis.as_deref().is_some_and(|target| {
                state
                    .gateway_params
                    .load_full()
                    .as_ref()
                    .as_ref()
                    .is_some_and(|params| params.origin != target)
            });
        if requested.mode != current || rebind_to_other_platform {
            return switch_profile_source(&state, requested, wire.force)
                .await
                .map(Json);
        }
        if let Some(target) = requested.collection.as_ref() {
            return update_profile_selection(&state, target, wire.force)
                .await
                .map(Json);
        }
    }
    require_local_source(&state)?;
    let force = wire.force;

    // The sentinel endpoint id is reserved (it routes the profile-less
    // root to the placeholder backend); a user profile named the same
    // would silently collide with that switch.
    for id in wire.endpoints.keys() {
        if id == crate::backend::UNCONFIGURED {
            return Err(ApiError::bad_request(format!(
                "endpoint id '{}' is reserved",
                crate::backend::UNCONFIGURED
            )));
        }
    }
    let config = merge_wire(&state.profiles.load().config, wire)?;
    // Reference integrity: a set some agent still records must not vanish
    // over a wholesale PUT — that would strand the agent (its delivery is
    // rejected as dangling). Force the operator through the set-removal
    // flow, which interrupts the bound agents first. With `force` the
    // operator confirms the stranding in the UI and the save proceeds.
    let stranded = dangling_bindings(&state, &config).await;
    if !stranded.is_empty() && !force {
        return Err(ApiError::conflict_dangling(
            format!(
                "config drops sets still bound by agents: {}; re-PUT with force=true, or use DELETE /profiles/sets/{{name}} to remove a referenced set",
                stranded.join(", ")
            ),
            stranded,
        ));
    }
    let registry = validate_config(&config)?;
    // Declared-capability shrinkage inside a set is never silent on the PUT
    // path either; load_file warns the same way at startup.
    for (name, effective) in kallipai_adk::profile::shadowed_set_notices(&config.sets) {
        warn!(
            set = %name,
            effective = %effective,
            "profile set has members declaring beyond the effective intersection; the intersection governs"
        );
    }

    persist_config(&config);

    // Swap the ArcSwap atomically.
    let bundle = crate::state::ProfileBundle {
        config: config.clone(),
        registry,
    };
    state.profiles.store(Arc::new(bundle));

    info!("profile registry hot-swapped");
    Ok(Json(masked_config(&config)?))
}

#[cfg(test)]
mod tests;
