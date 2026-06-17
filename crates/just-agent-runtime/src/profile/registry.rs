//! The profile registry: ordered capability tiers over named endpoints, backed by a
//! [`BackendSource`].
//!
//! Selection resolves a [`Tier`] per agent purely by supervisor depth: `tiers[depth.min(len-1)]`
//! (root → highest-capability tier; deeper delegation → lower tiers). Tiers are positional — no
//! name, no explicit override (see [`Tier`]). The active profile is `tier.profiles[0]`; the rest
//! of the chain is the within-tier failover order, whose backends are built lazily on first use.
//! Cross-tier failover is intentionally off.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use just_llm_client::{ChatClient, ChatClientOptions, LlmBackend};

use super::model::{Profile, Tier};

/// Lazily provides [`LlmBackend`]s keyed by endpoint id. The daemon owns the implementation
/// (reqwest + [`just_llm_client::client::BackendFactory`]); the registry looks up the active
/// profile's backend via [`get`](Self::get), and a failover profile's backend is built on first
/// use. Implementations must construct a given endpoint's backend at most once under concurrent
/// access (e.g. a locked cache), so repeated lookups share one backend.
pub trait BackendSource: Send + Sync {
    fn get(&self, endpoint_id: &str) -> Result<Arc<dyn LlmBackend>>;
}

pub struct ProfileRegistry {
    tiers: Vec<Tier>,
    /// Backends keyed by endpoint id. The active set is pre-built at daemon startup; failover
    /// endpoints are built lazily by the [`BackendSource`] on first lookup. The registry itself
    /// never constructs — it calls [`BackendSource::get`].
    source: Arc<dyn BackendSource>,
}

impl ProfileRegistry {
    /// Construct and validate: non-empty tier list, every tier non-empty. Endpoint existence,
    /// family, and base_url are validated by the daemon when it builds the active set (see
    /// `just_agent_daemon::backend`); the registry only checks structure.
    pub fn new(tiers: Vec<Tier>, source: Arc<dyn BackendSource>) -> Result<Self> {
        if tiers.is_empty() {
            bail!("profile registry has no tiers");
        }
        for (i, tier) in tiers.iter().enumerate() {
            if tier.profiles.is_empty() {
                bail!("tier at index {i} has no profiles");
            }
        }
        Ok(Self { tiers, source })
    }

    pub fn tiers(&self) -> &[Tier] {
        &self.tiers
    }

    /// Resolve the agent's tier by supervisor depth: `tiers[depth.min(len-1)]`. Root (depth 0)
    /// maps to the highest-capability tier; deeper delegation maps to lower tiers. Infallible —
    /// [`new`](Self::new) guarantees a non-empty tier list, so the index always clamps in range.
    ///
    /// Returns the resolved [`Tier`] handle so the caller (and the failover loop) can walk
    /// `tier.profiles`. The active profile is always `tier.profiles[0]`. Callers that need to
    /// know whether `depth` was clamped (e.g. to warn) compare `depth` against
    /// [`tiers().len()`](Self::tiers).
    pub fn select_profile(&self, depth: usize) -> &Tier {
        let idx = depth.min(self.tiers.len() - 1);
        &self.tiers[idx]
    }

