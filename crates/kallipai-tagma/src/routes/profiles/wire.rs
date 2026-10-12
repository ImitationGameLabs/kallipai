//! The profile wire format: the request and response shapes plus the
//! tri-state merge against the live config.

use crate::probe::mask_key;
use crate::profile_source::ProfileSourceKind;
use just_llm_client::types::generation::ReasoningEffort;
use kallipai_adk::profile::{
    Profile, ProfileConfig, ProfileSet, Provider, is_valid_set_name, normalize_default,
};
use kallipai_common::protocol::{ApiError, Modality};
use serde::Deserialize;
use std::collections::HashMap;

/// Wire DTO for PUT /profiles: same shape as [`ProfileConfig`] except each
/// endpoint's `api_key` and `base_url` are tri-state (see [`merge_wire`]).
#[derive(Deserialize)]
pub(super) struct ProviderPatch {
    id: String,
    family: String,
    api_key: Option<String>,
    base_url: Option<String>,
    /// Model-list base, same tri-state as `base_url`: null keeps the live
    /// value, "" clears it (the list then comes from `base_url`), a value
    /// replaces.
    model_list_base_url: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ProfileWire {
    pub(super) id: String,
    pub(super) endpoint: String,
    pub(super) model: String,
    pub(super) max_context_window: usize,
    pub(super) store: Option<bool>,
    pub(super) effort: Option<ReasoningEffort>,
    /// Absent on the wire = the text-only default (matching the runtime
    /// model's TOML semantics), so older clients that omit the key keep
    /// their declarations intact across a PUT.
    #[serde(default = "kallipai_adk::profile::Profile::default_modalities")]
    pub(super) modalities: Vec<Modality>,
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
    pub(super) endpoints: HashMap<String, ProviderPatch>,
    /// Tri-state like the endpoint fields: an absent key keeps the live
    /// parking (an old client PUTting its full config cannot clear it);
    /// a present list replaces it.
    #[serde(default)]
    pub(super) parking: Option<Vec<ProfileWire>>,
    /// Operator-confirmed acceptance of dangling profile-set bindings: when
    /// true, `put_profiles` (and its switch and collection-selection
    /// branches) skips the dangling-bindings gate.
    #[serde(default)]
    pub(super) force: bool,
    /// Source-mode switch intent. When the requested mode differs from the
    /// live source, this PUT takes the switch branch: the profiles
    /// payload is ignored (the switch never touches profiles.toml), the
    /// mode is validated and persisted to settings.toml, and the source
    /// and registry hot-swap. See `switch_profile_source`.
    #[serde(default)]
    pub(super) source: Option<SourceModeWire>,
}
/// The `source` member of a switch-bearing PUT body.
#[derive(Deserialize)]
pub(super) struct SourceModeWire {
    pub(super) mode: ProfileSourceKind,
    /// The platform origin to activate (model-gateway switches): the
    /// serving platform's origin. Absent re-resolves like boot: the
    /// pinned platform, or the first enrolled entry before the first
    /// pin. Ignored by local switches.
    #[serde(default)]
    pub(super) polis: Option<String>,
    /// The collection to point this tagma at (absent keeps the
    /// selected one): the switch writes it through to the gateway,
    /// and a same-mode PUT carrying it is the selection update. A
    /// switch to local ignores it (no gateway pointer to write).
    #[serde(default)]
    pub(super) collection: Option<CollectionSelectionWire>,
}

/// One collection of the account's visibility domain: an absent
/// owner addresses the account's own space; an explicit owner a
/// shared or platform collection.
#[derive(Debug, Deserialize)]
pub(super) struct CollectionSelectionWire {
    #[serde(default)]
    pub(super) owner: Option<String>,
    pub(super) collection: String,
}

/// Resolve the wire tri-state `api_key` and `base_url` fields against the live
/// config into a full [`ProfileConfig`] carrying real keys.
pub(super) fn merge_wire(
    live: &ProfileConfig,
    wire: ProfileConfigWire,
) -> Result<ProfileConfig, ApiError> {
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
        // `model_list_base_url` follows the same tri-state: null keeps, ""
        // clears (the list then comes from `base_url`), a value replaces.
        let model_list_base_url = match ep.model_list_base_url {
            None => live
                .endpoints
                .get(&key)
                .and_then(|e| e.model_list_base_url.clone()),
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
                model_list_base_url,
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
pub(super) fn masked_config(config: &ProfileConfig) -> Result<serde_json::Value, ApiError> {
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
