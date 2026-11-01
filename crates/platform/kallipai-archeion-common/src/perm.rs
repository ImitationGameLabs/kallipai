//! The permission vocabulary: one flat `surface:resource:action` string per
//! capability, shared by the registry and the data-plane services.
//!
//! A credential carries a [`PermSet`]; a route names the one string it needs.
//! The set is closed on purpose: a perm that no route checks is a checkbox
//! that grants nothing. Unknown strings are rejected at both the write and the
//! read edge, and `every_string_is_checked_by_a_route` reads the route sources
//! to fail when one of these strings loses its guard.
//!
//! Two levels per resource (read / write) is deliberate: the research on
//! fine-grained keys across vendors lands on a vocabulary small enough to
//! render as checkboxes. A third level (`admin`) arrives only with a resource
//! that needs it.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Read a user's tagmata (list, fetch one).
pub const ARCHEION_TAGMA_READ: &str = "archeion:tagma:read";

/// Rename or revoke one of the owner's tagmata.
pub const ARCHEION_TAGMA_WRITE: &str = "archeion:tagma:write";

/// Read the caller's own account identity (`GET /me`).
pub const ARCHEION_ACCOUNT_READ: &str = "archeion:account:read";

/// Read stored records: list, download, head.
pub const FILES_RECORD_READ: &str = "files:record:read";

/// Store and remove records in the caller's own space.
pub const FILES_RECORD_WRITE: &str = "files:record:write";

/// Hand a stored record to another member of the caller's space.
pub const FILES_DELIVERY_WRITE: &str = "files:delivery:write";

/// Every string a credential may carry, in the order a picker renders them.
pub const VOCABULARY: &[&str] = &[
    ARCHEION_TAGMA_READ,
    ARCHEION_TAGMA_WRITE,
    ARCHEION_ACCOUNT_READ,
    FILES_RECORD_READ,
    FILES_RECORD_WRITE,
    FILES_DELIVERY_WRITE,
];

/// What a tagma's credential carries at enrollment: read and write its own
/// records and hand one to a member of its space. A tagma reaches no archeion
/// route today, so no `archeion:*` string is here.
pub const TAGMA_DEFAULTS: &[&str] = &[FILES_RECORD_READ, FILES_RECORD_WRITE, FILES_DELIVERY_WRITE];

/// The permission gate both services run.
///
/// `None` means the call carried no record: the session cookie and the admin
/// bearer are potencies of the identity, and no record narrows those. A record
/// passes exactly the strings its set names, so an emptied set authenticates
/// every request and is allowed none of them -- a state the owner can create on
/// purpose, and not the same thing as a revoked identity.
pub fn require(
    credential: Option<&crate::control_plane::CredentialScope>,
    perm: &str,
) -> Result<(), kallipai_common::protocol::ApiError> {
    match credential {
        None => Ok(()),
        Some(scope) if scope.perms.contains(perm) => Ok(()),
        Some(_) => Err(kallipai_common::protocol::ApiError::forbidden(format!(
            "this credential does not carry {perm}"
        ))),
    }
}

/// Pre-checked on a new personal access token: reads only. Writes are an
/// explicit choice.
pub const PAT_DEFAULTS: &[&str] = &[
    ARCHEION_TAGMA_READ,
    ARCHEION_ACCOUNT_READ,
    FILES_RECORD_READ,
];

/// A string that is not part of [`VOCABULARY`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission '{perm}'")]
pub struct UnknownPerm {
    pub perm: String,
}

/// A credential's permission set. Sorted and deduplicated by construction, so
/// two sets holding the same strings are equal and serialize identically.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermSet(BTreeSet<String>);

impl PermSet {
    /// Build from untrusted strings (a request body, a stored row, a wire
    /// field). Any string outside [`VOCABULARY`] rejects the whole set: a
    /// silently dropped perm would read as a narrower credential than the
    /// caller asked for, which is a denial bug that only shows up in
    /// production.
    pub fn from_known<I, S>(perms: I) -> Result<Self, UnknownPerm>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = BTreeSet::new();
        for perm in perms {
            let perm = perm.as_ref();
            if !VOCABULARY.contains(&perm) {
                return Err(UnknownPerm {
                    perm: perm.to_owned(),
                });
            }
            set.insert(perm.to_owned());
        }
        Ok(Self(set))
    }

    /// Build from one of the crate's own default lists. Panics on a string this
    /// module did not define -- a programming error, not an input error.
    pub(crate) fn from_defaults(defaults: &[&str]) -> Self {
        Self::from_known(defaults).expect("defaults are part of VOCABULARY")
    }

    /// Whether the set grants `perm`.
    pub fn contains(&self, perm: &str) -> bool {
        self.0.contains(perm)
    }

    /// The granted strings, sorted.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// True when the credential may do nothing.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The strings this set grants that `other` does not: the audit trail's
    /// "added" side, and `other.difference(self)` its "removed" side.
    pub fn difference(&self, other: &PermSet) -> PermSet {
        Self(self.0.difference(&other.0).cloned().collect())
    }

    /// The set as a vector, for storage and for a request body.
    pub fn to_vec(&self) -> Vec<String> {
        self.0.iter().cloned().collect()
    }
}

