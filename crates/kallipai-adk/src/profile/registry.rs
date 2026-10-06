//! The profile registry: named profile sets over named providers, backed by a
//! [`BackendSource`].
//!
//! Sets are addressed by name (exact match, case-sensitive); the chosen name is
//! persisted on the agent record at spawn. The active profile is `set.profiles[0]`;
//! the rest of the chain is the within-set failover order, whose backends are built
//! lazily on first use. Cross-set failover is intentionally off.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use just_llm_client::{GenerationClient, GenerationClientOptions, LlmBackend};
use kallipai_common::protocol::Modality;

use super::model::{Profile, ProfileSet};

/// Lazily provides [`LlmBackend`]s keyed by provider id. The tagma owns the implementation
/// (reqwest + [`just_llm_client::client::BackendFactory`]); the registry looks up the active
/// profile's backend via [`get`](Self::get), and a failover profile's backend is built on first
/// use. Implementations must construct a given provider's backend at most once under concurrent
/// access (e.g. a locked cache), so repeated lookups share one backend.
pub trait BackendSource: Send + Sync {
    fn get(&self, provider_id: &str) -> Result<Arc<dyn LlmBackend>>;
}

pub struct ProfileRegistry {
    sets: BTreeMap<String, ProfileSet>,
    /// Backends keyed by provider id. The active set is pre-built at tagma startup; failover
    /// endpoints are built lazily by the [`BackendSource`] on first lookup. The registry itself
    /// never constructs — it calls [`BackendSource::get`].
    source: Arc<dyn BackendSource>,
}

impl ProfileRegistry {
    /// Construct and normalize: each set's `name` is synchronized with its map key
    /// (deserialization cannot fill it — `name` is skipped on the wire — so this
    /// constructor is the single normalization point, unconditionally overwriting
    /// `name` with the key; the name can never disagree with the key it is
    /// addressed by). An empty collection is allowed (profile-less boot —
    /// resolution dangles until a profile is added), and a memberless set is
    /// held rather than rejected: the tagma's boot stays alive with the set
    /// visible, while resolution of a binding to it fails per agent (see
    /// [`select_set`](Self::select_set) and [`resolve_recorded_set`](Self::resolve_recorded_set)).
    /// Provider existence, family, and base_url are validated by the tagma when
    /// it builds the active set (see `kallipai_tagma::backend`); the registry
    /// only checks structure.
    pub fn new(
        mut sets: BTreeMap<String, ProfileSet>,
        source: Arc<dyn BackendSource>,
    ) -> Result<Self> {
        for (key, set) in sets.iter_mut() {
            set.name = key.clone();
        }
        Ok(Self { sets, source })
    }

    pub fn sets(&self) -> &BTreeMap<String, ProfileSet> {
        &self.sets
    }

    /// Resolve a set by its exact name. Unknown names error with the available set names
    /// listed, so the caller surfaces an actionable message (spawn rejects with it; runtime
    /// rebinds treat it as a dangling record). A memberless set is held for
    /// visibility but errors here: nothing in it can serve, so spawn refuses
    /// the binding at the agent level.
    pub fn select_set(&self, name: &str) -> Result<&ProfileSet> {
        let set = self.sets.get(name).ok_or_else(|| {
            let known: Vec<&str> = self.sets.keys().map(String::as_str).collect();
            anyhow::anyhow!(
                "unknown profile set '{name}'; available sets: {}",
                known.join(", ")
            )
        })?;
        if set.profiles.is_empty() {
            bail!("profile set '{name}' has no profiles; add a member or rebind");
        }
        Ok(set)
    }
    /// Resolve the set a record is bound to. Spawn writes the binding;
    /// restore, reactivation, and delivery re-read it. A missing binding
    /// (record predates set binding) and an unknown name are the same
    /// dangling state — callers surface it instead of guessing a set. A memberless
    /// set is the same agent-level failure: the binding names a real set that
    /// cannot serve, so restore, reactivation, and delivery all dangle on it.
    pub fn resolve_recorded_set(&self, binding: Option<&str>) -> Result<&ProfileSet, DanglingSet> {
        let name = binding.ok_or_else(|| self.dangling("agent record has no profile set"))?;
        let set = self
            .sets
            .get(name)
            .ok_or_else(|| self.dangling(&format!("unknown profile set '{name}'")))?;
        if set.profiles.is_empty() {
            return Err(self.dangling(&format!("profile set '{name}' has no profiles")));
        }
        Ok(set)
    }

