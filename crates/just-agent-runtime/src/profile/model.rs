//! Runtime data model for the profile registry.

/// A provider instance: credentials + endpoint. Maps ~1:1 to a just-llm-client backend.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub id: String,
    /// Backend family — dispatched by the daemon's `BackendFactory` ("deepseek" /
    /// "openai-compatible").
    pub family: String,
    pub api_key: String,
    pub base_url: Option<String>,
}

/// A model bound to an [`Endpoint`], carrying its declared capabilities.
#[derive(Clone, Debug)]
pub struct Profile {
    pub id: String,
    /// The [`Endpoint::id`] this profile connects through.
    pub endpoint: String,
    pub model: String,
    /// Declared context window — the authoritative source for this profile's window. Required on
    /// both paths: config-file profiles declare it in TOML; the implicit env profile
    /// (`profile::from_env`) derives it from `JUST_AGENT_CONTEXT_WINDOW_TOKENS`. Installed into
    /// `AgentConfig` at spawn via `set_context_window`, and re-applied on within-tier failover.
    pub max_context_window: usize,
}

/// A capability bucket with an ordered failover chain of profiles.
///
/// Tiers are **purely positional**: the registry is ordered by capability rank and an agent's
/// tier is selected by supervisor depth (`tiers[depth.min(len-1)]`) — root (depth 0) maps to the
/// highest-capability tier. There is no name or explicit override; treat the tier list as
/// append-only / truncate-tail, since reordering or removing a middle tier silently rebinds
/// agents. The order here is the within-tier failover order (profile 0 first). Cross-tier
/// failover is intentionally off.
#[derive(Clone, Debug)]
pub struct Tier {
    pub profiles: Vec<Profile>,
}

impl Tier {
    /// The spawn-time active profile (always `profiles[0]`). At runtime the active profile may
    /// advance via within-tier failover — see `FailoverState::current_profile`, which tracks the
    /// live position and differs once failover has advanced. Non-empty profiles is a registry
    /// construction invariant ([`crate::profile::ProfileRegistry::new`] rejects empty tiers), so
    /// this never panics for a tier obtained through the registry.
    pub fn active_profile(&self) -> &Profile {
        self.profiles
            .first()
            .expect("tier has profiles (registry construction invariant)")
    }
}
