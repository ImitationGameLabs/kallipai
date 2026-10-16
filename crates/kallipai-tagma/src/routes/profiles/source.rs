//! Profile-source switching: the local-write guard, the source-mode
//! switch and the collection-selection write-through.

use super::read::profiles_body;
use super::shared::{dangling_bindings, validate_config};
use super::wire::{CollectionSelectionWire, SourceModeWire};
use crate::profile_source::ProfileSource as _;
use crate::profile_source::ProfileSourceKind;
use crate::state::SharedState;
use kallipai_common::protocol::ApiError;

/// Guard for the mutating profile face under the gateway source:
/// local writes would be silently overwritten by the next refresh,
/// so they are refused with the disclosure instead. GET routes stay
/// open (the snapshot is served as-is). A source-mode switch bypasses
/// this guard by design: put_profiles dispatches on the intent before
/// reaching here, so the guard only ever sees edit requests.
/// Apply is not guarded: it pushes the in-memory registry to live
/// agents (a distribution step, not a write), so both sources serve it.
pub(super) fn require_local_source(state: &SharedState) -> Result<(), ApiError> {
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
pub(super) static SWITCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
pub(super) async fn switch_profile_source(
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
            // after the gateway proves reachable and the token proves live.
            // A dead token or an unreachable or erroring gateway fails here
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
pub(super) async fn update_profile_selection(
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