    fn dangling(&self, reason: &str) -> DanglingSet {
        DanglingSet {
            reason: reason.to_owned(),
            available: self.sets.keys().cloned().collect(),
        }
    }

    /// Build a [`GenerationClient`] for a profile, looking up its provider's backend via the
    /// [`BackendSource`] (pre-built for the active set, lazily constructed for failover profiles).
    pub fn build_client(
        &self,
        profile: &Profile,
        system_prompt: Option<String>,
    ) -> Result<GenerationClient> {
        let backend = self.source.get(&profile.endpoint).with_context(|| {
            format!(
                "profile '{}' references endpoint '{}' with no backend",
                profile.id, profile.endpoint
            )
        })?;

        let mut options = GenerationClientOptions::new(profile.model.clone());
        if let Some(system_prompt) = system_prompt {
            options = options.with_system_prompt(system_prompt);
        }
        Ok(GenerationClient::new(backend, options))
    }
}

/// Hint surfaced on an empty registry (and reused verbatim by the tagma's
/// sentinel backend, so the zero-profile story reads one voice).
pub const NO_PROFILE_HINT: &str =
    "no model profile is configured; add one via the tagma management page";

/// A recorded profile-set binding that does not resolve: the record has no
/// binding, or names a set the registry no longer offers. `available`
/// carries the current set names so callers can surface a recovery hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanglingSet {
    pub reason: String,
    pub available: Vec<String>,
}

impl std::fmt::Display for DanglingSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = if self.available.is_empty() {
            "none configured".to_owned()
        } else {
            self.available.join(", ")
        };
        write!(f, "{}; available sets: {names}", self.reason)
    }
}

/// Wake modality gate failure: the restored context carries
/// references to content in modalities the bound set cannot serve. Mirrors
/// [`DanglingSet`]'s shape — a human reason plus the two decision values —
/// so callers surface one recovery action: rebind the agent to a set whose
/// effective modalities cover the context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalityBlocked {
    pub reason: String,
    pub required: BTreeSet<Modality>,
    pub served: BTreeSet<Modality>,
}

impl std::fmt::Display for ModalityBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; context requires {{{}}}, bound set serves {{{}}}; rebind the agent to a set whose modalities cover the context",
            self.reason,
            fmt_modalities(&self.required),
            fmt_modalities(&self.served),
        )
    }
}

