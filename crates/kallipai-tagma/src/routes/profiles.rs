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
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};

use just_llm_client::types::generation::ReasoningEffort;
use kallipai_adk::profile::{
    Profile, ProfileConfig, ProfileRegistry, ProfileSet, Provider, is_valid_set_name,
    normalize_default,
};
use kallipai_common::protocol::{
    ApiError, DeleteSetResponse, Modality, SetDefaultRequest, SetReference,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::AuthIdentity;
use crate::probe::mask_key;
use crate::state::{ProfileBundle, SharedState};

use crate::profile_source::ProfileSource as _;
use crate::profile_source::ProfileSourceKind;

/// Guard for the mutating profile face under the gateway source:
/// local writes would be silently overwritten by the next refresh,
/// so they are refused with the disclosure instead. GET routes stay
/// open (the snapshot is served as-is). A source-mode switch bypasses
/// this guard by design: put_profiles dispatches on the intent before
/// reaching here, so the guard only ever sees edit requests.
/// Apply is not guarded: it pushes the in-memory registry to live
/// agents (a distribution step, not a write), so both sources serve it.
fn require_local_source(state: &SharedState) -> Result<(), ApiError> {
    if state.profile_source.load_full().source().kind() == ProfileSourceKind::Proxy {
        return Err(ApiError::conflict(
            "the tagma profile source is the model gateway (proxy mode): the in-memory snapshot mirrors the gateway's distribution face (the local profiles.toml is never written in this mode); manage profile sets on the gateway's management face",
        ));
    }
    Ok(())
}

/// The switch branch of PUT /profiles: change the profile source mode.
///
/// The switch never touches profiles.toml (it belongs to the local
/// source; the profiles payload of the switch request is ignored), and
/// it is atomic: every step that can fail runs before the first
/// mutation — the validating gateway fetch (or the local file read)
/// and the registry validation, and only then the settings writes
/// (mode, pin, selection), the source-slot swap, and the bundle swap.
/// The dangling-bindings gate runs here too: a switch that drops a
/// set a live agent still records is a 409 unless the PUT wire
/// carries force.
/// A failure leaves the mode,
/// the source, and the registry exactly as they were. Switches also
/// serialize: one process-wide lock holds each switch end to end, so
/// two racing requests cannot interleave their halves.
static SWITCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The polis-less gateway-switch 400, in two shapes: nothing is
/// enrolled anywhere (run the relay enrollment first), or the
/// persisted pin names a platform that no longer carries an
/// enrolled token (name it: which pin went stale is the
/// operator's next question).
fn unresolvable_gateway_400(
    platforms: &[crate::state::PlatformFace],
    pinned: Option<&str>,
) -> ApiError {
    match pinned {
        Some(pin) if platforms.iter().any(|face| face.token.is_some()) => {
            ApiError::bad_request(format!(
                "the pinned platform origin {pin:?} no longer resolves to an enrolled platform; pass polis to switch to another configured platform, or re-enroll that platform first"
            ))
        }
        _ => ApiError::bad_request(
            "switching to the model gateway requires an enrolled platform: run the relay enrollment on a platform (KALLIPAI_POLIS_URL / polis.toml entry) first, then switch",
        ),
    }
}
async fn switch_profile_source(
    state: &SharedState,
    request: SourceModeWire,
    force: bool,
) -> Result<serde_json::Value, ApiError> {
    let target = request.mode;
    let _switch_guard = SWITCH_LOCK.lock().await;
    let from = state.profile_source.load_full().source().kind();
    let (config, new_slot, active_params) = match target {
        ProfileSourceKind::Proxy => {
            // The target platform face: the request's polis names it
            // (the UI carries the serving platform's origin); without
            // one, the boot-resolved active face is the target. Both
            // legs fail with guidance, never with a silent fallback to
            // another platform.
            let platforms = state.platforms.load_full();
            let params = match request.polis.as_deref() {
                Some(polis) => {
                    let face = platforms
                        .iter()
                        .find(|f| f.origin == polis)
                        .ok_or_else(|| {
                            ApiError::bad_request(format!(
                                "unknown platform origin {polis:?}; the configured platforms are: {}",
                                platforms
                                    .iter()
                                    .map(|f| f.origin.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                        })?;
                    let token = face.token.clone().ok_or_else(|| {
                        ApiError::bad_request(format!(
                            "the tagma is not enrolled on {polis:?}: run the relay enrollment on that platform first (the entry {:?} stores no token), then switch",
                            face.name
                        ))
                    })?;
                    crate::profile_source::GatewayParams {
                        base: face.base.clone(),
                        origin: face.origin.clone(),
                        token,
                    }
                }
                None => {
                    // No explicit target: resolve like boot (the
                    // pinned platform, or the first enrolled entry
                    // before the first pin). The pin survives a
                    // local detour, so this leg works right after
                    // switching back.
                    let pinned = crate::settings::load_profile_source_polis().map_err(|e| {
                        ApiError::internal(format!(
                            "reading the persisted source polis failed: {e:#}"
                        ))
                    })?;
                    crate::gateway_params_from(&platforms, pinned.as_deref())
                        .ok_or_else(|| unresolvable_gateway_400(&platforms, pinned.as_deref()))?
                }
            };
            let candidate =
                crate::profile_source::ProxyProfileSource::new(&params.base, &params.token)
                    .map_err(|e| {
                        ApiError::bad_request(format!("gateway enrollment unusable: {e:#}"))
                    })?;
            // The request may point this tagma at another
            // collection. The write goes through to the gateway
            // first (the tagma pointer is the single store): a
            // write changes nothing on either side; a later gate
            // that rejects discloses the moved pointer.
            if let Some(selection) = request.collection.as_ref() {
                let written = candidate
                    .put_selection(selection.owner.as_deref(), &selection.collection)
                    .await
                    .map_err(|e| {
                        ApiError::bad_gateway(format!(
                            "writing the collection selection to the gateway failed; nothing was switched: {e:#}"
                        ))
                    })?;
                if !written {
                    return Err(ApiError::bad_request(format!(
                        "the requested collection {} is not visible to this account on the gateway; nothing was switched",
                        selection.collection
                    )));
                }
            }
            // The validating fetch: settings and live state change only
            // after the gateway proves reachable and the token proves
            // live. A dead token or an unreachable gateway fails here
            // with nothing switched.
            let config = candidate.load().await.map_err(|e| {
                let note = match request.collection.as_ref() {
                    Some(selection) => format!(
                        "gateway fetch failed; the gateway selection already names {}: re-PUT to retry, or point this tagma at another collection: {e:#}",
                        selection.collection
                    ),
                    None => format!("gateway fetch failed; nothing was switched: {e:#}"),
                };
                ApiError::bad_gateway(note)
            })?;
            // A selected collection that serves no set is a request
            // error, not a profile-less switch: the write already
            // named it, so the guidance points elsewhere.
            if let Some(selection) = request.collection.as_ref()
                && config.sets.is_empty()
            {
                return Err(ApiError::bad_request(format!(
                    "the requested collection {} matched no profile set on the gateway; the gateway selection already names it: point this tagma at another collection",
                    selection.collection
                )));
            }
            let new_source: std::sync::Arc<dyn crate::profile_source::ProfileSource> =
                std::sync::Arc::new(candidate);
            (config, new_source, Some(params))
        }
        ProfileSourceKind::Local => {
            let config = kallipai_adk::profile::load().map_err(|e| {
                ApiError::conflict(format!(
                    "local profiles.toml unreadable; nothing was switched: {e:#}"
                ))
            })?;
            let local_source: std::sync::Arc<dyn crate::profile_source::ProfileSource> =
                std::sync::Arc::new(crate::profile_source::LocalProfileSource::new());
            (config, local_source, None)
        }
    };

    // The same dangling-bindings gate the wholesale PUT runs: a switch
    // replaces the whole set universe, so a target that drops a set a
    // live agent still records strands that agent identically. The
    // PUT wire's force field is the confirmation channel here too.
    let stranded = dangling_bindings(state, &config).await;
    if !stranded.is_empty() && !force {
        // The proxy write-through may have moved the tagma
        // pointer already: the 409 says so, so the operator
        // knows the follow-up PUT completes it or re-aims it.
        let tail = match (request.collection.as_ref(), &target) {
            (Some(selection), ProfileSourceKind::Proxy) => format!(
                "; the gateway selection already names {}: re-PUT with force=true to complete the switch, or point this tagma at another collection",
                selection.collection
            ),
            _ => "; re-PUT with force=true to confirm the switch".to_owned(),
        };
        return Err(ApiError::conflict_dangling(
            format!(
                "switching to {} drops sets still bound by agents: {}{}",
                target.as_wire_str(),
                stranded.join(", "),
                tail
            ),
            stranded,
        ));
    }
    let registry = validate_config(&config)?;
    crate::settings::save_profile_source_mode(target)
        .map_err(|e| ApiError::internal(format!("persisting the source mode failed: {e:#}")))?;
    // The pin follows only an activation: model-gateway pins the
    // platform it switched to (the next boot and a later selection
    // update resolve the same face); local keeps the persisted pin:
    // mode and pin stay decoupled, so a restart or a later
    // polis-less gateway switch resolves the operator's recorded
    // platform, never a silent fallback to the first enrolled entry.
    if let Some(params) = active_params.as_ref() {
        crate::settings::save_profile_source_polis(Some(params.origin.as_str())).map_err(|e| {
            ApiError::internal(format!("persisting the source polis failed: {e:#}"))
        })?;
    }
    state
        .gateway_params
        .store(std::sync::Arc::new(active_params));
    state
        .profile_source
        .store(std::sync::Arc::new(crate::profile_source::SourceSlot::new(
            new_slot,
        )));
    let bundle = crate::state::ProfileBundle {
        config: config.clone(),
        registry,
    };
    state.profiles.store(std::sync::Arc::new(bundle));
    tracing::info!(
        from = from.as_wire_str(),
        to = target.as_wire_str(),
        "profile source switched"
    );
    // The response is the GET face (the same source block: mode,
    // switchability, the active platform, the enrollment list, and
    // the health fields) plus the switch note; the store adopts it
    // as the live config, so the page reads the new posture
    // without a refetch.
    let mut body = profiles_body(state).await?;
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "source_switch".into(),
            serde_json::json!({
                "from": from.as_wire_str(),
                "to": target.as_wire_str(),
                "note": "the profiles payload of the switch request was ignored; POST /profiles/apply pushes the new source to running agents",
            }),
        );
    }
    Ok(body)
}

/// The same-mode branch of a source-bearing PUT: point this tagma at
/// another collection on the gateway. Serialized with switches by
/// the same lock (it rebuilds the live source); the gateway write
/// happens first, so a failed write leaves both sides untouched.
/// The validating fetch gates the live swap; any rejection after a
/// successful write names the pointer it moved (force completes a
/// stranding one, the others need another collection). Local mode
/// has no gateway to write, so the selection update needs the
/// model-gateway source.
async fn update_profile_selection(
    state: &SharedState,
    target: &CollectionSelectionWire,
    force: bool,
) -> Result<serde_json::Value, ApiError> {
    let _switch_guard = SWITCH_LOCK.lock().await;
    if state.profile_source.load_full().source().kind() != ProfileSourceKind::Proxy {
        return Err(ApiError::bad_request(
            "the collection selection needs the model-gateway source; switch the source mode first",
        ));
    }
    // Rebuild from the ACTIVE face (the pin the last switch wrote):
    // the selection write goes through this source, and the fetch
    // takes the new selection live, not just the pointer.
    let params = state
        .gateway_params
        .load_full()
        .as_ref()
        .clone()
        .ok_or_else(|| {
            ApiError::internal("the gateway source is live without active platform parameters")
        })?;
    let candidate = crate::profile_source::ProxyProfileSource::new(&params.base, &params.token)
        .map_err(|e| ApiError::internal(format!("rebuilding the gateway source failed: {e:#}")))?;
    let written = candidate
        .put_selection(target.owner.as_deref(), &target.collection)
        .await
        .map_err(|e| {
            ApiError::bad_gateway(format!(
                "writing the collection selection to the gateway failed; the live source kept the previous snapshot: {e:#}"
            ))
        })?;
    if !written {
        return Err(ApiError::bad_request(format!(
            "the requested collection {} is not visible to this account on the gateway; the live source kept the previous selection",
            target.collection
        )));
    }
    let config = candidate.load().await.map_err(|e| {
        ApiError::bad_gateway(format!(
            "gateway fetch failed; the gateway selection already names {}: re-PUT to retry, or point this tagma at another collection; the live source kept the previous snapshot: {e:#}",
            target.collection,
        ))
    })?;
    // A collection that serves no set leaves the tagma with nothing
    // to hand agents: a request error, not a silent empty source.
    if config.sets.is_empty() {
        return Err(ApiError::bad_request(format!(
            "the requested collection {} matched no profile set on the gateway; the gateway selection already names it, and the live source kept the previous snapshot: point this tagma at another collection",
            target.collection
        )));
    }
    // The same dangling-bindings gate the wholesale PUT runs: the
    // narrowed selection can drop a set a live agent still records.
    let stranded = dangling_bindings(state, &config).await;
    if !stranded.is_empty() && !force {
        return Err(ApiError::conflict_dangling(
            format!(
                "the collection selection drops sets still bound by agents: {}; the gateway selection already names {}: re-PUT with force=true to complete it, or point this tagma at another collection",
                stranded.join(", "),
                target.collection
            ),
            stranded,
        ));
    }
    let registry = validate_config(&config)?;
    state
        .profile_source
        .store(std::sync::Arc::new(crate::profile_source::SourceSlot::new(
            std::sync::Arc::new(candidate),
        )));
    let bundle = crate::state::ProfileBundle {
        config: config.clone(),
        registry,
    };
    state.profiles.store(std::sync::Arc::new(bundle));
    tracing::info!(collection = %target.collection, "profile collection selection updated");
    profiles_body(state).await
}

/// GET /profiles — return the current profile configuration.
///
/// Operator-only: profiles contain API keys.
pub async fn get_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    Ok(Json(profiles_body(&state).await?))
}

/// POST /profiles/refresh — re-pull the live gateway snapshot and swap
/// the bundle in place. The explicit pull is how gateway-side edits (a
/// re-anchored default set, a newly published member) reach a running
/// tagma: the boot pull happens once, the degraded repull only fires
/// on an empty boot, and a healthy tagma would otherwise serve the
/// boot snapshot until a restart. A local source has no snapshot to
/// re-pull and is refused. The swap validates first and never
/// degrades the bundle on failure. Serialized with switches by
/// the same lock: the re-pull is a live-source rebuild, so a
/// concurrent switch or selection change cannot interleave with
/// the swap.
pub async fn refresh_profile_source(
    State(state): State<SharedState>,
    auth: AuthIdentity,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    let _switch_guard = SWITCH_LOCK.lock().await;
    let source = state.profile_source.load_full().source();
    if source.kind() != ProfileSourceKind::Proxy {
        return Err(ApiError::conflict(
            "the profile source is local; only a model-gateway source has a snapshot to re-pull",
        ));
    }
    let health = source.health().await;
    if health.poisoned {
        return Err(ApiError::conflict(
            "the enrollment token is dead; re-enroll the tagma, then restart or round-trip the source mode (the token is read once at boot)",
        ));
    }
    let config = source.load().await.map_err(|error| {
        ApiError::bad_gateway(format!(
            "gateway fetch failed; the live snapshot is unchanged: {error:#}"
        ))
    })?;
    // Defensive only: the switch lock already serializes the
    // swap-in points, so this cannot race a completed switch.
    let current = state.profile_source.load_full().source();
    if !Arc::ptr_eq(&current, &source) {
        return Err(ApiError::conflict(
            "a source switch replaced the gateway source mid-refresh; nothing was swapped",
        ));
    }
    let registry = validate_config(&config)?;
    state
        .profiles
        .store(Arc::new(ProfileBundle { config, registry }));
    let (applied, skipped) = broadcast_current_bundle(&state).await;
    tracing::info!(applied, skipped, "profile source refreshed");
    Ok(Json(profiles_body(&state).await?))
}

/// The GET /profiles response body, shared with the collection-update
/// branch of PUT /profiles (its response is the same face, so the UI
/// consumes one shape after either action).
async fn profiles_body(state: &SharedState) -> Result<serde_json::Value, ApiError> {
    let bundle = state.profiles.load();
    let mut body = masked_config(&bundle.config)?;
    // The source health block, additive on the config shape (the
    // web reads plain JSON members; local mode reports the defaults).
    let source = state.profile_source.load_full().source();
    let health = source.health().await;
    let mut source_block = serde_json::json!({ "mode": source.kind() });
    // proxy_available: the switchability signal — any configured
    // platform entry stores an enrollment token, whatever the current
    // mode (a local detour keeps the pin and the signal alike). The
    // UI hides the online face when it is false; the switch API
    // answers 400. polis names the active gateway face (null in
    // local mode), and platforms lists every configured entry's
    // enrollment state: the switch-target list.
    let active = state.gateway_params.load_full();
    let platforms = state.platforms.load_full();
    if let Some(obj) = source_block.as_object_mut() {
        obj.insert(
            "proxy_available".into(),
            serde_json::json!(platforms.iter().any(|face| face.token.is_some())),
        );
        obj.insert(
            "polis".into(),
            serde_json::json!((*active).clone().map(|params| params.origin)),
        );
        obj.insert(
            "platforms".into(),
            serde_json::json!(
                platforms
                    .iter()
                    .map(|face| serde_json::json!({
                        "name": face.name,
                        "origin": face.origin,
                        "enrolled": face.token.is_some(),
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    // Under the gateway source the operator still needs to see what
    // switching back to local would face: serve the on-disk file as a
    // masked, read-only preview. Omitted entirely in local mode (the
    // live config already is that file).
    if source.kind() == ProfileSourceKind::Proxy {
        let preview = match kallipai_adk::profile::load() {
            Ok(local) => match masked_config(&local) {
                Ok(masked) => masked,
                Err(e) => serde_json::json!({ "unreadable": format!("{e}") }),
            },
            Err(e) => serde_json::json!({ "absent": true, "error": format!("{e:#}") }),
        };
        body.as_object_mut()
            .map(|obj| obj.insert("local_disk".into(), preview));
    }
    let health_json = serde_json::to_value(&health)
        .map_err(|e| ApiError::internal(format!("source health serialization failed: {e}")))?;
    if let (Some(mode_obj), Some(health_obj)) =
        (source_block.as_object_mut(), health_json.as_object())
    {
        for (key, value) in health_obj {
            mode_obj.insert(key.clone(), value.clone());
        }
    }
    body.as_object_mut()
        .map(|obj| obj.insert("source".into(), source_block));
    Ok(body)
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
    #[serde(default = "kallipai_adk::profile::Profile::default_modalities")]
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
    /// true, `put_profiles` (and its switch and collection-selection
    /// branches) skips the dangling-bindings gate.
    #[serde(default)]
    force: bool,
    /// Source-mode switch intent. When the requested mode differs from the
    /// live source, this PUT takes the switch branch: the profiles
    /// payload is ignored (the switch never touches profiles.toml), the
    /// mode is validated and persisted to settings.toml, and the source
    /// and registry hot-swap. See `switch_profile_source`.
    #[serde(default)]
    source: Option<SourceModeWire>,
}
/// The `source` member of a switch-bearing PUT body.
#[derive(Deserialize)]
struct SourceModeWire {
    mode: ProfileSourceKind,
    /// The platform origin to activate (model-gateway switches): the
    /// serving platform's origin. Absent re-resolves like boot: the
    /// pinned platform, or the first enrolled entry before the first
    /// pin. Ignored by local switches.
    #[serde(default)]
    polis: Option<String>,
    /// The collection to point this tagma at (absent keeps the
    /// selected one): the switch writes it through to the gateway,
    /// and a same-mode PUT carrying it is the selection update. A
    /// switch to local ignores it (no gateway pointer to write).
    #[serde(default)]
    collection: Option<CollectionSelectionWire>,
}

/// One collection of the account's visibility domain: an absent
/// owner addresses the account's own space; an explicit owner a
/// shared or platform collection.
#[derive(Debug, Deserialize)]
struct CollectionSelectionWire {
    #[serde(default)]
    owner: Option<String>,
    collection: String,
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
            kallipai_adk::profile::require_text_member("set", p)
                .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
        }
    }
    for p in &parking {
        kallipai_adk::profile::require_text_member("parking", p)
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
/// registry and writes a [`ProfileReset`](kallipai_adk::ProfileReset) to the agent's pending-reset
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

    let (applied, skipped) = broadcast_current_bundle(&state).await;
    info!(applied, skipped, "profile apply signaled to live agents");
    Ok(Json(ApplyResponse { applied, skipped }))
}

/// Push the current profile bundle to every live agent: resolve each
/// recorded set through the bundle's default-set fallback, write the
/// in-memory rebinds, and signal a pending reset. The shared tail of
/// POST /profiles/apply and the degraded-repull recovery (a repull that
/// lands a fresh bundle broadcasts it, so bound agents re-resolve
/// without a restart).
async fn broadcast_current_bundle(state: &SharedState) -> (usize, usize) {
    let bundle = state.profiles.load();
    let registry = bundle.registry.clone();

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
                    kallipai_adk::ProfileReset {
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

    let mut applied = 0usize;
    for (reset, cell_lock, notify) in targets {
        signal_profile_reset(reset, &cell_lock, &notify);
        applied += 1;
    }
    (applied, skipped)
}

// -- degraded-boot repull ------------------------------------------------

/// First retry delay after a degraded boot, doubling per failure.
const REPULL_RETRY_BASE: Duration = Duration::from_secs(30);
/// Backoff ceiling: a down gateway is retried at most every 5 minutes.
const REPULL_RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// One round of the degraded-repull task, shared by the task loop and
/// the tests. Reads the live source through the slot, so a runtime
/// switch between rounds redirects the next attempt.
#[derive(Debug)]
enum RepullOutcome {
    /// The live source is not the gateway: nothing to repull.
    NotProxy,
    /// The gateway source carries no degraded flag: nothing to do.
    Healthy,
    /// The enrollment token is confirmed dead: retrying cannot help
    /// (the token is read once at boot); recovery is re-enroll plus
    /// restart. The task idles at the backoff ceiling.
    Poisoned,
    /// The fetch still fails, or the result was dropped because a
    /// switch replaced the source mid-fetch; retry later.
    Retrying(String),
    /// A fresh config landed: the bundle was rebuilt, stored, and
    /// broadcast to live agents.
    Recovered { applied: usize, skipped: usize },
}

/// Value-level config equality. ProfileConfig derives no PartialEq; the
/// repull gate only asks "is the live bundle the source's current
/// snapshot", so a JSON round-trip comparison is exact and cheap at the
/// task's cadence.
fn config_matches(live: &ProfileConfig, snapshot: &ProfileConfig) -> bool {
    serde_json::to_value(live).ok() == serde_json::to_value(snapshot).ok()
}

/// One repull attempt against the live gateway source. The slot is
/// loaded per attempt, and a source swapped mid-fetch means the result
/// belongs to a source no longer live — it is dropped instead of
/// stored (the switch that replaced it owns the state from there).
async fn repull_once(state: &SharedState) -> RepullOutcome {
    let source = state.profile_source.load_full().source();
    if source.kind() != ProfileSourceKind::Proxy {
        return RepullOutcome::NotProxy;
    }
    let health = source.health().await;
    if health.poisoned {
        return RepullOutcome::Poisoned;
    }
    // The gate stays armed until the live bundle reflects the source's
    // latest known-good snapshot. The degraded flag alone is not
    // enough: a fetch that succeeds with a config that cannot build
    // (an unknown family, say) clears the flag while the bundle
    // stays stale — without the snapshot comparison the task would
    // idle on an empty bundle after one retry.
    let snapshot = source.snapshot().await;
    if let Some(snapshot) = snapshot.as_ref() {
        if config_matches(&state.profiles.load().config, snapshot) {
            return RepullOutcome::Healthy;
        }
    } else if !health.degraded {
        return RepullOutcome::Healthy;
    }
    match source.load().await {
        Ok(config) => {
            let current = state.profile_source.load_full().source();
            if !Arc::ptr_eq(&current, &source) {
                return RepullOutcome::Retrying(
                    "a source switch replaced the gateway source mid-fetch; result dropped"
                        .to_owned(),
                );
            }
            let registry = match validate_config(&config) {
                Ok(registry) => registry,
                Err(error) => {
                    return RepullOutcome::Retrying(format!(
                        "the refreshed gateway config does not build: {}",
                        error.message
                    ));
                }
            };
            state
                .profiles
                .store(Arc::new(ProfileBundle { config, registry }));
            let (applied, skipped) = broadcast_current_bundle(state).await;
            RepullOutcome::Recovered { applied, skipped }
        }
        Err(error) => RepullOutcome::Retrying(format!("{error:#}")),
    }
}

/// The boot task for a degraded model-gateway source: while the live
/// source is the gateway and its health flags `degraded` (the boot
/// fetched nothing), retry the fetch on a doubling backoff; the first
/// success rebuilds the profile bundle and broadcasts the reset signal
/// to live agents, so a gateway that comes back is picked up without a
/// restart. A poisoned source stops retrying (a dead enrollment token
/// cannot recover in-process); a later switch re-arms the loop through
/// the per-round slot load.
pub(crate) fn spawn_degraded_repull_task(state: SharedState) {
    tokio::spawn(async move {
        let mut backoff = REPULL_RETRY_BASE;
        let mut poison_noted = false;
        loop {
            tokio::time::sleep(backoff).await;
            match repull_once(&state).await {
                RepullOutcome::NotProxy | RepullOutcome::Healthy => {
                    backoff = REPULL_RETRY_BASE;
                    poison_noted = false;
                }
                RepullOutcome::Poisoned => {
                    if !poison_noted {
                        warn!(
                            "degraded repull paused: the gateway enrollment token is dead; re-enroll the tagma on the platform and restart"
                        );
                        poison_noted = true;
                    }
                    backoff = REPULL_RETRY_MAX;
                }
                RepullOutcome::Retrying(reason) => {
                    backoff = (backoff * 2).min(REPULL_RETRY_MAX);
                    warn!(
                        reason,
                        backoff_secs = backoff.as_secs(),
                        "degraded repull failed; retrying"
                    );
                }
                RepullOutcome::Recovered { applied, skipped } => {
                    info!(
                        applied,
                        skipped, "degraded repull recovered: profile bundle rebuilt and broadcast"
                    );
                    backoff = REPULL_RETRY_BASE;
                    poison_noted = false;
                }
            }
        }
    });
}

/// Refuse memberless sets, then build backends and a trial registry for the
/// candidate config; nothing changes on failure. Shared by every set-
/// management mutation. The refusal is this face's call: the user is
/// present, so the actionable 400 beats a silent partial boot — while the
/// boot path degrades instead (skips serving the set; agents bound to it
/// fail at resolution, see `kallipai_adk::profile::ProfileRegistry`).
fn validate_config(config: &ProfileConfig) -> Result<Arc<ProfileRegistry>, ApiError> {
    for (name, set) in &config.sets {
        if set.profiles.is_empty() {
            return Err(ApiError::bad_request(format!(
                "set '{name}' has no profiles; remove the set or add a member",
            )));
        }
    }
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
    match kallipai_adk::profile::config_path() {
        Ok(path) => {
            if let Err(e) = kallipai_adk::profile::save(config, &path) {
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
    reset: kallipai_adk::ProfileReset,
    cell: &std::sync::Mutex<Option<kallipai_adk::ProfileReset>>,
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
    use kallipai_common::agentid::AgentId;

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
        // The additive source health block (local mode = defaults).
        assert_eq!(value["source"]["mode"], "local");
        assert_eq!(value["source"]["poisoned"], false);
        assert_eq!(value["source"]["token_state"], "ok");
        assert_eq!(value["source"]["refresh_failure_count"], 0);
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

    #[tokio::test]
    async fn mutating_profile_routes_are_409_under_proxy_source() {
        let source = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::profile_source::SourceSlot::new(std::sync::Arc::new(
                crate::profile_source::ProxyProfileSource::new(
                    "http://gw.test:7501",
                    "tagma-token-1",
                )
                .unwrap(),
            )),
        ));
        let state = std::sync::Arc::new(crate::state::AppState::with_limits(
            kallipai_common::authtoken::TokenHash::of("op-token"),
            1,
            1,
            5,
            crate::test_helpers::make_profile_bundle(),
            kallipai_common::policy::PolicyPreset::Default,
            kallipai_adk::usage_stats::UsageStats::default(),
            kallipai_adk::token_budget::TokenBudget::unlimited(),
            None,
            source,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                None::<crate::profile_source::GatewayParams>,
            )),
            Vec::new(),
            None,
        ));
        let response = set_default_profile_set(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(SetDefaultRequest {
                default: "whatever".to_owned(),
            }),
        )
        .await;
        let err = response.unwrap_err();
        assert_eq!(err.status, 409);
        // The disclosure: the local file is never written under a proxy source.
        assert!(
            err.message.contains("never written"),
            "got: {}",
            err.message
        );
    }

    // -- POST /profiles/refresh ----------------------------------------------

    /// A swappable stand-in gateway: /selected-collection answers the
    /// current cell, so a test serves one shape at boot and another
    /// after gateway-side edits.
    async fn spawn_swappable_gateway(
        answer: std::sync::Arc<std::sync::RwLock<serde_json::Value>>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route(
                "/selected-collection",
                axum::routing::get(move || {
                    let answer = answer.clone();
                    async move { axum::Json(answer.read().unwrap().clone()) }
                }),
            )
            .route(
                "/parking",
                axum::routing::get(|| async { axum::Json(serde_json::json!({ "profiles": [] })) }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// The baseline shape: two member sets and (when asked) an
    /// anchored default — the operator regression contract the
    /// distribution face and this refresh test share.
    fn baseline_snapshot(default: Option<&str>) -> serde_json::Value {
        let set = |name: &str, model: &str| {
            serde_json::json!({
                "name": name,
                "description": "the baseline fixture set",
                "profiles": [
                    { "family": "openai-compatible",
                      "base_url": "http://gw.test:7501/v1",
                      "model": model, "max_context_window": 128000,
                      "effort": null, "store": null, "modalities": [],
                      "api_key": "tagma-token-1" }
                ]
            })
        };
        serde_json::json!({
            "owner": "system",
            "collection": "baseline",
            "description": "the platform default audience",
            "sets": [set("alpha", "baseline-main"), set("beta", "baseline-fast")],
            "default_set": default,
        })
    }

    #[tokio::test]
    async fn refresh_under_local_source_is_refused() {
        let state = make_state_with_source(std::sync::Arc::new(
            crate::profile_source::LocalProfileSource,
        ));
        let err = refresh_profile_source(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 409);
        assert!(err.message.contains("local"), "got: {}", err.message);
    }

    /// A poisoned source answers the refresh with the dead-token 409:
    /// the retry cannot succeed until the tagma re-enrolls and the
    /// token is read again (restart, or a source-mode round-trip).
    #[tokio::test]
    async fn refresh_under_a_poisoned_source_is_refused() {
        let state = make_state_with_source(std::sync::Arc::new(StubGatewaySource {
            flags: std::sync::Arc::new(std::sync::Mutex::new(StubGatewayFlags {
                poisoned: true,
                ..Default::default()
            })),
        }));
        let err = refresh_profile_source(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 409);
        assert!(
            err.message.contains("dead") && err.message.contains("restart"),
            "got: {}",
            err.message
        );
    }

    /// The operator regression: a re-anchored baseline (multiple
    /// members, an anchored default) reaches a running tagma through
    /// the explicit refresh, no restart involved.
    #[tokio::test]
    async fn refresh_lands_the_anchored_default_without_a_restart() {
        crate::test_helpers::ensure_test_data_dir();
        let answer = std::sync::Arc::new(std::sync::RwLock::new(baseline_snapshot(Some("alpha"))));
        let base = spawn_swappable_gateway(answer).await;
        let state = make_state_with_source(std::sync::Arc::new(
            crate::profile_source::ProxyProfileSource::new(&base, "tagma-token-1").unwrap(),
        ));
        let response = refresh_profile_source(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
        )
        .await
        .unwrap();
        let body = response.0;
        assert_eq!(body["default"], "alpha");
        assert_eq!(body["source"]["mode"], "model-gateway");
        // The bundle behind the response carries the same landing.
        let bundle = state.profiles.load_full();
        assert_eq!(bundle.config.default, "alpha");
        let names: Vec<&str> = bundle.config.sets.keys().map(|s| s.as_str()).collect();
        assert!(names.contains(&"alpha") && names.contains(&"beta"));
    }

    #[tokio::test]
    async fn refresh_fetch_failure_leaves_the_bundle_alone() {
        crate::test_helpers::ensure_test_data_dir();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/selected-collection",
            axum::routing::get(|| async {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "down")
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let state = make_state_with_source(std::sync::Arc::new(
            crate::profile_source::ProxyProfileSource::new(
                &format!("http://{addr}"),
                "tagma-token-1",
            )
            .unwrap(),
        ));
        let before = state.profiles.load_full();
        let err = refresh_profile_source(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 502);
        assert!(err.message.contains("unchanged"), "got: {}", err.message);
        assert!(std::sync::Arc::ptr_eq(&before, &state.profiles.load_full()));
    }
    // -- source switching (PUT /profiles source.mode) -----------------------

    fn make_proxy_state(
        gateway_params: Option<crate::profile_source::GatewayParams>,
    ) -> crate::state::SharedState {
        let source = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::profile_source::SourceSlot::new(std::sync::Arc::new(
                crate::profile_source::ProxyProfileSource::new(
                    "http://127.0.0.1:1",
                    "tagma-token-1",
                )
                .unwrap(),
            )),
        ));
        std::sync::Arc::new(crate::state::AppState::with_limits(
            kallipai_common::authtoken::TokenHash::of("op-token"),
            1,
            1,
            5,
            crate::test_helpers::make_profile_bundle(),
            kallipai_common::policy::PolicyPreset::Default,
            kallipai_adk::usage_stats::UsageStats::default(),
            kallipai_adk::token_budget::TokenBudget::unlimited(),
            None,
            source,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(gateway_params)),
            Vec::new(),
            None,
        ))
    }

    /// Like [`make_proxy_state`], but over a caller-chosen source (the
    /// repull tests drive a controllable stand-in).
    fn make_state_with_source(
        source: std::sync::Arc<dyn crate::profile_source::ProfileSource>,
    ) -> crate::state::SharedState {
        std::sync::Arc::new(crate::state::AppState::with_limits(
            kallipai_common::authtoken::TokenHash::of("op-token"),
            1,
            1,
            5,
            crate::test_helpers::make_profile_bundle(),
            kallipai_common::policy::PolicyPreset::Default,
            kallipai_adk::usage_stats::UsageStats::default(),
            kallipai_adk::token_budget::TokenBudget::unlimited(),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::profile_source::SourceSlot::new(source),
            )),
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                None::<crate::profile_source::GatewayParams>,
            )),
            Vec::new(),
            None,
        ))
    }

    /// A controllable stand-in for the gateway source: the health flags
    /// and the load outcome are set by the test, so the repull gating
    /// and recovery run without a live HTTP face (the HTTP behaviors of
    /// the degraded boot are covered in profile_source::proxy's tests).
    #[derive(Default)]
    struct StubGatewayFlags {
        poisoned: bool,
        degraded: bool,
        fail_load: bool,
        /// What a successful load returns and snapshot reports: the
        /// default shape is the recoverable config; a test can hold an
        /// unbuildable one here (the N-1 stall shape).
        loaded: Option<kallipai_adk::profile::ProfileConfig>,
    }

    struct StubGatewaySource {
        flags: std::sync::Arc<std::sync::Mutex<StubGatewayFlags>>,
    }

    /// The snapshot the stub's successful load returns: one set named
    /// `recovered` (distinct from make_profile_bundle's `default`, so a
    /// refilled bundle is distinguishable from the initial one).
    fn recovered_config() -> kallipai_adk::profile::ProfileConfig {
        use just_llm_client::family;
        use kallipai_adk::profile::{Profile, ProfileConfig, ProfileSet, Provider};
        use std::collections::{BTreeMap, HashMap};
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "test".into(),
            Provider {
                id: "test".into(),
                family: family::DEEPSEEK.into(),
                api_key: "test".into(),
                base_url: None,
            },
        );
        ProfileConfig {
            sets: BTreeMap::from([(
                "recovered".to_string(),
                ProfileSet {
                    name: "recovered".into(),
                    description: None,
                    profiles: vec![Profile {
                        id: "test".into(),
                        endpoint: "test".into(),
                        model: "test".into(),
                        max_context_window: 128_000,
                        store: None,
                        effort: None,
                        modalities: Profile::default_modalities(),
                    }],
                },
            )]),
            default: "recovered".into(),
            endpoints,
            parking: vec![],
        }
    }

    #[async_trait::async_trait]
    impl crate::profile_source::ProfileSource for StubGatewaySource {
        fn kind(&self) -> crate::profile_source::ProfileSourceKind {
            crate::profile_source::ProfileSourceKind::Proxy
        }

        async fn load(&self) -> anyhow::Result<kallipai_adk::profile::ProfileConfig> {
            let mut flags = self.flags.lock().unwrap();
            if flags.fail_load {
                return Err(anyhow::anyhow!("gateway unreachable"));
            }
            flags.degraded = false;
            Ok(flags.loaded.clone().unwrap_or_else(recovered_config))
        }

        async fn snapshot(&self) -> Option<kallipai_adk::profile::ProfileConfig> {
            self.flags.lock().unwrap().loaded.clone()
        }

        async fn health(&self) -> crate::profile_source::SourceHealth {
            let flags = self.flags.lock().unwrap();
            crate::profile_source::SourceHealth {
                poisoned: flags.poisoned,
                degraded: flags.degraded,
                ..Default::default()
            }
        }
    }

    /// The repull lifecycle: a degraded source whose fetch still fails
    /// reports Retrying; once the fetch succeeds, the bundle is rebuilt
    /// and stored, the live agent receives the reset signal carrying
    /// the new bundle's set, and the in-memory rebind lands.
    #[tokio::test]
    async fn degraded_repull_recovers_the_bundle_and_broadcasts() {
        let flags = std::sync::Arc::new(std::sync::Mutex::new(StubGatewayFlags {
            degraded: true,
            fail_load: true,
            ..Default::default()
        }));
        let state = make_state_with_source(std::sync::Arc::new(StubGatewaySource {
            flags: flags.clone(),
        }));
        let agent = AgentId::random();
        let (mut entry, _rx) = make_entry_with_rx(None, format!("agent-{agent}"));
        // The helper hardcodes a recorded binding; clear it so the
        // recovery exercises the unbound-root rebind path.
        entry.identity.config.profile_set = None;
        state
            .registry
            .write()
            .await
            .register(agent.clone(), RegistryEntry::Live(entry));

        // Still down: the round reports Retrying and nothing is stored.
        assert!(matches!(
            repull_once(&state).await,
            RepullOutcome::Retrying(_)
        ));
        assert_eq!(
            state.profiles.load().config.sets.len(),
            1,
            "the initial bundle (make_profile_bundle) is untouched"
        );

        // The gateway answers: the round recovers, the bundle is refilled
        // with the fetched config, and the live agent is signaled and
        // rebound onto the recovered default set.
        {
            let mut flags = flags.lock().unwrap();
            flags.fail_load = false;
            flags.loaded = Some(recovered_config());
        }
        match repull_once(&state).await {
            RepullOutcome::Recovered { applied, skipped } => {
                assert_eq!(applied, 1);
                assert_eq!(skipped, 0);
            }
            other => panic!("expected recovery, got {other:?}"),
        }
        // Third round: the bundle mirrors the source's snapshot now, so
        // the gate disarms through the equal-snapshot branch.
        assert!(matches!(repull_once(&state).await, RepullOutcome::Healthy));
        let bundle = state.profiles.load();
        assert!(bundle.config.sets.contains_key("recovered"));
        let reg = state.registry.read().await;
        let entry = reg.get(&agent).unwrap();
        let live = entry.as_live().unwrap();
        assert_eq!(
            live.identity.config.profile_set.as_deref(),
            Some("recovered"),
            "the broadcast rebound the unbound root"
        );
        let cell = live.agent.pending_profile_reset.lock().unwrap();
        let reset = cell.as_ref().unwrap();
        assert_eq!(reset.set.name, "recovered");
        assert!(reset.registry.sets().contains_key("recovered"));
    }

    /// The gate arms only for a degraded, unpoisoned gateway source: a
    /// local source, a healthy gateway source, and a poisoned one all
    /// skip the fetch.
    #[tokio::test]
    async fn repull_gates_on_kind_degraded_and_poison() {
        // Local source: not the gateway's business.
        let local = make_state();
        assert!(matches!(repull_once(&local).await, RepullOutcome::NotProxy));
        // Healthy gateway source (no degraded flag): nothing to do.
        let healthy = make_state_with_source(std::sync::Arc::new(StubGatewaySource {
            flags: std::sync::Arc::new(std::sync::Mutex::new(StubGatewayFlags::default())),
        }));
        assert!(matches!(
            repull_once(&healthy).await,
            RepullOutcome::Healthy
        ));
        // Poisoned: the fetch is skipped; recovery is re-enroll plus restart.
        let poisoned = make_state_with_source(std::sync::Arc::new(StubGatewaySource {
            flags: std::sync::Arc::new(std::sync::Mutex::new(StubGatewayFlags {
                poisoned: true,
                degraded: true,
                ..Default::default()
            })),
        }));
        assert!(matches!(
            repull_once(&poisoned).await,
            RepullOutcome::Poisoned
        ));
    }

    /// The N-1 stall shape: a fetch that succeeds with a config that
    /// cannot build (an unknown family) clears the degraded flag, but
    /// the snapshot-vs-bundle comparison keeps the task armed — the
    /// next round retries instead of idling on the empty bundle.
    #[tokio::test]
    async fn repull_stays_armed_when_recovery_cannot_build() {
        let state = make_state_with_source(std::sync::Arc::new(StubGatewaySource {
            flags: std::sync::Arc::new(std::sync::Mutex::new(StubGatewayFlags {
                degraded: true,
                loaded: Some(unbuildable_config()),
                ..Default::default()
            })),
        }));
        // Round 1: the fetch succeeds but the config cannot build into
        // a registry, so the round reports Retrying...
        assert!(matches!(
            repull_once(&state).await,
            RepullOutcome::Retrying(_)
        ));
        // ...and although the degraded flag cleared with that load, the
        // live bundle is still not the source's snapshot: the next round
        // keeps retrying rather than reporting Healthy.
        assert!(matches!(
            repull_once(&state).await,
            RepullOutcome::Retrying(_)
        ));
        assert_eq!(
            state.profiles.load().config.sets.len(),
            1,
            "the initial bundle is still the live one"
        );
    }

    /// A config whose provider family no backend knows: a fetch can
    /// carry it, validate_config refuses it.
    fn unbuildable_config() -> kallipai_adk::profile::ProfileConfig {
        use kallipai_adk::profile::{Profile, ProfileConfig, ProfileSet, Provider};
        use std::collections::{BTreeMap, HashMap};
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "bad".into(),
            Provider {
                id: "bad".into(),
                family: "no-such-family".into(),
                api_key: "k".into(),
                base_url: None,
            },
        );
        ProfileConfig {
            sets: BTreeMap::from([(
                "recovered".to_string(),
                ProfileSet {
                    name: "recovered".into(),
                    description: None,
                    profiles: vec![Profile {
                        id: "bad".into(),
                        endpoint: "bad".into(),
                        model: "m".into(),
                        max_context_window: 1_000,
                        store: None,
                        effort: None,
                        modalities: Profile::default_modalities(),
                    }],
                },
            )]),
            default: "recovered".into(),
            endpoints,
            parking: vec![],
        }
    }

    fn switch_wire(mode: &str) -> ProfileConfigWire {
        serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": mode },
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn switch_to_model_gateway_without_boot_params_is_a_400() {
        // No derived gateway connection parameters, no switch to
        // model-gateway: the constraint is enforced server-side,
        // not by UI convention.
        let state = crate::test_helpers::make_state();
        assert!(state.gateway_params.load_full().is_none()); // the local default: no enrolled platform
        let err = put_profiles(State(state), op_auth(), Json(switch_wire("model-gateway")))
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("enrolled platform"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn switch_fetch_failure_switches_nothing() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        // Local-mode state (the make_state default) with an enrolled
        // platform face pointing at a dead port: a polis-less switch
        // resolves it like boot (the first enrolled entry), the
        // validating fetch fails, and the mode, the source, and the
        // registry stay untouched.
        let state = {
            let params = crate::profile_source::GatewayParams {
                base: "http://127.0.0.1:1/v1/model-gateway".to_owned(),
                origin: "http://127.0.0.1:1".to_owned(),
                token: "tagma-token-1".to_owned(),
            };
            // The helper builds without gateway params; unwrap the sole
            // reference and set them (local source stays).
            let mut inner = match std::sync::Arc::try_unwrap(crate::test_helpers::make_state()) {
                Ok(inner) => inner,
                Err(_) => panic!("make_state hands out the sole reference"),
            };
            inner.gateway_params =
                std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(Some(params)));
            inner.platforms = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(vec![face(
                "http://127.0.0.1:1",
                Some("tagma-token-1"),
            )]));
            std::sync::Arc::new(inner)
        };
        let err = put_profiles(State(state), op_auth(), Json(switch_wire("model-gateway")))
            .await
            .unwrap_err();
        assert_eq!(err.status, 502);
        assert!(
            err.message.contains("nothing was switched"),
            "got: {}",
            err.message
        );
        // Atomicity: the settings file was never written.
        let mode = crate::settings::load_profile_source_mode().unwrap();
        assert_eq!(mode, crate::profile_source::ProfileSourceKind::Local);
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    /// A mock distribution face on a loopback port: the selection write
    /// lands (PUT /selection is the single store) and the one-read face
    /// serves the selected collection's members. The account's own
    /// user-col holds both sets; beta-col holds beta alone; empty-col
    /// selects fine but serves no set; ghost-col is not visible to the
    /// account (the write 404s).
    async fn spawn_switch_gateway() -> String {
        use axum::response::IntoResponse;
        use axum::routing::{get, put};
        use std::sync::Mutex;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let selected: std::sync::Arc<Mutex<Option<String>>> = std::sync::Arc::new(Mutex::new(None));
        let sel_read = selected.clone();
        let sel_write = selected.clone();
        let app = axum::Router::new()
            .route(
                "/v1/model-gateway/selected-collection",
                get(move || {
                    let sel = sel_read.clone();
                    async move {
                        let sel = sel.lock().unwrap().clone();
                        match sel.as_deref() {
                            Some("beta-col") => axum::Json(serde_json::json!({
                                "owner": "acc-x", "collection": "beta-col",
                                "description": "the shared narrow collection",
                                "sets": [
                                    { "name": "beta", "description": "second",
                                      "profiles": [
                                        { "family": "deepseek", "base_url": "http://gw.test:7501/v1",
                                          "model": "ds-x", "max_context_window": 64000,
                                          "effort": null, "store": null, "modalities": [],
                                          "api_key": "tagma-token-1" }
                                      ] }
                                ],
                                "default_set": "beta"
                            })).into_response(),
                            Some("empty-col") => axum::Json(serde_json::json!({
                                "owner": "acc-x", "collection": "empty-col",
                                "description": "a collection with no sets yet",
                                "sets": [],
                                "default_set": null
                            })).into_response(),
                            Some("hollow-col") => axum::Json(serde_json::json!({
                                "owner": "acc-x", "collection": "hollow-col",
                                "description": "a collection whose set lost every member",
                                "sets": [
                                    { "name": "hollow", "description": "memberless",
                                      "profiles": [] }
                                ],
                                "default_set": "hollow"
                            })).into_response(),
                            _ => axum::Json(serde_json::json!({
                                "owner": "acc-x", "collection": "user-col",
                                "description": "the account's own collection",
                                "sets": [
                                    { "name": "alpha", "description": "primary",
                                      "profiles": [
                                        { "family": "openai-compatible", "base_url": "http://gw.test:7501/v1",
                                          "model": "gpt-x", "max_context_window": 128000,
                                          "effort": null, "store": null, "modalities": [],
                                          "api_key": "tagma-token-1" }
                                      ] },
                                    { "name": "beta", "description": "second",
                                      "profiles": [
                                        { "family": "deepseek", "base_url": "http://gw.test:7501/v1",
                                          "model": "ds-x", "max_context_window": 64000,
                                          "effort": null, "store": null, "modalities": [],
                                          "api_key": "tagma-token-1" }
                                      ] }
                                ],
                                "default_set": "alpha"
                            })).into_response(),
                        }
                    }
                }),
            )
            .route(
                "/v1/model-gateway/selection",
                put(move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let sel = sel_write.clone();
                    async move {
                        let asked = body
                            .get("collection")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned();
                        let owner_ok = body
                            .get("owner")
                            .is_none_or(|v| v.is_null() || v == "acc-x");
                        if !owner_ok || asked == "ghost-col" {
                            return (
                                axum::http::StatusCode::NOT_FOUND,
                                axum::Json(serde_json::json!({
                                    "error": { "message": "no such collection" }
                                })),
                            )
                                .into_response();
                        }
                        *sel.lock().unwrap() = Some(asked.clone());
                        axum::Json(serde_json::json!({
                            "owner": "acc-x",
                            "collection": asked
                        }))
                        .into_response()
                    }
                }),
            )
            .route(
                "/v1/model-gateway/parking",
                get(|| async { axum::Json(serde_json::json!({ "profiles": [] })) }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// A local-mode state carrying the configured platform faces (the
    /// switch-target list the polis leg resolves against).
    fn local_state_with_platforms(
        faces: Vec<crate::state::PlatformFace>,
    ) -> crate::state::SharedState {
        let mut inner = match std::sync::Arc::try_unwrap(crate::test_helpers::make_state()) {
            Ok(inner) => inner,
            Err(_) => panic!("make_state hands out the sole reference"),
        };
        inner.platforms = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(faces));
        std::sync::Arc::new(inner)
    }

    fn face(origin: &str, token: Option<&str>) -> crate::state::PlatformFace {
        crate::state::PlatformFace {
            name: "alpha".to_owned(),
            origin: origin.to_owned(),
            base: format!("{origin}/v1/model-gateway"),
            token: token.map(|t| t.to_owned()),
        }
    }

    #[tokio::test]
    async fn an_enrollment_refresh_flips_the_face_without_a_restart() {
        // The first-run enroll writes the token after the boot
        // snapshot (platform_faces runs before relay activation);
        // set_platform_token is the refresh, and the GET face
        // (enrolled, proxy_available) must see it without a restart.
        let beta = crate::state::PlatformFace {
            name: "beta".to_owned(),
            origin: "http://127.0.0.1:2".to_owned(),
            base: "http://127.0.0.1:2/v1/model-gateway".to_owned(),
            token: None,
        };
        let state = local_state_with_platforms(vec![face("http://127.0.0.1:1", None), beta]);
        state.set_platform_token("alpha", "tagma-token-1".to_owned());
        let body = profiles_body(&state).await.unwrap();
        let source = body.get("source").expect("source block");
        assert_eq!(source["proxy_available"], serde_json::json!(true));
        let platforms = source["platforms"].as_array().expect("platforms list");
        let alpha = platforms.iter().find(|p| p["name"] == "alpha").unwrap();
        assert_eq!(alpha["enrolled"], serde_json::json!(true));
        let beta = platforms.iter().find(|p| p["name"] == "beta").unwrap();
        assert_eq!(beta["enrolled"], serde_json::json!(false));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn switch_to_an_enrolled_platform_succeeds_and_pins_it() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        let origin = spawn_switch_gateway().await;
        let state = local_state_with_platforms(vec![face(&origin, Some("tagma-token-1"))]);
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "polis": origin },
        }))
        .unwrap();
        let response = put_profiles(State(state.clone()), op_auth(), Json(wire))
            .await
            .unwrap();
        assert_eq!(response["source"]["mode"], "model-gateway");
        assert_eq!(response["source_switch"]["to"], "model-gateway");
        // The pin: settings name the activated platform, and the live
        // face reports it (the boot and later updates resolve it).
        assert_eq!(
            crate::settings::load_profile_source_polis()
                .unwrap()
                .as_deref(),
            Some(origin.as_str())
        );
        assert_eq!(
            crate::settings::load_profile_source_mode().unwrap(),
            crate::profile_source::ProfileSourceKind::Proxy
        );
        let response = get_profiles(State(state), op_auth()).await.unwrap();
        assert_eq!(response["source"]["proxy_available"], true);
        assert_eq!(response["source"]["polis"], serde_json::json!(origin));
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    #[tokio::test]
    async fn switch_to_an_unenrolled_platform_answers_enrollment_guidance() {
        let origin = "http://127.0.0.1:1".to_owned();
        // The entry is configured but stores no token: the switch names
        // the enrollment step, not a missing token.
        let state = local_state_with_platforms(vec![face(&origin, None)]);
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "polis": origin },
        }))
        .unwrap();
        let err = put_profiles(State(state), op_auth(), Json(wire))
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("relay enrollment"),
            "got: {}",
            err.message
        );
        // An origin no entry carries is refused with the configured list.
        let state = local_state_with_platforms(vec![face(&origin, None)]);
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "polis": "http://nowhere.test" },
        }))
        .unwrap();
        let err = put_profiles(State(state), op_auth(), Json(wire))
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("the configured platforms are"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_same_mode_selection_write_through_narrows_the_live_source() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        let origin = spawn_switch_gateway().await;
        let state = make_proxy_state(Some(crate::profile_source::GatewayParams {
            base: format!("{origin}/v1/model-gateway"),
            origin: origin.clone(),
            token: "tagma-token-1".to_owned(),
        }));
        // Same-mode source member with a collection: the write goes
        // through to the gateway (the tagma pointer is the single
        // store), the live refresh serves the narrowed members, and
        // nothing switched modes.
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "collection": { "owner": "acc-x", "collection": "beta-col" } },
        }))
        .unwrap();
        let response = put_profiles(State(state.clone()), op_auth(), Json(wire))
            .await
            .unwrap();
        assert!(response.get("source_switch").is_none());
        assert_eq!(response["source"]["mode"], "model-gateway");
        let bundle = state.profiles.load();
        let names: Vec<String> = bundle.config.sets.keys().cloned().collect();
        assert_eq!(names, ["beta".to_owned()]);
        // The update is not a switch: the mode was never written (the
        // test started from an in-memory proxy state; settings never
        // saw a mode at all, so the default local spelling survives).
        assert_eq!(
            crate::settings::load_profile_source_mode().unwrap(),
            crate::profile_source::ProfileSourceKind::Local
        );
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    /// A gateway collection whose set lost every member: the adapted
    /// config reaches validation with a memberless set, and the
    /// validation funnel answers a request error naming the set —
    /// not a panic behind the relay's catch-all 502.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_gateway_set_with_no_members_is_a_request_error() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        let origin = spawn_switch_gateway().await;
        let state = make_proxy_state(Some(crate::profile_source::GatewayParams {
            base: format!("{origin}/v1/model-gateway"),
            origin: origin.clone(),
            token: "tagma-token-1".to_owned(),
        }));
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "collection": { "owner": "acc-x", "collection": "hollow-col" } },
        }))
        .unwrap();
        let err = put_profiles(State(state), op_auth(), Json(wire))
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("set 'hollow' has no profiles"),
            "got: {}",
            err.message
        );
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    /// The local wire face can assemble the same memberless shape
    /// directly: the same funnel guard answers it.
    #[tokio::test]
    async fn a_wire_set_with_no_profiles_is_a_request_error() {
        let state = crate::test_helpers::make_state();
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {
                "ep": { "id": "ep", "family": "deepseek", "api_key": "sk-x" }
            },
            "sets": [
                { "name": "hollow", "profiles": [] }
            ],
        }))
        .unwrap();
        let err = put_profiles(State(state), op_auth(), Json(wire))
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("set 'hollow' has no profiles"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_same_mode_rebind_switches_the_pull_to_the_named_platform() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        // Two live platform faces, the pull active on the first: a
        // same-mode PUT naming the other platform is a rebind, which
        // rides the switch leg (the validating fetch, the pin move,
        // and the selection) instead of rebuilding on the active
        // face and silently keeping the pull where it was.
        let origin_a = spawn_switch_gateway().await;
        let origin_b = spawn_switch_gateway().await;
        let state = {
            let mut inner = match std::sync::Arc::try_unwrap(make_proxy_state(Some(
                crate::profile_source::GatewayParams {
                    base: format!("{origin_a}/v1/model-gateway"),
                    origin: origin_a.clone(),
                    token: "tagma-token-1".to_owned(),
                },
            ))) {
                Ok(inner) => inner,
                Err(_) => panic!("make_proxy_state hands out the sole reference"),
            };
            inner.platforms = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(vec![
                face(&origin_a, Some("tagma-token-1")),
                face(&origin_b, Some("tagma-token-2")),
            ]));
            std::sync::Arc::new(inner)
        };
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "polis": origin_b, "collection": { "owner": "acc-x", "collection": "beta-col" } },
        }))
        .unwrap();
        let response = put_profiles(State(state.clone()), op_auth(), Json(wire))
            .await
            .unwrap();
        // The switch leg answered (the filter-update leg returns no
        // source_switch block), the pin moved, and the selection rode
        // the rebind.
        assert_eq!(response["source_switch"]["to"], "model-gateway");
        assert_eq!(response["source"]["polis"], serde_json::json!(origin_b));
        assert_eq!(
            crate::settings::load_profile_source_polis()
                .unwrap()
                .as_deref(),
            Some(origin_b.as_str())
        );
        let bundle = state.profiles.load();
        let names: Vec<String> = bundle.config.sets.keys().cloned().collect();
        assert_eq!(names, ["beta".to_owned()]);
        // A rebind without a selection writes nothing: the target
        // tagma's own selection (never touched here) serves.
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "polis": origin_a },
        }))
        .unwrap();
        let response = put_profiles(State(state.clone()), op_auth(), Json(wire))
            .await
            .unwrap();
        assert_eq!(response["source"]["polis"], serde_json::json!(origin_a));
        assert_eq!(
            crate::settings::load_profile_source_polis()
                .unwrap()
                .as_deref(),
            Some(origin_a.as_str())
        );
        let bundle = state.profiles.load();
        let names: Vec<String> = bundle.config.sets.keys().cloned().collect();
        assert_eq!(names, ["alpha".to_owned(), "beta".to_owned()]);
        assert_eq!(
            state
                .gateway_params
                .load_full()
                .as_ref()
                .as_ref()
                .map(|params| params.origin.as_str()),
            Some(origin_a.as_str())
        );
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn switch_to_local_serves_the_disk_file_and_persists_the_mode() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        // A proxy-mode state; the on-disk local file is the switch target.
        let disk = kallipai_adk::profile::config_path().unwrap();
        kallipai_adk::profile::save(
            &serde_json::from_value::<kallipai_adk::profile::ProfileConfig>(serde_json::json!({
                "endpoints": {
                    "deepseek": { "id": "deepseek", "family": "deepseek", "api_key": "local-key" }
                },
                "sets": {
                    "local-set": { "profiles": [
                        { "id": "p", "endpoint": "deepseek", "model": "m", "max_context_window": 8 }
                    ] }
                },
                "parking": [],
                "default": "local-set"
            }))
            .unwrap(),
            &disk,
        )
        .unwrap();
        let disk_before = std::fs::read(&disk).unwrap();
        let state = make_proxy_state(None);
        let response = put_profiles(State(state.clone()), op_auth(), Json(switch_wire("local")))
            .await
            .unwrap();
        // The switch took the branch (source_switch block) and the registry
        // now serves the on-disk file.
        assert_eq!(response["source"]["mode"], "local");
        assert_eq!(response["source_switch"]["to"], "local");
        assert_eq!(response["default"], "local-set");
        assert_eq!(state.profiles.load().config.default, "local-set");
        // The disk file itself was not rewritten by the switch.
        assert_eq!(std::fs::read(&disk).unwrap(), disk_before);
        assert_eq!(
            crate::settings::load_profile_source_mode().unwrap(),
            crate::profile_source::ProfileSourceKind::Local
        );
        let _ = std::fs::remove_file(&disk);
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    #[tokio::test]
    async fn same_mode_put_takes_the_edit_path() {
        // A PUT whose source.mode equals the live kind is an edit, not a
        // switch: the response carries no source_switch block.
        let state = crate::test_helpers::make_state();
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "local" },
        }))
        .unwrap();
        let response = put_profiles(State(state), op_auth(), Json(wire))
            .await
            .unwrap();
        assert!(response.get("source_switch").is_none());
    }

    #[tokio::test]
    async fn health_block_reports_switchability_and_local_disk() {
        // Local mode: proxy_available answers the UI gate; no local_disk
        // preview (the live config already is that file).
        let state = crate::test_helpers::make_state();
        let response = get_profiles(State(state), op_auth()).await.unwrap();
        assert_eq!(response["source"]["proxy_available"], false);
        assert!(response.get("local_disk").is_none());
        // Gateway mode: the preview names what switching back would face.
        let state = make_proxy_state(None);
        let response = get_profiles(State(state), op_auth()).await.unwrap();
        assert_eq!(response["source"]["mode"], "model-gateway");
        assert_eq!(response["source"]["proxy_available"], false);
        assert!(response.get("local_disk").is_some());
        // last_refresh on the wire is a string or null (RFC 3339),
        // never the raw epoch map the clock type would serialize to.
        assert!(response["source"]["last_refresh"].is_null());
        // The signal derives from the platform table, not the live
        // slot: an enrolled entry keeps it on in local mode too (a
        // local detour never strands the UI's gateway tab).
        let state =
            local_state_with_platforms(vec![face("http://127.0.0.1:1", Some("tagma-token-1"))]);
        let response = get_profiles(State(state), op_auth()).await.unwrap();
        assert_eq!(response["source"]["mode"], "local");
        assert_eq!(response["source"]["proxy_available"], true);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_local_detour_keeps_the_pin_and_a_polisless_switch_resolves_it() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        let origin = spawn_switch_gateway().await;
        let state = local_state_with_platforms(vec![face(&origin, Some("tagma-token-1"))]);
        // Pin the platform by switching to it, then detour through
        // local: the pin must survive the detour (mode and pin
        // stay decoupled).
        let pin: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "polis": origin },
        }))
        .unwrap();
        let pinned = put_profiles(State(state.clone()), op_auth(), Json(pin))
            .await
            .unwrap();
        assert_eq!(pinned["source_switch"]["to"], "model-gateway");
        let detoured = put_profiles(State(state.clone()), op_auth(), Json(switch_wire("local")))
            .await
            .unwrap();
        assert_eq!(detoured["source"]["mode"], "local");
        assert_eq!(
            crate::settings::load_profile_source_polis()
                .unwrap()
                .as_deref(),
            Some(origin.as_str())
        );
        // No polis on the way back: the switch resolves the pinned
        // platform exactly like boot would.
        let response = put_profiles(
            State(state.clone()),
            op_auth(),
            Json(switch_wire("model-gateway")),
        )
        .await
        .unwrap();
        assert_eq!(response["source_switch"]["to"], "model-gateway");
        assert_eq!(
            state
                .gateway_params
                .load_full()
                .as_ref()
                .clone()
                .map(|p| p.origin),
            Some(origin)
        );
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_polisless_switch_with_a_stale_pin_names_the_pin() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        crate::settings::save_profile_source_polis(Some("http://stale.test")).unwrap();
        // An enrolled platform exists elsewhere: the two-state 400
        // names the stale pin, instead of the enrollment guidance.
        let state =
            local_state_with_platforms(vec![face("http://127.0.0.1:1", Some("tagma-token-1"))]);
        let err = put_profiles(State(state), op_auth(), Json(switch_wire("model-gateway")))
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("http://stale.test") && err.message.contains("no longer resolves"),
            "got: {}",
            err.message
        );
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn an_unknown_or_empty_collection_selection_is_a_400_not_a_silent_switch() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        let origin = spawn_switch_gateway().await;
        let state = make_proxy_state(Some(crate::profile_source::GatewayParams {
            base: format!("{origin}/v1/model-gateway"),
            origin,
            token: "tagma-token-1".to_owned(),
        }));
        // A collection the account cannot see: the gateway 404s the
        // write, and the update is a 400 before anything moves.
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "collection": { "owner": "acc-x", "collection": "ghost-col" } },
        }))
        .unwrap();
        let err = put_profiles(State(state.clone()), op_auth(), Json(wire))
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("not visible to this account"),
            "got: {}",
            err.message
        );
        // A foreign owner's collection is equally invisible: the
        // pointer never writes.
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "collection": { "owner": "acc-other", "collection": "user-col" } },
        }))
        .unwrap();
        let err = put_profiles(State(state.clone()), op_auth(), Json(wire))
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("not visible to this account"),
            "got: {}",
            err.message
        );
        // A visible but empty collection writes fine and still 400s:
        // a selection that serves nothing is a request error.
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "collection": { "owner": "acc-x", "collection": "empty-col" } },
        }))
        .unwrap();
        let err = put_profiles(State(state), op_auth(), Json(wire))
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("matched no profile set")
                && err
                    .message
                    .contains("the gateway selection already names it"),
            "got: {}",
            err.message
        );
        // Nothing touched the local settings file.
        assert!(!crate::settings::settings_path().unwrap().exists());
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_collections_narrowing_that_strands_a_bound_agent_confirms_via_force() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        let origin = spawn_switch_gateway().await;
        let state = make_proxy_state(Some(crate::profile_source::GatewayParams {
            base: format!("{origin}/v1/model-gateway"),
            origin: origin.clone(),
            token: "tagma-token-1".to_owned(),
        }));
        let _sub = alt_bound_sub(&state).await;
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "collection": { "owner": "acc-x", "collection": "beta-col" } },
        }))
        .unwrap();
        let err = put_profiles(State(state.clone()), op_auth(), Json(wire))
            .await
            .unwrap_err();
        assert_eq!(err.status, 409);
        let dangling = err.dangling.expect("structured dangling list present");
        assert!(
            dangling.iter().any(|s| s.contains("'alt'")),
            "got: {dangling:?}"
        );
        // Force confirms: the same PUT narrows the live source.
        let confirmed: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "force": true,
            "source": { "mode": "model-gateway", "collection": { "owner": "acc-x", "collection": "beta-col" } },
        }))
        .unwrap();
        let response = put_profiles(State(state.clone()), op_auth(), Json(confirmed))
            .await
            .unwrap();
        assert_eq!(response["source"]["mode"], "model-gateway");
        let names: Vec<String> = state.profiles.load().config.sets.keys().cloned().collect();
        assert_eq!(names, ["beta".to_owned()]);
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_switch_that_strands_a_bound_agent_confirms_via_force() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        let origin = spawn_switch_gateway().await;
        let state = local_state_with_platforms(vec![face(&origin, Some("tagma-token-1"))]);
        let _sub = alt_bound_sub(&state).await;
        // The gateway serves alpha and beta: the agent's 'alt' binding
        // would dangle after the switch.
        let wire: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "source": { "mode": "model-gateway", "polis": origin },
        }))
        .unwrap();
        let err = put_profiles(State(state.clone()), op_auth(), Json(wire))
            .await
            .unwrap_err();
        assert_eq!(err.status, 409);
        assert!(
            err.message.contains("re-PUT with force=true"),
            "got: {}",
            err.message
        );
        // Nothing switched yet: the mode never reached settings.
        assert_eq!(
            crate::settings::load_profile_source_mode().unwrap(),
            crate::profile_source::ProfileSourceKind::Local
        );
        let confirmed: ProfileConfigWire = serde_json::from_value(serde_json::json!({
            "endpoints": {},
            "sets": [],
            "force": true,
            "source": { "mode": "model-gateway", "polis": origin },
        }))
        .unwrap();
        let response = put_profiles(State(state), op_auth(), Json(confirmed))
            .await
            .unwrap();
        assert_eq!(response["source_switch"]["to"], "model-gateway");
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }

    #[tokio::test]
    async fn apply_serves_the_model_gateway_source_too() {
        // Apply pushes the in-memory registry to live agents (a
        // distribution step, not a write), so the model-gateway
        // source answers it just like local.
        let state = make_proxy_state(None);
        let response = apply_profiles(State(state), op_auth()).await.unwrap();
        assert_eq!(response.applied + response.skipped, 0); // no live agents
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn switch_to_local_with_a_broken_disk_file_answers_409() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        // A broken profiles.toml on disk: switching back has nothing
        // to load, so the switch refuses before touching anything.
        let disk = kallipai_adk::profile::config_path().unwrap();
        std::fs::write(&disk, "not toml {{{").unwrap();
        let state = make_proxy_state(None);
        let err = put_profiles(State(state.clone()), op_auth(), Json(switch_wire("local")))
            .await
            .unwrap_err();
        assert_eq!(err.status, 409);
        assert!(
            err.message.contains("nothing was switched"),
            "got: {}",
            err.message
        );
        // Nothing switched: the slot still serves the gateway source
        // and the settings file was never written.
        assert_eq!(
            state.profile_source.load_full().source().kind(),
            crate::profile_source::ProfileSourceKind::Proxy
        );
        assert!(!crate::settings::settings_path().unwrap().exists());
        let _ = std::fs::remove_file(&disk);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn concurrent_switches_serialize_instead_of_interleaving() {
        crate::test_helpers::ensure_test_data_dir();
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
        // A valid local file, so the accepted direction can load it.
        let disk = kallipai_adk::profile::config_path().unwrap();
        kallipai_adk::profile::save(
            &serde_json::from_value::<kallipai_adk::profile::ProfileConfig>(serde_json::json!({
                "endpoints": {},
                "sets": {},
                "parking": []
            }))
            .unwrap(),
            &disk,
        )
        .unwrap();
        // One shared state, two racing switches in opposite
        // directions: the lock runs them one at a time, so whichever
        // order they take, the accepted switch lands whole and the
        // refused one (no connection params, 400) tears nothing.
        let state = make_proxy_state(None);
        let (to_local, to_gateway) = tokio::join!(
            put_profiles(State(state.clone()), op_auth(), Json(switch_wire("local"))),
            put_profiles(
                State(state.clone()),
                op_auth(),
                Json(switch_wire("model-gateway"))
            ),
        );
        assert!(to_local.is_ok());
        assert_eq!(to_gateway.unwrap_err().status, 400);
        assert_eq!(
            state.profile_source.load_full().source().kind(),
            crate::profile_source::ProfileSourceKind::Local
        );
        assert_eq!(
            crate::settings::load_profile_source_mode().unwrap(),
            crate::profile_source::ProfileSourceKind::Local
        );
        let _ = std::fs::remove_file(&disk);
        let _ = std::fs::remove_file(crate::settings::settings_path().unwrap());
    }
}
