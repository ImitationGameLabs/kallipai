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

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};

use just_llm_client::types::generation::ReasoningEffort;
use kallip_common::protocol::{
    ApiError, DeleteSetResponse, Modality, SetDefaultRequest, SetReference,
};
use kallip_runtime::profile::{
    Profile, ProfileConfig, ProfileRegistry, ProfileSet, Provider, is_valid_set_name,
    normalize_default,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::AuthIdentity;
use crate::probe::mask_key;
use crate::state::SharedState;

/// GET /profiles — return the current profile configuration.
///
/// Operator-only: profiles contain API keys.
pub async fn get_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    let bundle = state.profiles.load();
    Ok(Json(masked_config(&bundle.config)?))
}

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
    Json(wire): Json<ProfileConfigWire>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
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
    for (name, effective) in kallip_runtime::profile::shadowed_set_notices(&config.sets) {
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

/// Wire DTO for PUT /profiles: same shape as [`ProfileConfig`] except each
/// endpoint's `api_key` and `base_url` are tri-state (see [`merge_wire`]).
#[derive(Deserialize)]
struct ProviderPatch {
    id: String,
    family: String,
    api_key: Option<String>,
    base_url: Option<String>,
}

#[derive(Deserialize)]
struct ProfileWire {
    id: String,
    endpoint: String,
    model: String,
    max_context_window: usize,
    store: Option<bool>,
    effort: Option<ReasoningEffort>,
    /// Absent on the wire = the text-only default (matching the runtime
    /// model's TOML semantics), so older clients that omit the key keep
    /// their declarations intact across a PUT.
    #[serde(default = "kallip_runtime::profile::Profile::default_modalities")]
    modalities: Vec<Modality>,
}

#[derive(Deserialize)]
struct SetWire {
    /// The set name doubles as its map key; the config schema keeps a single
    /// source of truth for it.
    name: String,
    description: Option<String>,
    profiles: Vec<ProfileWire>,
}

#[derive(Deserialize)]
pub(crate) struct ProfileConfigWire {
    sets: Vec<SetWire>,
    /// Tri-state like the endpoint fields: an absent key lets the collection
    /// resolve (exactly one set becomes the default), a present name must
    /// reference one of `sets`.
    #[serde(default)]
    default: Option<String>,
    endpoints: HashMap<String, ProviderPatch>,
    /// Tri-state like the endpoint fields: an absent key keeps the live
    /// parking (an old client PUTting its full config cannot clear it);
    /// a present list replaces it.
    #[serde(default)]
    parking: Option<Vec<ProfileWire>>,
    /// Operator-confirmed acceptance of dangling profile-set bindings: when
    /// true, `put_profiles` skips the dangling-bindings gate.
    #[serde(default)]
    force: bool,
}

/// Resolve the wire tri-state `api_key` and `base_url` fields against the live
/// config into a full [`ProfileConfig`] carrying real keys.
fn merge_wire(live: &ProfileConfig, wire: ProfileConfigWire) -> Result<ProfileConfig, ApiError> {
    let mut endpoints = HashMap::new();
    for (key, ep) in wire.endpoints {
        if key != ep.id {
            return Err(ApiError::bad_request(format!(
                "provider map key '{key}' does not match id '{}'",
                ep.id
            )));
        }
        let api_key = match ep.api_key {
            None => live
                .endpoints
                .get(&key)
                .map(|e| e.api_key.clone())
                .ok_or_else(|| {
                    ApiError::bad_request(format!("provider '{key}' is new; api_key is required"))
                })?,
            Some(k) if k.is_empty() => {
                return Err(ApiError::bad_request(format!(
                    "provider '{key}': api_key must not be empty"
                )));
            }
            Some(k) => match live.endpoints.get(&key) {
                // Round-trip safety: the masked form echoed back means "keep".
                Some(e) if mask_key(&e.api_key) == k => e.api_key.clone(),
                _ => k,
            },
        };

        // `base_url` mirrors the api_key tri-state so a partial PUT that omits
        // it keeps the live URL instead of silently clearing it: null keeps,
        // "" resets to the family default, a value replaces.
        let base_url = match ep.base_url {
            None => live.endpoints.get(&key).and_then(|e| e.base_url.clone()),
            Some(url) if url.is_empty() => None,
            Some(url) => Some(url),
        };
        endpoints.insert(
            key.clone(),
            Provider {
                id: ep.id,
                family: ep.family,
                api_key,
                base_url,
            },
        );
    }
    let mut sets: std::collections::BTreeMap<String, ProfileSet> =
        std::collections::BTreeMap::new();
    for s in wire.sets {
        if !is_valid_set_name(&s.name) {
            return Err(ApiError::bad_request(format!(
                "set name '{}' must match ^[A-Za-z0-9_-]+$ (letters, digits, '_', '-')",
                s.name
            )));
        }
        let profiles = s
            .profiles
            .into_iter()
            .map(|p| Profile {
                id: p.id,
                endpoint: p.endpoint,
                model: p.model,
                max_context_window: p.max_context_window,
                store: p.store,
                effort: p.effort,
                modalities: p.modalities,
            })
            .collect();
        let set = ProfileSet {
            name: s.name.clone(),
            description: s.description,
            profiles,
        };
        if sets.insert(s.name.clone(), set).is_some() {
            return Err(ApiError::bad_request(format!(
                "duplicate set name '{}'",
                s.name
            )));
        }
    }
    let default = normalize_default(&sets, wire.default.as_deref().unwrap_or(""))
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?
        .0;
    // Resolve the final parking first: absent key keeps the live list (the
    // tri-state rule), a present list replaces it wholesale. The seen-set
    // below scans sets ∪ this final parking, so a set referencing an id
    // that only exists in the kept live parking is caught at the wire
    // boundary, not on the next startup's file validate().
    let parking: Vec<Profile> = match wire.parking {
        None => live.parking.clone(),
        Some(list) => list
            .into_iter()
            .map(|p| Profile {
                id: p.id,
                endpoint: p.endpoint,
                model: p.model,
                max_context_window: p.max_context_window,
                store: p.store,
                effort: p.effort,
                modalities: p.modalities,
            })
            .collect(),
    };
    // Cross-set duplicate profile ids pass registry validation but the stricter
    // load-time validate() bails on them — a config the tagma could no longer
    // start from. Reject here so the wire boundary matches the file boundary.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for set in sets.values() {
        for p in &set.profiles {
            if !seen.insert(p.id.as_str()) {
                return Err(ApiError::bad_request(format!(
                    "duplicate profile id '{}'",
                    p.id
                )));
            }
        }
    }
    for p in &parking {
        if !seen.insert(p.id.as_str()) {
            return Err(ApiError::bad_request(format!(
                "duplicate profile id '{}'",
                p.id
            )));
        }
    }
    // Text-membership rule, same as the file boundary's validate(): a
    // profile that cannot accept text is refused, so a PUT cannot persist
    // a config the tagma could no longer start from (absent declarations
    // default to [text] and never trip).
    for set in sets.values() {
        for p in &set.profiles {
            kallip_runtime::profile::require_text_member("set", p)
                .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
        }
    }
    for p in &parking {
        kallip_runtime::profile::require_text_member("parking", p)
            .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    }
    Ok(ProfileConfig {
        sets,
        default,
        parking,
        endpoints,
    })
}

