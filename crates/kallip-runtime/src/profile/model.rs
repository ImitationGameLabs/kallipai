//! Runtime data model for the profile registry.

use just_llm_client::types::generation::ReasoningEffort;
use serde::{Deserialize, Serialize};

use std::collections::BTreeSet;

use super::registry::ModalityBlocked;
use kallip_common::protocol::Modality;

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
    /// Input modalities this profile accepts (TOML `modalities`). Defaults
    /// to text-only; the default is omitted on serialization (see
    /// [`Profile::modalities_is_default`]) so existing configs round-trip
    /// byte-stable.
    #[serde(
        default = "Profile::default_modalities",
        skip_serializing_if = "Profile::modalities_is_default"
    )]
    pub modalities: Vec<Modality>,
}

impl Profile {
    /// The implicit modality declaration: text-only (the conservative
    /// default — an unannotated profile never claims capabilities it may
    /// not have).
    pub fn default_modalities() -> Vec<Modality> {
        vec![Modality::Text]
    }

    /// serde `skip_serializing_if` guard: the text-only default is omitted
    /// on serialization so existing profiles.toml write-backs stay
    /// byte-stable (no new key materializes on files that never declared
    /// modalities). The parameter is `&Vec<_>` because serde invokes the
    /// path as `fn(&T) -> bool` with `T = Vec<Modality>` — no deref
    /// coercion through fn items.
    #[allow(clippy::ptr_arg)]
    pub fn modalities_is_default(modalities: &Vec<Modality>) -> bool {
        modalities.len() == 1 && modalities[0] == Modality::Text
    }
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

    /// The set's effective input modalities: the intersection of member
    /// declarations (computed on demand, never stored — a materialized copy
    /// would be a second source of truth able to drift from the member
    /// declarations). With every member supporting at least the intersection,
    /// within-set failover never crosses a modality boundary — the
    /// declarative constraint that makes runtime modality filtering
    /// unnecessary.
    /// The web mirror (setEffectiveModalities) matches this, including
    /// the empty-set short-circuit to an empty intersection.
    pub fn effective_modalities(&self) -> BTreeSet<Modality> {
        let Some(first) = self.profiles.first() else {
            return BTreeSet::new();
        };
        let mut effective: BTreeSet<Modality> = first.modalities.iter().copied().collect();
        for profile in &self.profiles[1..] {
            effective.retain(|m| profile.modalities.contains(m));
        }
        effective
    }

    /// Whether any member declares strictly more than the set's effective
    /// intersection — a capability that can never be used through this set.
    /// Load-time warnings key off this: declared-capability shrinkage is
    /// never silent.
    pub fn has_shadowed_members(&self) -> bool {
        let effective = self.effective_modalities();
        self.profiles
            .iter()
            .any(|p| p.modalities.iter().any(|m| !effective.contains(m)))
    }

    /// Whether a context carrying `required` modalities can run against this
    /// set: the requirement must be a subset of the effective intersection.
    /// Kept modality-agnostic — adding a modality never changes this code.
    pub fn supports(&self, required: &BTreeSet<Modality>) -> bool {
        required.is_subset(&self.effective_modalities())
    }

