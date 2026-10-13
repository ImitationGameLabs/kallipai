//! Tests for the profile face: wire merging, the source guard, apply and the
//! set operations.

use super::*;

use axum::Json;
use axum::extract::{Path, Query, State};

use just_llm_client::types::generation::ReasoningEffort;
use kallipai_adk::profile::{Profile, ProfileConfig, Provider};
use kallipai_common::protocol::{ApiError, SetDefaultRequest, SetReference};

use crate::auth::AuthIdentity;
use crate::probe::mask_key;

use super::apply::{RepullOutcome, apply_profiles, repull_once};
use super::read::{get_profiles, profiles_body, refresh_profile_source};
use super::sets::{DeleteSetQuery, delete_profile_set, references_grew, set_default_profile_set};
use super::wire::{ProfileConfigWire, ProfileWire, merge_wire};

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
                model_list_base_url: Some("https://list.live.example/v1".into()),
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
    wire_list(api_key, base_url, serde_json::Value::Null)
}

/// The same wire with the model-list base spelled out, so the two base
/// fields can be exercised side by side.
fn wire_list(
    api_key: serde_json::Value,
    base_url: serde_json::Value,
    model_list_base_url: serde_json::Value,
) -> ProfileConfigWire {
    serde_json::from_value(serde_json::json!({
        "endpoints": { "main": {
            "id": "main",
            "family": "deepseek",
            "api_key": api_key,
            "base_url": base_url,
            "model_list_base_url": model_list_base_url
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
fn merge_wire_null_bases_keep_live_urls() {
    let merged = merge_wire(
        &live_config(),
        wire_list(
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ),
    )
    .unwrap();
    assert_eq!(
        merged.endpoints["main"].base_url.as_deref(),
        Some("https://live.example/v1")
    );
    assert_eq!(
        merged.endpoints["main"].model_list_base_url.as_deref(),
        Some("https://list.live.example/v1")
    );
}

#[test]
fn merge_wire_empty_bases_clear() {
    let merged = merge_wire(
        &live_config(),
        wire_list(
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!(""),
        ),
    )
    .unwrap();
    assert_eq!(merged.endpoints["main"].base_url, None);
    assert_eq!(merged.endpoints["main"].model_list_base_url, None);
}

#[test]
fn merge_wire_base_strings_replace() {
    let merged = merge_wire(
        &live_config(),
        wire_list(
            serde_json::Value::Null,
            serde_json::json!("https://new.example/v1"),
            serde_json::json!("https://list.new.example/v1"),
        ),
    )
    .unwrap();
    assert_eq!(
        merged.endpoints["main"].base_url.as_deref(),
        Some("https://new.example/v1")
    );
    assert_eq!(
        merged.endpoints["main"].model_list_base_url.as_deref(),
        Some("https://list.new.example/v1")
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
            crate::profile_source::ProxyProfileSource::new("http://gw.test:7501", "tagma-token-1")
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
        axum::routing::get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "down") }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let state = make_state_with_source(std::sync::Arc::new(
        crate::profile_source::ProxyProfileSource::new(&format!("http://{addr}"), "tagma-token-1")
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
            crate::profile_source::ProxyProfileSource::new("http://127.0.0.1:1", "tagma-token-1")
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
            model_list_base_url: None,
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
            model_list_base_url: None,
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
        inner.gateway_params = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(Some(params)));
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
fn local_state_with_platforms(faces: Vec<crate::state::PlatformFace>) -> crate::state::SharedState {
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
    let state = local_state_with_platforms(vec![face("http://127.0.0.1:1", Some("tagma-token-1"))]);
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
    let state = local_state_with_platforms(vec![face("http://127.0.0.1:1", Some("tagma-token-1"))]);
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