/// Serialize a config with every endpoint's `api_key` replaced by its masked form —
/// the shape GET and PUT responses return.
fn masked_config(config: &ProfileConfig) -> Result<serde_json::Value, ApiError> {
    let mut value = serde_json::to_value(config)
        .map_err(|e| ApiError::internal(format!("profile serialization failed: {e}")))?;
    if let Some(endpoints) = value.get_mut("endpoints").and_then(|v| v.as_object_mut()) {
        for ep in endpoints.values_mut() {
            if let Some(masked) = ep.get("api_key").and_then(|k| k.as_str()).map(mask_key) {
                ep["api_key"] = serde_json::Value::String(masked);
            }
        }
    }
    Ok(value)
}
/// Response body for POST /profiles/apply.
#[derive(Serialize)]
pub struct ApplyResponse {
    /// Number of live agents that received a pending-reset signal.
    pub applied: usize,
    /// Number of agents skipped: faulted entries and live agents whose
    /// recorded set no longer resolves.
    pub skipped: usize,
}

/// POST /profiles/apply — push the current registry to all live agents.
///
/// For each live agent, resolves its recorded set name against the new
/// registry and writes a [`ProfileReset`](kallip_runtime::ProfileReset) to the agent's pending-reset
/// cell. An unbound root instead derives its binding from the current
/// default set, and the derived name is written back to the in-memory
/// record (not meta.json — restore re-derives it on the next boot).
/// The agent picks it up on its next wake-up (top of
/// `run_and_report`). Agents mid-round finish their current work first.
pub async fn apply_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
) -> Result<Json<ApplyResponse>, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    let bundle = state.profiles.load();
    let registry = bundle.registry.clone();

    let mut applied = 0usize;

    // Collect the reset targets under the read lock, then apply outside it
    // so register/remove writes are not blocked during cell writes + notify.
    let (targets, rebinds, non_live): (Vec<_>, Vec<_>, usize) = {
        let registry_guard = state.registry.read().await;
        registry_guard.iter().fold(
            (Vec::new(), Vec::new(), 0usize),
            |(mut targets, mut rebinds, mut skipped), (id, entry)| {
                let Some(live) = entry.as_live() else {
                    skipped += 1;
                    return (targets, rebinds, skipped);
                };
                // Resolve through the bundle's default-set fallback (the one
                // resolution rule for every consumer): an unbound record
                // resolves to the default and is rebound in memory; a
                // dangling root binding resolves through the fallback
                // without rebinding (its recorded name is kept). A still-
                // dangling binding is skipped, not guessed at. The rebind
                // never writes meta.json (restore re-derives it on the
                // next boot).
                let recorded = live.identity.config.profile_set.clone();
                let Ok(set) = bundle
                    .resolve_with_fallback(recorded.as_deref(), live.identity.config.is_root())
                else {
                    skipped += 1;
                    return (targets, rebinds, skipped);
                };
                if recorded.is_none() {
                    rebinds.push((id.clone(), Some(set.name.clone())));
                }
                targets.push((
                    kallip_runtime::ProfileReset {
                        set: set.clone(),
                        registry: registry.clone(),
                    },
                    live.agent.pending_profile_reset.clone(),
                    live.agent.notify.clone(),
                ));
                (targets, rebinds, skipped)
            },
        )
    };
    let skipped = non_live;

    // Write the derived root bindings back to the in-memory records so
    // the delivery gate sees them without a restart.
    if !rebinds.is_empty() {
        let mut registry_guard = state.registry.write().await;
        for (id, name) in rebinds {
            if let Some(entry) = registry_guard.get_mut(&id)
                && let Some(live) = entry.as_live_mut()
            {
                live.identity.config.profile_set = name;
            }
        }
    }

    for (reset, cell_lock, notify) in targets {
        signal_profile_reset(reset, &cell_lock, &notify);
        applied += 1;
    }

    info!(applied, skipped, "profile apply signaled to live agents");
    Ok(Json(ApplyResponse { applied, skipped }))
}