/// A tagma's enrollment-time set.
pub fn tagma_defaults() -> PermSet {
    PermSet::from_defaults(TAGMA_DEFAULTS)
}

/// A personal access token's pre-checked set.
pub fn pat_defaults() -> PermSet {
    PermSet::from_defaults(PAT_DEFAULTS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every default names a string the vocabulary defines: a renamed constant
    /// would otherwise ship a credential carrying a perm nothing can check.
    #[test]
    fn defaults_are_part_of_the_vocabulary() {
        for perm in TAGMA_DEFAULTS.iter().chain(PAT_DEFAULTS) {
            assert!(VOCABULARY.contains(perm), "{perm} is outside VOCABULARY");
        }
    }

    /// The vocabulary is the closed set: duplicates or an accidental third
    /// level on one resource would slip past a picker silently.
    #[test]
    fn vocabulary_is_unique_and_two_level() {
        let unique: BTreeSet<&&str> = VOCABULARY.iter().collect();
        assert_eq!(unique.len(), VOCABULARY.len(), "duplicate string");
        for perm in VOCABULARY {
            let parts: Vec<&str> = perm.split(':').collect();
            assert_eq!(parts.len(), 3, "{perm} is not surface:resource:action");
            assert!(matches!(parts[2], "read" | "write"), "{perm} action");
        }
    }

    #[test]
    fn unknown_string_rejects_the_whole_set() {
        let err = PermSet::from_known([FILES_RECORD_READ, "files:records:read"]).unwrap_err();
        assert_eq!(err.perm, "files:records:read");
    }

    #[test]
    fn set_round_trips_through_serde_sorted() {
        let set = PermSet::from_known([FILES_RECORD_WRITE, ARCHEION_TAGMA_READ]).expect("known");
        let json = serde_json::to_string(&set).expect("serialize");
        assert_eq!(json, r#"["archeion:tagma:read","files:record:write"]"#);
        let back: PermSet = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, set);
    }

    #[test]
    fn difference_names_both_audit_sides() {
        let before = PermSet::from_known([FILES_RECORD_READ, ARCHEION_TAGMA_READ]).expect("known");
        let after = PermSet::from_known([FILES_RECORD_READ, ARCHEION_TAGMA_WRITE]).expect("known");
        assert_eq!(
            after.difference(&before).to_vec(),
            vec![ARCHEION_TAGMA_WRITE]
        );
        assert_eq!(
            before.difference(&after).to_vec(),
            vec![ARCHEION_TAGMA_READ]
        );
    }

    /// The route sources, compiled in: each vocabulary string must appear as a
    /// guard's argument, and every guard must name a vocabulary string. A
    /// seventh string without a caller, or a guard on a string the picker does
    /// not offer, fails here instead of shipping.
    const GUARDED_SOURCES: &[&str] = &[
        include_str!("../../kallipai-archeion/src/routes/tagmata.rs"),
        include_str!("../../kallipai-archeion/src/routes/session.rs"),
        include_str!("../../kallipai-files/src/api.rs"),
        include_str!("../../kallipai-files/src/api/get.rs"),
        include_str!("../../kallipai-files/src/api/list.rs"),
        include_str!("../../kallipai-files/src/api/put.rs"),
        include_str!("../../kallipai-files/src/api/send.rs"),
    ];

    /// The constant each string is dispatched through, so the scan looks at a
    /// guard's argument rather than at any mention of the name.
    const GUARDS: &[(&str, &str)] = &[
        (ARCHEION_TAGMA_READ, "ARCHEION_TAGMA_READ"),
        (ARCHEION_TAGMA_WRITE, "ARCHEION_TAGMA_WRITE"),
        (ARCHEION_ACCOUNT_READ, "ARCHEION_ACCOUNT_READ"),
        (FILES_RECORD_READ, "FILES_RECORD_READ"),
        (FILES_RECORD_WRITE, "FILES_RECORD_WRITE"),
        (FILES_DELIVERY_WRITE, "FILES_DELIVERY_WRITE"),
    ];

    #[test]
    fn every_string_is_checked_by_a_route() {
        assert_eq!(
            GUARDS.len(),
            VOCABULARY.len(),
            "a vocabulary string has no guard row"
        );
        let mut guarded: BTreeSet<&str> = BTreeSet::new();
        for source in GUARDED_SOURCES {
            for call in source.split("require_perm(&auth, ").skip(1) {
                let argument = call.split(')').next().expect("a call closes");
                let name = argument
                    .trim()
                    .rsplit("::")
                    .next()
                    .expect("a path has a last segment");
                guarded.insert(name);
            }
        }
        for (perm, name) in GUARDS {
            assert!(guarded.contains(name), "{perm} is checked by no route");
        }
        for name in &guarded {
            assert!(
                GUARDS.iter().any(|(_, known)| known == name),
                "a route guards {name}, which is not in the vocabulary"
            );
        }
    }
}
