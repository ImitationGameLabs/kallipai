//! Profile reads: the masked snapshot, the source refresh and the shared
//! body builder.

use super::apply::broadcast_current_bundle;
use super::shared::validate_config;
use super::source::SWITCH_LOCK;
use super::wire::masked_config;
use crate::auth::AuthIdentity;
use crate::profile_source::ProfileSourceKind;
use crate::state::{ProfileBundle, SharedState};
use axum::Json;
use axum::extract::State;
use kallipai_common::protocol::ApiError;
use std::sync::Arc;

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
pub(super) async fn profiles_body(state: &SharedState) -> Result<serde_json::Value, ApiError> {
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