/// Build backends and a trial registry for the candidate config; nothing
/// changes on failure. Shared by every set-management mutation.
fn validate_config(config: &ProfileConfig) -> Result<Arc<ProfileRegistry>, ApiError> {
    let factory = just_llm_client::client::BackendFactory::new();
    let user_agent = crate::backend::DEFAULT_USER_AGENT;
    let source = crate::backend::build_backends(config, factory, user_agent)
        .map_err(|e| ApiError::bad_request(format!("profile validation failed: {e:#}")))?;
    Ok(Arc::new(
        ProfileRegistry::new(config.sets.clone(), source)
            .map_err(|e| ApiError::bad_request(format!("invalid profile registry: {e:#}")))?,
    ))
}

/// Persist the config to disk (best-effort: a failure is logged but does not
/// block the swap, since the in-memory state is already validated). Shared
/// by every set-management mutation.
fn persist_config(config: &ProfileConfig) {
    match kallip_runtime::profile::config_path() {
        Ok(path) => {
            if let Err(e) = kallip_runtime::profile::save(config, &path) {
                warn!(path = %path.display(), "failed to persist profiles to disk: {e:#}");
            } else {
                info!(path = %path.display(), "profiles persisted to disk");
            }
        }
        Err(e) => {
            warn!("cannot resolve profiles config path for persistence: {e:#}");
        }
    }
}

/// Collect the profile-set bindings that the candidate config would strand:
/// each entry renders as `agent-id → 'set'`. Consumed by `put_profiles` to
/// reject (or, with `force`, accept) a wholesale save that drops a set some
/// agent still records.
async fn dangling_bindings(state: &SharedState, config: &ProfileConfig) -> Vec<String> {
    let registry = state.registry.read().await;
    registry
        .iter()
        .filter_map(|(id, entry)| {
            let binding = entry.identity().config.profile_set.as_deref()?;
            (!config.sets.contains_key(binding)).then(|| format!("{id} → '{binding}'"))
        })
        .collect()
}