    /// Wake modality gate: [`Self::supports`] as a checked
    /// judgment — the failure carries the required and served sets plus the
    /// recovery action, so the wake path can surface one actionable error.
    pub fn ensure_supports(&self, required: &BTreeSet<Modality>) -> Result<(), ModalityBlocked> {
        if self.supports(required) {
            return Ok(());
        }
        Err(ModalityBlocked {
            reason: "bound profile set cannot serve the restored context".to_owned(),
            required: required.clone(),
            served: self.effective_modalities(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, modalities: &[Modality]) -> Profile {
        Profile {
            id: id.into(),
            endpoint: "ep".into(),
            model: "m".into(),
            max_context_window: 1000,
            store: None,
            effort: None,
            modalities: modalities.to_vec(),
        }
    }

    fn set(profiles: Vec<Profile>) -> ProfileSet {
        ProfileSet {
            name: "s".into(),
            description: None,
            profiles,
        }
    }

    fn set_of(modalities: &[&[Modality]]) -> ProfileSet {
        set(modalities
            .iter()
            .enumerate()
            .map(|(i, m)| profile(&format!("p{i}"), m))
            .collect())
    }

    #[test]
    fn default_modalities_are_text_only() {
        assert_eq!(Profile::default_modalities(), vec![Modality::Text]);
        assert!(Profile::modalities_is_default(
            &Profile::default_modalities()
        ));
        assert!(!Profile::modalities_is_default(&vec![
            Modality::Text,
            Modality::Image
        ]));
        assert!(!Profile::modalities_is_default(&vec![]));
        assert!(!Profile::modalities_is_default(&vec![Modality::Image]));
    }

    #[test]
    fn effective_modalities_intersect_members() {
        // Uniform declarations pass through.
        assert_eq!(
            set_of(&[&[Modality::Text], &[Modality::Text]]).effective_modalities(),
            BTreeSet::from([Modality::Text])
        );
        // A uniform multimodal set keeps the shared declaration.
        assert_eq!(
            set_of(&[
                &[Modality::Text, Modality::Image],
                &[Modality::Text, Modality::Image],
            ])
            .effective_modalities(),
            BTreeSet::from([Modality::Text, Modality::Image])
        );
        // A narrower member shadows the set's capability.
        assert_eq!(
            set_of(&[&[Modality::Text, Modality::Image], &[Modality::Text]]).effective_modalities(),
            BTreeSet::from([Modality::Text])
        );
        // Empty sets intersect to empty (the registry invariant forbids
        // them; the method stays total).
        assert!(set(vec![]).effective_modalities().is_empty());
    }

    #[test]
    fn shadowed_members_flag_tracks_the_intersection() {
        assert!(!set_of(&[&[Modality::Text], &[Modality::Text]]).has_shadowed_members());
        assert!(
            set_of(&[&[Modality::Text, Modality::Image], &[Modality::Text]]).has_shadowed_members()
        );
    }

    #[test]
    fn supports_requires_a_subset_of_effective() {
        let text_set = set_of(&[&[Modality::Text]]);
        let multimodal_set = set_of(&[&[Modality::Text, Modality::Image]]);

        // The empty requirement is trivially supported.
        assert!(text_set.supports(&BTreeSet::new()));
        assert!(text_set.supports(&BTreeSet::from([Modality::Text])));
        assert!(multimodal_set.supports(&BTreeSet::from([Modality::Text])));
        // Non-subset demands are refused — including the "neither bigger
        // nor smaller" {audio} vs {text, image} corner.
        assert!(!text_set.supports(&BTreeSet::from([Modality::Image])));
        assert!(multimodal_set.supports(&BTreeSet::from([Modality::Image])));
        assert!(!multimodal_set.supports(&BTreeSet::from([Modality::Audio])));
        assert!(!text_set.supports(&BTreeSet::from([Modality::Text, Modality::Image])));
    }

    #[test]
    fn modalities_round_trip_with_the_default_omitted() {
        // A text-only profile serializes without the key at all...
        let toml = toml::to_string(&profile("p", &[Modality::Text])).unwrap();
        assert!(!toml.contains("modalities"));
        // ...and a key-less profile deserializes to the default.
        let parsed: Profile = toml::from_str(&toml).unwrap();
        assert!(Profile::modalities_is_default(&parsed.modalities));
        // A non-default declaration round-trips explicitly, snake_case.
        let toml = toml::to_string(&profile("p", &[Modality::Text, Modality::Image])).unwrap();
        assert!(toml.contains("modalities"));
        let parsed: Profile = toml::from_str(&toml).unwrap();
        assert_eq!(parsed.modalities, vec![Modality::Text, Modality::Image]);
    }
}
