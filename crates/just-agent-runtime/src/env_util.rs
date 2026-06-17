//! Shared env-var parsing helpers and the default context window.
//!
//! Neutral home for the generic env readers ([`parse_env`], [`parse_env_list`]) and the
//! `JUST_AGENT_CONTEXT_WINDOW_TOKENS` fallback ([`DEFAULT_CONTEXT_WINDOW_TOKENS`]), so neither
//! `config` nor `profile` reaches across the other for them. Consumed by `AgentConfig::load` (env
//! path) and by the implicit env profile (`profile::from_env`).

use anyhow::Result;

/// Default context window (tokens) for `JUST_AGENT_CONTEXT_WINDOW_TOKENS` when unset. Shared by
/// `AgentConfig::load` (the implicit-profile budget-shape validation anchor) and `profile::from_env`
/// (the implicit profile's `max_context_window`).
pub(crate) const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 128_000;

/// Parse a typed env var, returning `None` when unset and an error on a malformed value.
pub(crate) fn parse_env<T: std::str::FromStr>(name: &str) -> Result<Option<T>> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value.parse::<T>().map_err(|_| {
                anyhow::anyhow!("{name} must be a valid {}", std::any::type_name::<T>())
            })
        })
        .transpose()
}

/// Parse a comma-separated list env var into a `Vec<T>`.
pub(crate) fn parse_env_list<T: std::str::FromStr>(name: &str) -> Result<Option<Vec<T>>> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let items: Result<Vec<T>, _> = value.split(',').map(|s| s.trim().parse()).collect();
    let items = items.map_err(|_| {
        anyhow::anyhow!(
            "{name} must be a comma-separated list of {}",
            std::any::type_name::<T>()
        )
    })?;
    Ok(Some(items))
}