/// Canonical-order modality list for gate messages (mirrors
/// `Modality::as_str`, joined for display).
fn fmt_modalities(modalities: &BTreeSet<Modality>) -> String {
    Modality::ALL
        .iter()
        .filter(|m| modalities.contains(m))
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::test_support::{MapSource, ds_backend};

    fn set(name: &str, model: &str) -> ProfileSet {
        ProfileSet {
            name: name.into(),
            description: None,
            profiles: vec![Profile {
                id: format!("p-{name}"),
                endpoint: "ds".into(),
                model: model.into(),
                max_context_window: 500_000,
                store: None,
                effort: None,
                modalities: Profile::default_modalities(),
            }],
        }
    }

    fn single_set_registry() -> ProfileRegistry {
        let mut backends = HashMap::new();
        backends.insert("ds".into(), ds_backend());
        ProfileRegistry::new(
            BTreeMap::from([("only".to_string(), set("only", "deepseek-test"))]),
            Arc::new(MapSource(backends)),
        )
        .unwrap()
    }

    /// "alpha" < "beta" in sorted order, so alpha occupies the first positional slot.
    fn two_set_registry() -> ProfileRegistry {
        let mut backends = HashMap::new();
        backends.insert("ds".into(), ds_backend());
        ProfileRegistry::new(
            BTreeMap::from([
                ("beta".to_string(), set("beta", "deepseek-flash")),
                ("alpha".to_string(), set("alpha", "deepseek-pro")),
            ]),
            Arc::new(MapSource(backends)),
        )
        .unwrap()
    }

    #[test]
    fn new_syncs_set_name_with_map_key() {
        let reg = two_set_registry();
        assert_eq!(reg.sets()["alpha"].name, "alpha");
        assert_eq!(reg.sets()["beta"].name, "beta");
    }

    #[test]
    fn select_set_returns_named_set() {
        let reg = two_set_registry();
        let set = reg.select_set("beta").unwrap();
        assert_eq!(set.active_profile().model, "deepseek-flash");
    }

    #[test]
    fn select_set_unknown_name_lists_available() {
        let reg = two_set_registry();
        let err = reg
            .select_set("missing")
            .expect_err("unknown name must error");
        let msg = format!("{err}");
        assert!(msg.contains("'missing'"), "got: {msg}");
        assert!(msg.contains("alpha") && msg.contains("beta"), "got: {msg}");
    }

    #[test]
    fn resolve_recorded_set_returns_named_set() {
        let reg = two_set_registry();
        let set = reg.resolve_recorded_set(Some("beta")).unwrap();
        assert_eq!(set.name, "beta");
        assert_eq!(set.active_profile().model, "deepseek-flash");
    }
    #[test]
    fn resolve_recorded_set_dangling_states_carry_reason_and_available() {
        let reg = two_set_registry();
        let missing = reg
            .resolve_recorded_set(None)
            .expect_err("unbound record dangles");
        assert!(
            missing.reason.contains("no profile set"),
            "got: {missing:?}"
        );
        assert_eq!(missing.available, vec!["alpha", "beta"]);
        let unknown = reg
            .resolve_recorded_set(Some("missing"))
            .expect_err("unknown name dangles");
        assert!(unknown.reason.contains("'missing'"), "got: {unknown:?}");
        assert_eq!(unknown.available, vec!["alpha", "beta"]);
        let msg = format!("{unknown}");
        assert!(msg.contains("alpha, beta"), "hint lists sets: {msg}");
    }

    #[test]
    fn build_client_binds_model_and_system_prompt() {
        let reg = single_set_registry();
        let p = reg.select_set("only").unwrap().active_profile().clone();
        let client = reg.build_client(&p, Some("sp".into())).unwrap();
        assert_eq!(client.model(), "deepseek-test");
        assert_eq!(client.system_prompt(), Some("sp"));
    }

    #[test]
    fn new_allows_empty_collection_and_resolve_dangles() {
        let reg = ProfileRegistry::new(BTreeMap::new(), Arc::new(MapSource(HashMap::new())))
            .expect("empty collection is constructible");
        let dangling = reg
            .resolve_recorded_set(Some("any"))
            .expect_err("resolve on an empty registry dangles");
        assert!(dangling.available.is_empty());
        assert!(format!("{dangling}").contains("none configured"));
        assert!(reg.select_set("any").is_err());
    }

    #[test]
    fn memberless_set_is_held_and_fails_at_resolution() {
        let sets = BTreeMap::from([(
            "empty".to_string(),
            ProfileSet {
                name: "empty".into(),
                description: None,
                profiles: vec![],
            },
        )]);
        let reg = ProfileRegistry::new(sets, Arc::new(MapSource(HashMap::new())))
            .expect("memberless set is held, not rejected");
        assert_eq!(reg.sets()["empty"].name, "empty");
        let err = reg
            .select_set("empty")
            .expect_err("spawn-side select refuses a memberless set");
        assert!(format!("{err}").contains("has no profiles"), "got: {err}");
        let dangling = reg
            .resolve_recorded_set(Some("empty"))
            .expect_err("record-side resolve dangles on a memberless set");
        assert!(
            dangling.reason.contains("has no profiles"),
            "got: {dangling:?}"
        );
        assert_eq!(dangling.available, vec!["empty".to_string()]);
    }

    #[test]
    fn build_client_errors_when_provider_missing() {
        // Provider existence is host-validated at startup; this covers the runtime lookup path.
        let mut lone = set("lone", "m");
        lone.profiles[0].endpoint = "missing".into();
        lone.profiles[0].max_context_window = 1000;
        let reg = ProfileRegistry::new(
            BTreeMap::from([("lone".to_string(), lone)]),
            Arc::new(MapSource(HashMap::new())),
        )
        .unwrap();
        let profile = reg.select_set("lone").unwrap().active_profile().clone();
        let err = reg
            .build_client(&profile, None)
            .expect_err("build_client should error on a missing provider");
        assert!(format!("{err}").contains("no backend"), "got: {err}");
    }

    #[test]
    fn ensure_supports_failure_carries_required_and_served() {
        let s = set("a", "m");
        let required = BTreeSet::from([Modality::Text, Modality::Image]);
        let err = s.ensure_supports(&required).unwrap_err();
        assert_eq!(
            err.required,
            BTreeSet::from([Modality::Text, Modality::Image])
        );
        assert_eq!(err.served, BTreeSet::from([Modality::Text]));
        // Within-capacity requirements pass.
        s.ensure_supports(&BTreeSet::from([Modality::Text]))
            .unwrap();
    }
}