/// Write a ProfileReset into the agent's pending cell and wake it — the
/// shared tail of apply and the explicit per-agent rebind.
pub(crate) fn signal_profile_reset(
    reset: kallip_runtime::ProfileReset,
    cell: &std::sync::Mutex<Option<kallip_runtime::ProfileReset>>,
    notify: &tokio::sync::Notify,
) {
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(reset);
    drop(guard);
    notify.notify_one();
}

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
fn references_grew(
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
                super::agent::interrupt_core(&state, &r.id).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_wire_profile_behavior_fields_pass_through() {
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "main": { "id": "main", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [
                { "name": "a", "profiles": [
                    { "id": "tuned", "endpoint": "main", "model": "m", "max_context_window": 8, "store": false, "effort": "low" },
                    { "id": "plain", "endpoint": "main", "model": "m2", "max_context_window": 8 }
                ] }
            ],
            "parking": [
                { "id": "parked", "endpoint": "main", "model": "m3", "max_context_window": 8, "store": false, "effort": "xhigh" }
            ],
            "default": "a"
        }))
        .unwrap();
        let cfg = merge_wire(&live_config(), w).unwrap();
        let profiles = &cfg.sets["a"].profiles;
        assert_eq!(profiles[0].store, Some(false));
        assert_eq!(profiles[0].effort, Some(ReasoningEffort::Low));
        assert_eq!(profiles[1].store, None);
        assert_eq!(profiles[1].effort, None);
        // A present parking list replaces wholesale; the behavior fields ride
        // through exactly like the sets path.
        let parked = &cfg.parking;
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].store, Some(false));
        assert_eq!(parked[0].effort, Some(ReasoningEffort::Xhigh));
    }
    use crate::state::RegistryEntry;
    use crate::test_helpers::{alt_bound_sub, make_entry_with_rx, make_state, make_state_two_sets};
    use kallip_common::agentid::AgentId;

    fn op_auth() -> AuthIdentity {
        AuthIdentity::test_new(crate::auth::Identity::Operator)
    }

    #[tokio::test]
    async fn get_profiles_masks_api_keys() {
        let state = make_state();
        // The default test endpoint carries api_key "test" (4 chars → fixed stars).
        let Json(value) = get_profiles(State(state), op_auth()).await.unwrap();
        assert_eq!(value["endpoints"]["test"]["api_key"], "********");
        // Non-key fields are untouched.
        assert_eq!(value["endpoints"]["test"]["family"], "deepseek");
    }

    #[test]
    fn references_grew_flags_only_new_ids_and_root() {
        let a = SetReference {
            id: AgentId::random(),
            role: "a".into(),
        };
        let same_again = SetReference {
            id: a.id.clone(),
            role: "a".into(),
        };
        // Interrupted records keep their binding: the same set again passes.
        assert!(!references_grew(
            std::slice::from_ref(&a),
            &[same_again],
            false
        ));
        // A brand-new id in the recheck blocks.
        let b = SetReference {
            id: AgentId::random(),
            role: "b".into(),
        };
        assert!(references_grew(&[a], &[b], false));
        // The root landing on the set blocks regardless.
        assert!(references_grew(&[], &[], true));
    }

    #[tokio::test]
    async fn set_default_transfers_marker() {
        let state = make_state_two_sets();
        let Json(cfg) = set_default_profile_set(
            State(state.clone()),
            op_auth(),
            Json(SetDefaultRequest {
                default: "alt".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(cfg["default"], "alt");
        assert_eq!(state.profiles.load().config.default, "alt");
        // The marker moves; the sets themselves do not.
        assert!(state.profiles.load().config.sets.contains_key("default"));
    }

    #[tokio::test]
    async fn set_default_rejects_unknown_set() {
        let state = make_state_two_sets();
        let err = set_default_profile_set(
            State(state),
            op_auth(),
            Json(SetDefaultRequest {
                default: "ghost".into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown set 'ghost'"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn remove_refuses_default_set() {
        let state = make_state_two_sets();
        let err = delete_profile_set(
            State(state),
            op_auth(),
            Path("default".into()),
            Query(DeleteSetQuery { force: Some(true) }),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("default set cannot be deleted"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn remove_refuses_root_bound_set() {
        let state = make_state_two_sets();
        let root = AgentId::random();
        let (mut entry, rx) = make_entry_with_rx(None, format!("agent-{root}"));
        entry.identity.config.profile_set = Some("alt".into());
        drop(rx);
        state
            .registry
            .write()
            .await
            .register(root, RegistryEntry::Live(entry));
        let err = delete_profile_set(
            State(state),
            op_auth(),
            Path("alt".into()),
            Query(DeleteSetQuery { force: Some(true) }),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("root agent is bound"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn remove_lists_references_without_force() {
        let state = make_state_two_sets();
        let sub = alt_bound_sub(&state).await;
        let err = delete_profile_set(
            State(state.clone()),
            op_auth(),
            Path("alt".into()),
            Query(DeleteSetQuery { force: None }),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains(&sub.to_string()),
            "the refusal must name the bound agent, got: {err}"
        );
        assert!(err.to_string().contains("force=true"), "got: {err}");
        // Refusal is atomic: the set is still there.
        assert!(state.profiles.load().config.sets.contains_key("alt"));
    }

    #[tokio::test]
    async fn remove_set_interrupts_users_and_deletes() {
        let state = make_state_two_sets();
        let sub = alt_bound_sub(&state).await;
        let Json(resp) = delete_profile_set(
            State(state.clone()),
            op_auth(),
            Path("alt".into()),
            Query(DeleteSetQuery { force: Some(true) }),
        )
        .await
        .unwrap();
        assert_eq!(resp.removed, "alt");
        assert_eq!(resp.interrupted.len(), 1);
        assert_eq!(resp.interrupted[0].id, sub);
        assert!(!state.profiles.load().config.sets.contains_key("alt"));
        // The interrupted record keeps its (now dangling) binding — the
        // state the delivery gate rejects until a rebind lands.
        let reg = state.registry.read().await;
        let entry = reg.get(&sub).unwrap();
        assert_eq!(entry.identity().config.profile_set.as_deref(), Some("alt"));
    }

    #[tokio::test]
    async fn remove_force_skips_faulted_binders() {
        // After a restart every restored record registers as Faulted, so
        // a referenced set must stay force-deletable: the sweep skips the
        // interrupt (nothing is running) and leaves the same dangling
        // record an interrupt would produce.
        let state = make_state_two_sets();
        let sub = AgentId::random();
        let supervisor = AgentId::random();
        let (mut entry, rx) = make_entry_with_rx(Some(supervisor), format!("agent-{sub}"));
        entry.identity.config.profile_set = Some("alt".into());
        drop(rx);
        state.registry.write().await.register(
            sub.clone(),
            RegistryEntry::Faulted(crate::state::FaultedEntry {
                identity: entry.identity,
                subagent_ids: entry.subagent_ids,
                reason: "restore".into(),
                at: 0,
            }),
        );

        let Json(resp) = delete_profile_set(
            State(state.clone()),
            op_auth(),
            Path("alt".into()),
            Query(DeleteSetQuery { force: Some(true) }),
        )
        .await
        .unwrap();
        assert_eq!(resp.removed, "alt");
        assert_eq!(
            resp.interrupted.len(),
            1,
            "faulted binders count as interrupted (same terminal state)"
        );
        assert!(!state.profiles.load().config.sets.contains_key("alt"));
        let reg = state.registry.read().await;
        assert_eq!(
            reg.get(&sub)
                .unwrap()
                .identity()
                .config
                .profile_set
                .as_deref(),
            Some("alt"),
            "the faulted record keeps its dangling binding"
        );
    }

    #[test]
    fn mask_key_shapes() {
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("12345678"), "********");
        assert_eq!(mask_key("123456789"), "1234********6789");
        assert_eq!(mask_key("sk-abcdef123456wxyz"), "sk-a********wxyz");
        // Multibyte input masks by chars, never splitting a code point.
        assert_eq!(mask_key("密钥密钥"), "********");
    }
    #[test]
    fn merge_wire_duplicate_profile_id_across_sets_is_rejected() {
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "main": { "id": "main", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [
                { "name": "a", "profiles": [ { "id": "p", "endpoint": "main", "model": "m", "max_context_window": 8 } ] },
                { "name": "b", "profiles": [ { "id": "p", "endpoint": "main", "model": "m", "max_context_window": 8 } ] }
            ],
            "default": "a"
        }))
        .unwrap();
        let err = merge_wire(&live_config(), w).unwrap_err();
        assert!(
            err.to_string().contains("duplicate profile id 'p'"),
            "got: {err}"
        );
    }
    #[test]
    fn merge_wire_rejects_set_profile_without_text() {
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "main": { "id": "main", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [
                { "name": "a", "profiles": [ { "id": "p", "endpoint": "main", "model": "m", "max_context_window": 8, "modalities": ["image"] } ] }
            ],
            "default": "a"
        }))
        .unwrap();
        let err = merge_wire(&live_config(), w).unwrap_err();
        assert!(err.to_string().contains("must accept text"), "got: {err}");
    }
    #[test]
    fn merge_wire_rejects_parking_profile_without_text() {
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "main": { "id": "main", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [
                { "name": "a", "profiles": [ { "id": "ok", "endpoint": "main", "model": "m", "max_context_window": 8 } ] }
            ],
            "parking": [
                { "id": "pk", "endpoint": "main", "model": "m", "max_context_window": 8, "modalities": ["image"] }
            ],
            "default": "a"
        }))
        .unwrap();
        let err = merge_wire(&live_config(), w).unwrap_err();
        assert!(err.to_string().contains("must accept text"), "got: {err}");
    }

    #[test]
    fn merge_wire_rejects_invalid_set_name() {
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "main": { "id": "main", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [
                { "name": "bad name!", "profiles": [ { "id": "p", "endpoint": "main", "model": "m", "max_context_window": 8 } ] }
            ]
        }))
        .unwrap();
        let err = merge_wire(&live_config(), w).unwrap_err();
        assert!(
            err.to_string().contains("set name 'bad name!'"),
            "got: {err}"
        );
    }

    #[test]
    fn merge_wire_rejects_duplicate_set_name() {
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "main": { "id": "main", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [
                { "name": "a", "profiles": [ { "id": "p1", "endpoint": "main", "model": "m", "max_context_window": 8 } ] },
                { "name": "a", "profiles": [ { "id": "p2", "endpoint": "main", "model": "m", "max_context_window": 8 } ] }
            ],
            "default": "a"
        }))
        .unwrap();
        let err = merge_wire(&live_config(), w).unwrap_err();
        assert!(
            err.to_string().contains("duplicate set name 'a'"),
            "got: {err}"
        );
    }

    #[test]
    fn merge_wire_rejects_default_naming_missing_set() {
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "main": { "id": "main", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [
                { "name": "a", "profiles": [ { "id": "p", "endpoint": "main", "model": "m", "max_context_window": 8 } ] }
            ],
            "default": "ghost"
        }))
        .unwrap();
        let err = merge_wire(&live_config(), w).unwrap_err();
        assert!(
            err.to_string()
                .contains("default set name 'ghost' does not match any set"),
            "got: {err}"
        );
    }

    #[test]
    fn merge_wire_absent_parking_keeps_live() {
        let merged = merge_wire(
            &live_config(),
            wire(serde_json::Value::Null, serde_json::Value::Null),
        )
        .unwrap();
        assert_eq!(merged.parking.len(), 1);
        assert_eq!(merged.parking[0].id, "parked");
    }

    #[test]
    fn merge_wire_parking_list_replaces() {
        let mut w = wire(serde_json::Value::Null, serde_json::Value::Null);
        w.parking = Some(vec![ProfileWire {
            id: "new".into(),
            endpoint: "main".into(),
            model: "m2".into(),
            max_context_window: 8,
            store: None,
            effort: None,
            modalities: Profile::default_modalities(),
        }]);
        let merged = merge_wire(&live_config(), w).unwrap();
        assert_eq!(merged.parking.len(), 1);
        assert_eq!(merged.parking[0].id, "new");

        // An explicitly empty list clears (absent ≠ empty on this field).
        let mut w = wire(serde_json::Value::Null, serde_json::Value::Null);
        w.parking = Some(vec![]);
        let merged = merge_wire(&live_config(), w).unwrap();
        assert!(merged.parking.is_empty());
    }

    #[test]
    fn merge_wire_rejects_set_id_colliding_with_kept_live_parking() {
        // The set references an id that only exists in the kept live parking;
        // the seen set must scan the final parking, not just the wire.
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "main": { "id": "main", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [
                { "name": "a", "profiles": [ { "id": "parked", "endpoint": "main", "model": "m", "max_context_window": 8 } ] }
            ]
        }))
        .unwrap();
        let err = merge_wire(&live_config(), w).unwrap_err();
        assert!(
            err.to_string().contains("duplicate profile id 'parked'"),
            "got: {err}"
        );
    }

    fn live_config() -> ProfileConfig {
        ProfileConfig {
            default: String::new(),
            sets: std::collections::BTreeMap::new(),
            endpoints: std::collections::HashMap::from([(
                "main".into(),
                Provider {
                    id: "main".into(),
                    family: "deepseek".into(),
                    api_key: "sk-live-secret-key".into(),
                    base_url: Some("https://live.example/v1".into()),
                },
            )]),
            // Live draft space: one parked profile (dangling-free but out of rotation).
            parking: vec![Profile {
                id: "parked".into(),
                endpoint: "main".into(),
                model: "pm".into(),
                max_context_window: 64_000,
                store: None,
                effort: None,
                modalities: Profile::default_modalities(),
            }],
        }
    }

    fn wire(api_key: serde_json::Value, base_url: serde_json::Value) -> ProfileConfigWire {
        serde_json::from_value(serde_json::json!({
            "endpoints": { "main": {
                "id": "main",
                "family": "deepseek",
                "api_key": api_key,
                "base_url": base_url
            }},
            "sets": [{ "name": "a", "profiles": [{
                "id": "p", "endpoint": "main", "model": "m", "max_context_window": 128000
            }]}]
        }))
        .unwrap()
    }

    #[test]
    fn merge_wire_null_keeps_live_key() {
        let merged = merge_wire(
            &live_config(),
            wire(serde_json::Value::Null, serde_json::Value::Null),
        )
        .unwrap();
        assert_eq!(merged.endpoints["main"].api_key, "sk-live-secret-key");
    }

    #[test]
    fn merge_wire_masked_echo_keeps_live_key() {
        // A GET→PUT round-trip of the masked form must not corrupt the key.
        let masked = mask_key("sk-live-secret-key");
        let merged = merge_wire(
            &live_config(),
            wire(serde_json::json!(masked), serde_json::Value::Null),
        )
        .unwrap();
        assert_eq!(merged.endpoints["main"].api_key, "sk-live-secret-key");
    }

    #[test]
    fn merge_wire_string_replaces_key() {
        let merged = merge_wire(
            &live_config(),
            wire(serde_json::json!("sk-new-key-123"), serde_json::Value::Null),
        )
        .unwrap();
        assert_eq!(merged.endpoints["main"].api_key, "sk-new-key-123");
    }

    #[test]
    fn merge_wire_new_endpoint_without_key_is_rejected() {
        let mut w = wire(serde_json::Value::Null, serde_json::Value::Null);
        w.endpoints.insert(
            "extra".into(),
            serde_json::from_value(serde_json::json!({
                "id": "extra", "family": "deepseek", "api_key": null, "base_url": null
            }))
            .unwrap(),
        );
        let err = merge_wire(&live_config(), w).unwrap_err();
        assert!(err.to_string().contains("'extra' is new"), "got: {err}");
    }

    #[test]
    fn merge_wire_empty_key_is_rejected() {
        let err = merge_wire(
            &live_config(),
            wire(serde_json::json!(""), serde_json::Value::Null),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "got: {err}");
    }

    #[test]
    fn merge_wire_null_base_url_keeps_live_url() {
        let merged = merge_wire(
            &live_config(),
            wire(serde_json::Value::Null, serde_json::Value::Null),
        )
        .unwrap();
        assert_eq!(
            merged.endpoints["main"].base_url.as_deref(),
            Some("https://live.example/v1")
        );
    }

    #[test]
    fn merge_wire_empty_base_url_clears_to_family_default() {
        let merged = merge_wire(
            &live_config(),
            wire(serde_json::Value::Null, serde_json::json!("")),
        )
        .unwrap();
        assert_eq!(merged.endpoints["main"].base_url, None);
    }

    #[test]
    fn merge_wire_base_url_string_replaces() {
        let merged = merge_wire(
            &live_config(),
            wire(
                serde_json::Value::Null,
                serde_json::json!("https://new.example/v1"),
            ),
        )
        .unwrap();
        assert_eq!(
            merged.endpoints["main"].base_url.as_deref(),
            Some("https://new.example/v1")
        );
    }

    #[test]
    fn merge_wire_map_key_mismatch_is_rejected() {
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "wrong": {
                "id": "main", "family": "deepseek", "api_key": null, "base_url": null
            }},
            "sets": []
        }))
        .unwrap();
        let err = merge_wire(&live_config(), w).unwrap_err();
        assert!(err.to_string().contains("does not match"), "got: {err}");
    }

    #[tokio::test]
    async fn apply_signals_live_agents() {
        let state = make_state();
        let agent = AgentId::random();
        let (entry, _rx) = make_entry_with_rx(None, format!("agent-{agent}"));
        state
            .registry
            .write()
            .await
            .register(agent.clone(), RegistryEntry::Live(entry));

        let _resp = apply_profiles(State(state.clone()), op_auth())
            .await
            .unwrap();
        // Check that pending_profile_reset was set.
        let reg = state.registry.read().await;
        let entry = reg.get(&agent).unwrap();
        let live = entry.as_live().unwrap();
        let cell = live.agent.pending_profile_reset.lock().unwrap();
        assert!(cell.is_some(), "pending_profile_reset should be set");
        // make_state's bundle resolves the recorded default set.
        assert_eq!(cell.as_ref().unwrap().set.name, "default");
    }

    #[tokio::test]
    async fn apply_rebinds_unbound_root_to_current_default() {
        // The behavior contract for a profile-less boot: the root's
        // record starts unbound, profiles get configured, and the next
        // apply both pushes the new set and writes the derived binding
        // back to the in-memory record — so the delivery gate admits
        // the root without a restart (restore re-derives the same way
        // on the next boot).
        let state = make_state();
        let root = AgentId::random();
        let (mut entry, _rx) = make_entry_with_rx(None, format!("agent-{root}"));
        entry.identity.config.profile_set = None;
        state
            .registry
            .write()
            .await
            .register(root.clone(), RegistryEntry::Live(entry));

        let resp = apply_profiles(State(state.clone()), op_auth())
            .await
            .expect("apply succeeds");
        assert_eq!(resp.applied, 1, "unbound root is applied, not skipped");
        let binding = {
            let reg = state.registry.read().await;
            reg.get(&root)
                .expect("root registered")
                .as_live()
                .expect("live")
                .identity
                .config
                .profile_set
                .clone()
        };
        assert_eq!(binding.as_deref(), Some("default"));
    }

    #[tokio::test]
    async fn apply_skips_faulted_agents() {
        let state = make_state();
        let agent = AgentId::random();
        state.registry.write().await.register(
            agent.clone(),
            RegistryEntry::Faulted(crate::test_helpers::make_faulted_entry(None, "test")),
        );

        let _ = apply_profiles(State(state), op_auth()).await.unwrap();
        // Should not panic; skipped count includes the faulted agent.
    }
    // A wholesale PUT that drops a set a live agent still records is
    // rejected with a structured 409 carrying the stranded bindings.
    #[tokio::test]
    async fn put_rejects_dangling_with_structured_list() {
        let state = make_state_two_sets();
        let _sub = alt_bound_sub(&state).await;
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "test": { "id": "test", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [{ "name": "default", "profiles": [{
                "id": "test", "endpoint": "test", "model": "test", "max_context_window": 128000
            }]}],
            "default": "default"
        }))
        .unwrap();
        let err = put_profiles(State(state), op_auth(), Json(w))
            .await
            .unwrap_err();
        assert_eq!(err.status, 409);
        let dangling = err.dangling.expect("structured dangling list present");
        assert!(
            dangling.iter().any(|s| s.contains("→ 'alt'")),
            "got: {dangling:?}"
        );
    }

    // With `force` the same save proceeds: the config persists to disk and
    // the bound agent stays registered (its binding is now dangling).
    #[tokio::test]
    #[serial_test::serial]
    async fn put_force_accepts_dangling_and_persists() {
        let state = make_state_two_sets();
        let sub = alt_bound_sub(&state).await;
        let w: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": { "test": { "id": "test", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [{ "name": "default", "profiles": [{
                "id": "test", "endpoint": "test", "model": "test", "max_context_window": 128000
            }]}],
            "default": "default",
            "force": true
        }))
        .unwrap();
        let Json(value) = put_profiles(State(state.clone()), op_auth(), Json(w))
            .await
            .unwrap();
        assert_eq!(value["default"], "default");
        // Persisted under the guard's slug-derived config root.
        let dir = std::env::var("XDG_CONFIG_HOME").unwrap();
        let body = std::fs::read_to_string(
            std::path::Path::new(&dir)
                .join("kallipai")
                .join("tagmata")
                .join("test")
                .join("profiles.toml"),
        )
        .unwrap();
        assert!(body.contains("[[sets.default.profiles]]"));
        // The bound agent is still registered, binding intact (now dangling).
        let registry = state.registry.read().await;
        assert!(registry.iter().any(|(id, _)| id == &sub));
    }

    // The error serializes `dangling` at the top level (the {"error":...}
    // envelope wrapper is an axum-response concern), and errors without it
    // omit the key entirely.
    #[test]
    fn conflict_dangling_envelope_round_trip() {
        let err = ApiError::conflict_dangling("stranded", vec!["a → 'alt'".into()]);
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["dangling"][0], "a → 'alt'");
        let plain = ApiError::bad_request("nope");
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("dangling").is_none());
    }
}