    /// Build a [`ChatClient`] for a profile, looking up its endpoint's backend via the
    /// [`BackendSource`] (pre-built for the active set, lazily constructed for failover profiles).
    pub fn build_client(
        &self,
        profile: &Profile,
        system_prompt: Option<String>,
    ) -> Result<ChatClient> {
        let backend = self.source.get(&profile.endpoint).with_context(|| {
            format!(
                "profile '{}' references endpoint '{}' with no backend",
                profile.id, profile.endpoint
            )
        })?;

        let mut options = ChatClientOptions::new(profile.model.clone());
        if let Some(system_prompt) = system_prompt {
            options = options.with_system_prompt(system_prompt);
        }
        Ok(ChatClient::new(backend, options))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Test-only [`BackendSource`] backed by a static map.
    struct MapSource(HashMap<String, Arc<dyn LlmBackend>>);
    impl BackendSource for MapSource {
        fn get(&self, endpoint_id: &str) -> Result<Arc<dyn LlmBackend>> {
            self.0
                .get(endpoint_id)
                .cloned()
                .with_context(|| format!("unknown endpoint '{endpoint_id}'"))
        }
    }

    /// A real DeepSeek backend for fixtures — network-free construction via `LlmBackend::new`.
    /// The runtime never constructs backends in production; tests build one upstream-style.
    fn ds_backend() -> Arc<dyn LlmBackend> {
        use just_llm_client::LlmBackend;
        just_llm_client::provider::DeepSeekBackend::new(
            reqwest::Client::builder().use_rustls_tls(),
            "fake",
            None,
        )
        .expect("deepseek backend constructs without network")
    }

    /// A single tier whose active profile uses the "ds" endpoint + `deepseek-test` model.
    fn single_tier_registry() -> ProfileRegistry {
        let mut backends = HashMap::new();
        backends.insert("ds".into(), ds_backend());
        ProfileRegistry::new(
            vec![Tier {
                profiles: vec![Profile {
                    id: "p1".into(),
                    endpoint: "ds".into(),
                    model: "deepseek-test".into(),
                    max_context_window: 500_000,
                }],
            }],
            Arc::new(MapSource(backends)),
        )
        .unwrap()
    }

    /// Two tiers (both over the "ds" endpoint) with distinct models, for depth-routing tests.
    fn two_tier_registry() -> ProfileRegistry {
        let mut backends = HashMap::new();
        backends.insert("ds".into(), ds_backend());
        ProfileRegistry::new(
            vec![
                Tier {
                    profiles: vec![Profile {
                        id: "pro".into(),
                        endpoint: "ds".into(),
                        model: "deepseek-pro".into(),
                        max_context_window: 500_000,
                    }],
                },
                Tier {
                    profiles: vec![Profile {
                        id: "flash".into(),
                        endpoint: "ds".into(),
                        model: "deepseek-flash".into(),
                        max_context_window: 128_000,
                    }],
                },
            ],
            Arc::new(MapSource(backends)),
        )
        .unwrap()
    }

    #[test]
    fn select_profile_depth_zero_is_first_tier() {
        let reg = single_tier_registry();
        let tier = reg.select_profile(0);
        assert_eq!(tier.active_profile().id, "p1");
        assert_eq!(tier.active_profile().model, "deepseek-test");
    }

    #[test]
    fn select_profile_depth_routes_and_clamps() {
        let reg = two_tier_registry();
        // depth 0 (root) → tiers[0] (pro); depth 1 → tiers[1] (flash); beyond clamps to the last.
        assert_eq!(reg.select_profile(0).active_profile().model, "deepseek-pro");
        assert_eq!(
            reg.select_profile(1).active_profile().model,
            "deepseek-flash"
        );
        assert_eq!(
            reg.select_profile(9).active_profile().model,
            "deepseek-flash"
        );
    }

    #[test]
    fn build_client_binds_model_and_system_prompt() {
        let reg = single_tier_registry();
        let p = reg.select_profile(0).active_profile().clone();
        let client = reg.build_client(&p, Some("sp".into())).unwrap();
        assert_eq!(client.model(), "deepseek-test");
        assert_eq!(client.system_prompt(), Some("sp"));
    }

    #[test]
    fn new_rejects_empty_tier_list() {
        let res = ProfileRegistry::new(vec![], Arc::new(MapSource(HashMap::new())));
        assert!(res.is_err());
    }

    #[test]
    fn new_rejects_tier_with_no_profiles() {
        let res = ProfileRegistry::new(
            vec![Tier { profiles: vec![] }],
            Arc::new(MapSource(HashMap::new())),
        );
        assert!(res.is_err());
    }

    #[test]
    fn build_client_errors_when_endpoint_missing() {
        // Endpoint existence is daemon-validated at startup; this covers the runtime lookup path.
        let reg = ProfileRegistry::new(
            vec![Tier {
                profiles: vec![Profile {
                    id: "p".into(),
                    endpoint: "missing".into(),
                    model: "m".into(),
                    max_context_window: 1000,
                }],
            }],
            Arc::new(MapSource(HashMap::new())),
        )
        .unwrap();
        let profile = reg.select_profile(0).active_profile().clone();
        let err = reg
            .build_client(&profile, None)
            .expect_err("build_client should error on a missing endpoint");
        assert!(format!("{err}").contains("no backend"), "got: {err}");
    }
}
