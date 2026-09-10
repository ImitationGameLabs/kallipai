//! Runtime data model for the profile registry.

use just_llm_client::types::generation::ReasoningEffort;
use serde::{Deserialize, Serialize};

/// A provider instance: credentials + endpoint. Maps ~1:1 to a just-llm-client backend.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    /// Backend family — dispatched by the tagma's `BackendFactory` ("deepseek" /
    /// "openai-compatible").
    pub family: String,
    pub api_key: String,
    pub base_url: Option<String>,
}

/// A model bound to a [`Provider`], carrying its declared capabilities.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    /// The [`Provider::id`] this profile connects through.
    pub endpoint: String,
    pub model: String,
    /// Declared context window — the authoritative source for this profile's window. Required on
    /// both paths: config-file profiles declare it in TOML; the implicit env profile
    /// (`profile::from_env`) derives it from `KALLIP_CONTEXT_WINDOW_TOKENS`. Installed into
    /// `AgentConfig` at spawn via `set_context_window`, and re-applied on within-set failover.
    pub max_context_window: usize,
    /// Server-side conversation storage for the upstream chain (`None` = enabled, matching
    /// the client default). Only Responses-family backends continue chains; `false` (and any
    /// non-Responses family) replays the full conversation every turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Requested reasoning effort, forwarded to every backend that maps it (`None` = leave
    /// the request's effort unset). Levels: low/medium/high/xhigh/max.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
}

/// A named set of profiles with an ordered failover chain.
///
/// Sets are addressed by **name** (exact match, case-sensitive, `^[A-Za-z0-9_-]+$`)
/// and chosen explicitly at spawn; the name is persisted on the agent record. The
/// order within `profiles` is the failover order (profile 0 first). Cross-set
/// failover is intentionally off.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileSet {
    /// Addressed by its map key in `profiles.toml` (`[sets.<name>]`); serde skips it so the
    /// serialized form carries no redundant copy (the registry/loader injects the key).
    #[serde(skip)]
    pub name: String,
    /// Optional human-readable summary of what this set is for.
    pub description: Option<String>,
    pub profiles: Vec<Profile>,
}

impl ProfileSet {
    /// The spawn-time active profile (always `profiles[0]`). At runtime the active profile may
    /// advance via within-set failover — see `FailoverState::current_profile`, which tracks the
    /// live position and differs once failover has advanced. Non-empty profiles is a registry
    /// construction invariant ([`crate::profile::ProfileRegistry::new`] rejects empty sets), so
    /// this never panics for a set obtained through the registry.
    pub fn active_profile(&self) -> &Profile {
        self.profiles
            .first()
            .expect("set has profiles (registry construction invariant)")
    }
}
