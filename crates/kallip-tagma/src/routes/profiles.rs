//! Profile management API: read, update, and apply model profiles online.
//!
//! GET /profiles — return the current profile configuration with api_keys masked.
//! PUT /profiles — merge tri-state wire api_keys, validate, persist to disk, and
//!   hot-swap the in-memory registry.
//!   Does NOT affect running agents — new agents pick up the swap at spawn.
//! POST /profiles/apply — push the current registry to all live agents via a
//!   pending-reset cell; each agent rebuilds its failover state on its next wake-up.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;

use kallip_common::protocol::ApiError;
use kallip_runtime::profile::{Endpoint, Profile, ProfileConfig, ProfileRegistry, Tier};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::AuthIdentity;
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
/// success, writes to disk and swaps the [`ArcSwap`]; running agents are
/// unaffected until an explicit apply.
pub async fn put_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Json(wire): Json<ProfileConfigWire>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    let config = merge_wire(&state.profiles.load().config, wire)?;
    // Validate: build backends + trial registry. If this fails, nothing changes.
    let factory = just_llm_client::client::BackendFactory::new();
    let user_agent = crate::backend::DEFAULT_USER_AGENT;
    let source = crate::backend::build_backends(&config, factory, user_agent)
        .map_err(|e| ApiError::bad_request(format!("profile validation failed: {e:#}")))?;
    let registry = Arc::new(
        ProfileRegistry::new(config.tiers.clone(), source)
            .map_err(|e| ApiError::bad_request(format!("invalid profile registry: {e:#}")))?,
    );

    // Persist to disk (best-effort: a failure is logged but does not block the swap,
    // since the in-memory state is already validated).
    match kallip_runtime::profile::config_path() {
        Ok(path) => {
            if let Err(e) = kallip_runtime::profile::save(&config, &path) {
                warn!(path = %path.display(), "failed to persist profiles to disk: {e:#}");
            } else {
                info!(path = %path.display(), "profiles persisted to disk");
            }
        }
        Err(e) => {
            warn!("cannot resolve profiles config path for persistence: {e:#}");
        }
    }

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
struct EndpointWire {
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
}

#[derive(Deserialize)]
struct TierWire {
    profiles: Vec<ProfileWire>,
}

#[derive(Deserialize)]
pub(crate) struct ProfileConfigWire {
    tiers: Vec<TierWire>,
    endpoints: HashMap<String, EndpointWire>,
}

/// Resolve the wire tri-state `api_key` and `base_url` fields against the live
/// config into a full [`ProfileConfig`] carrying real keys.
fn merge_wire(live: &ProfileConfig, wire: ProfileConfigWire) -> Result<ProfileConfig, ApiError> {
    let mut endpoints = HashMap::new();
    for (key, ep) in wire.endpoints {
        if key != ep.id {
            return Err(ApiError::bad_request(format!(
                "endpoint map key '{key}' does not match id '{}'",
                ep.id
            )));
        }
        let api_key = match ep.api_key {
            None => live
                .endpoints
                .get(&key)
                .map(|e| e.api_key.clone())
                .ok_or_else(|| {
                    ApiError::bad_request(format!("endpoint '{key}' is new; api_key is required"))
                })?,
            Some(k) if k.is_empty() => {
                return Err(ApiError::bad_request(format!(
                    "endpoint '{key}': api_key must not be empty"
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
            Endpoint {
                id: ep.id,
                family: ep.family,
                api_key,
                base_url,
            },
        );
    }
    let tiers = wire
        .tiers
        .into_iter()
        .map(|t| Tier {
            profiles: t
                .profiles
                .into_iter()
                .map(|p| Profile {
                    id: p.id,
                    endpoint: p.endpoint,
                    model: p.model,
                    max_context_window: p.max_context_window,
                })
                .collect(),
        })
        .collect();
    Ok(ProfileConfig { tiers, endpoints })
}

/// Mask an API key for wire responses: `first4…last4`, all bullets when the key is
/// too short to leave anything identifiable.
pub(crate) fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "•".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
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
    /// Number of agents that were skipped (faulted or already pending).
    pub skipped: usize,
}

/// POST /profiles/apply — push the current registry to all live agents.
///
/// For each live agent, reads its depth, selects the new tier, and writes a
/// [`ProfileReset`] to the agent's pending-reset cell. The agent picks it up
/// on its next wake-up (top of `run_and_report`). Agents mid-round finish
/// their current work first.
pub async fn apply_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
) -> Result<Json<ApplyResponse>, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    let bundle = state.profiles.load();
    let registry = bundle.registry.clone();

    let mut applied = 0usize;
    let skipped;

    // Collect the reset targets under the read lock, then apply outside it
    // so register/remove writes are not blocked during cell writes + notify.
    let (targets, non_live): (Vec<_>, usize) = {
        let registry_guard = state.registry.read().await;
        registry_guard.iter().fold(
            (Vec::new(), 0usize),
            |(mut targets, mut skipped), (_id, entry)| {
                let Some(live) = entry.as_live() else {
                    skipped += 1;
                    return (targets, skipped);
                };
                let depth = live.identity.config.permissions.depth();
                let tier = registry.select_profile(depth).clone();
                targets.push((
                    kallip_runtime::ProfileReset {
                        tier,
                        registry: registry.clone(),
                    },
                    live.agent.pending_profile_reset.clone(),
                    live.agent.notify.clone(),
                ));
                (targets, skipped)
            },
        )
    };
    skipped = non_live;

    for (reset, cell_lock, notify) in targets {
        let mut cell = cell_lock.lock().unwrap_or_else(|e| e.into_inner());
        *cell = Some(reset);
        drop(cell);
        notify.notify_one();
        applied += 1;
    }

    info!(applied, skipped, "profile apply signaled to live agents");
    Ok(Json(ApplyResponse { applied, skipped }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RegistryEntry;
    use crate::test_helpers::{make_entry_with_rx, make_state};
    use kallip_common::agentid::AgentId;

    fn op_auth() -> AuthIdentity {
        AuthIdentity::test_new(crate::auth::Identity::Operator)
    }

    #[tokio::test]
    async fn get_profiles_masks_api_keys() {
        let state = make_state();
        // The default test endpoint carries api_key "test" (4 chars → all bullets).
        let Json(value) = get_profiles(State(state), op_auth()).await.unwrap();
        assert_eq!(value["endpoints"]["test"]["api_key"], "••••");
        // Non-key fields are untouched.
        assert_eq!(value["endpoints"]["test"]["family"], "deepseek");
    }

    #[test]
    fn mask_key_shapes() {
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("12345678"), "•".repeat(8));
        assert_eq!(mask_key("123456789"), "1234…6789");
        assert_eq!(mask_key("sk-abcdef123456wxyz"), "sk-a…wxyz");
        // Multibyte input masks by chars, never splitting a code point.
        assert_eq!(mask_key("密钥密钥"), "•".repeat(4));
    }

    fn live_config() -> ProfileConfig {
        ProfileConfig {
            tiers: vec![],
            endpoints: std::collections::HashMap::from([(
                "main".into(),
                Endpoint {
                    id: "main".into(),
                    family: "deepseek".into(),
                    api_key: "sk-live-secret-key".into(),
                    base_url: Some("https://live.example/v1".into()),
                },
            )]),
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
            "tiers": [{ "profiles": [{
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
            "tiers": []
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
}
