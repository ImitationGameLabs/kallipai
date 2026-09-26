//! Typed view of the daemon's profile config response (`get_profiles`).
//!
//! Clients only render a few attributes of the config, so they deserialize
//! a field subset instead of borrowing the runtime types: unknown fields
//! are ignored by serde, and runtime-only payload (api keys, reasoning
//! effort, context windows) never enters the client's type surface. The
//! modality helpers mirror the runtime's set semantics so `profile-set
//! list` and the `status` deep view present what the runtime serves.

use std::collections::{BTreeMap, BTreeSet};

use kallipai_common::protocol::Modality;
use serde::Deserialize;

/// The set-name-keyed config the daemon serves, narrowed to what the CLI
/// renders.
#[derive(Deserialize)]
pub struct ProfileConfig {
    /// Named profile sets keyed by set name.
    pub sets: BTreeMap<String, ProfileSet>,
    /// The default set's name; empty when unset.
    #[serde(default)]
    pub default: String,
}

/// A named set, narrowed to its members: the modality helpers' data source
/// (and the member count `profile-set list` prints).
#[derive(Deserialize)]
pub struct ProfileSet {
    pub profiles: Vec<Profile>,
}

/// A member profile, narrowed to its declared input modalities. Absent
/// declarations default to text-only, matching the runtime's round-trip
/// default.
#[derive(Deserialize)]
pub struct Profile {
    #[serde(default = "Profile::default_modalities")]
    pub modalities: Vec<Modality>,
}

impl Profile {
    fn default_modalities() -> Vec<Modality> {
        vec![Modality::Text]
    }
}

impl ProfileSet {
    /// The set's effective input modalities: the intersection of member
    /// declarations, empty for a memberless set. Mirrors the runtime's set
    /// semantics (computed on demand, never stored) so the CLI presents the
    /// fact the runtime would serve.
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

    /// Whether any member declares more than the effective intersection —
    /// a capability that can never be used through this set.
    pub fn has_shadowed_members(&self) -> bool {
        let effective = self.effective_modalities();
        self.profiles
            .iter()
            .any(|p| p.modalities.iter().any(|m| !effective.contains(m)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(members: &[&[Modality]]) -> ProfileSet {
        ProfileSet {
            profiles: members
                .iter()
                .map(|ms| Profile {
                    modalities: ms.to_vec(),
                })
                .collect(),
        }
    }

    /// One table row: (member declarations, effective intersection, any
    /// member shadowed).
    type Row<'a> = (&'a [&'a [Modality]], &'a [Modality], bool);

    /// Table of (member declarations, effective intersection, any member
    /// shadowed), pinning the intersection semantics the runtime serves.
    #[test]
    fn modality_semantics_table() {
        let cases: &[Row] = &[
            // Single member: the declaration is the intersection.
            (&[&[Modality::Text]], &[Modality::Text], false),
            (
                &[&[Modality::Text, Modality::Image]],
                &[Modality::Text, Modality::Image],
                false,
            ),
            // A member declaring beyond the intersection is shadowed.
            (
                &[&[Modality::Text, Modality::Image], &[Modality::Text]],
                &[Modality::Text],
                true,
            ),
            // Identical members: nothing shadowed.
            (
                &[&[Modality::Text], &[Modality::Text]],
                &[Modality::Text],
                false,
            ),
            // Memberless: empty intersection, nothing shadowed.
            (&[], &[], false),
            // Shadowing is any-member, not first-member.
            (
                &[
                    &[Modality::Text],
                    &[Modality::Text, Modality::Image],
                    &[Modality::Text],
                ],
                &[Modality::Text],
                true,
            ),
        ];
        for (members, expected, shadowed) in cases {
            let set = set(members);
            let effective: Vec<Modality> = set.effective_modalities().into_iter().collect();
            assert_eq!(effective, *expected, "members {members:?}");
            assert_eq!(set.has_shadowed_members(), *shadowed, "members {members:?}");
        }
    }

    /// Unknown fields at every level are ignored; the fields the CLI
    /// renders survive the parse.
    #[test]
    fn deserializes_ignoring_unknown_fields() {
        let raw = serde_json::json!({
            "sets": {
                "main": {
                    "description": "primary",
                    "name": "main",
                    "profiles": [{
                        "id": "p0",
                        "endpoint": "e0",
                        "model": "m",
                        "max_context_window": 8,
                        "store": null,
                        "effort": "high",
                        "modalities": ["image"],
                    }],
                }
            },
            "default": "main",
            "endpoints": {},
            "parking": [],
        });
        let cfg: ProfileConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.default, "main");
        let set = cfg.sets.get("main").unwrap();
        assert_eq!(set.profiles.len(), 1);
        assert_eq!(set.profiles[0].modalities, vec![Modality::Image]);
    }

    /// An undeclared modality list reads as the text-only default, and an
    /// absent default set name reads as empty — both matching the runtime.
    #[test]
    fn missing_fields_take_runtime_defaults() {
        let raw = serde_json::json!({
            "sets": { "main": { "profiles": [{ "model": "m" }] } },
        });
        let cfg: ProfileConfig = serde_json::from_value(raw).unwrap();
        assert!(cfg.default.is_empty());
        assert_eq!(
            cfg.sets["main"].profiles[0].modalities,
            vec![Modality::Text]
        );
    }
}
