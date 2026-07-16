//! Policy types shared between the runtime policy engine and the daemon API/config.
//!
//! Only `bash_exec` is gated (it is the arbitrary-execution surface); every other
//! tool is the agent's own self-management and runs unconditionally. There is no
//! per-tool decision lattice. The `bash_exec` rule-set is selected by a daemon-global
//! [`PolicyPreset`]; per-command overrides live in [`ExecPolicy`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, de, de::Visitor};
use std::str::FromStr;

/// The daemon-global rule-set applied to `bash_exec` classification, selected once
/// at startup by `KALLIP_POLICY_PRESET`.
///
/// There is no separate "mode" type: the preset *is* the rule bundle. The classifier
/// consumes the preset directly to decide how an unclassified command resolves and
/// whether the denylist applies.
///
/// - `Default` — strict: catalog commands allow, unclassified commands ask, the
///   builtin denylist and structural rejects (`curl | sh`, ...) deny.
/// - `Auto` — the optimized middle: like `Default` but unclassified commands allow
///   too; the denylist still applies.
/// - `AllowAll` — debug bypass: everything allows (the denylist and structural
///   rejects do not apply). Not for production.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyPreset {
    #[serde(rename = "default")]
    Default,
    Auto,
    AllowAll,
}

impl std::fmt::Display for PolicyPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Default => "default",
            Self::Auto => "auto",
            Self::AllowAll => "allow-all",
        })
    }
}

impl std::str::FromStr for PolicyPreset {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "default" => Ok(Self::Default),
            "auto" => Ok(Self::Auto),
            "allow-all" => Ok(Self::AllowAll),
            _ => Err(format!(
                "invalid policy preset '{s}' (expected default, auto, or allow-all)"
            )),
        }
    }
}

/// Per-command decision for the `bash_exec` exec-policy override layer.
///
/// This lattice's `Ord` (`Allow < Ask < Deny`) drives the monotonic-strictness
/// check in [`ExecPolicy::validate_at_least_as_strict_as`]: a child agent's
/// effective per-command decision must be at least as strict as its parent's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecDecision {
    Allow,
    Ask,
    Deny,
}

impl std::fmt::Display for ExecDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        })
    }
}

impl std::str::FromStr for ExecDecision {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "deny" => Ok(Self::Deny),
            _ => Err(format!(
                "invalid exec decision '{s}' (expected allow/ask/deny)"
            )),
        }
    }
}

/// A per-command exec-policy override: a [`ExecDecision`] plus an optional
/// human-readable `reason`.
///
/// The reason is surfaced verbatim to the agent when the override narrows the
/// verdict (Ask/Deny), so it understands *why* a command is gated and what to
/// use instead (e.g. deny `sed` with "silent substitution; make changes manually").
/// It is informational metadata only — it never participates in the
/// strictness lattice, which is purely on [`ExecDecision`].
///
/// Serializes in a dual form for backward compatibility and ergonomics: a
/// reason-less override is a bare decision string (`sed = "deny"`); a reasoned
/// override is a table (`sed = { decision = "deny", reason = "..." }`). The same
/// dual form is accepted on deserialize, for both TOML (`exec_policy.toml`) and
/// JSON (the PUT `/exec-policy` body). Unknown table fields are rejected so a
/// typo'd `reason` key fails loudly instead of silently dropping the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOverride {
    pub decision: ExecDecision,
    pub reason: Option<String>,
}

impl ExecOverride {
    pub fn new(decision: ExecDecision) -> Self {
        Self {
            decision,
            reason: None,
        }
    }

    /// Attach a reason (builder-style, consuming).
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

impl From<ExecDecision> for ExecOverride {
    fn from(decision: ExecDecision) -> Self {
        Self::new(decision)
    }
}

impl Serialize for ExecOverride {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match &self.reason {
            // Bare form: byte-identical to the legacy `ExecDecision` value, so
            // existing exec_policy.toml entries round-trip unchanged.
            None => serializer.serialize_str(&self.decision.to_string()),
            Some(reason) => {
                use serde::ser::SerializeStruct;
                let mut st = serializer.serialize_struct("ExecOverride", 2)?;
                st.serialize_field("decision", &self.decision)?;
                st.serialize_field("reason", reason)?;
                st.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ExecOverride {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ExecOverrideVisitor;

        impl<'de> Visitor<'de> for ExecOverrideVisitor {
            type Value = ExecOverride;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an exec decision (allow/ask/deny) or a table { decision, reason }")
            }

            fn visit_str<E>(self, v: &str) -> Result<ExecOverride, E>
            where
                E: de::Error,
            {
                let decision = ExecDecision::from_str(v.trim()).map_err(E::custom)?;
                Ok(ExecOverride::new(decision))
            }

            fn visit_map<A>(self, mut map: A) -> Result<ExecOverride, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut decision: Option<ExecDecision> = None;
                let mut reason: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "decision" => {
                            if decision.is_some() {
                                return Err(de::Error::duplicate_field("decision"));
                            }
                            decision = Some(map.next_value()?);
                        }
                        "reason" => {
                            if reason.is_some() {
                                return Err(de::Error::duplicate_field("reason"));
                            }
                            reason = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::unknown_field(other, &["decision", "reason"]));
                        }
                    }
                }
                let decision = decision.ok_or_else(|| de::Error::missing_field("decision"))?;
                Ok(ExecOverride { decision, reason })
            }
        }

        // `deserialize_any` so both the bare-string and table forms dispatch
        // correctly under toml (string vs table) and serde_json (string vs object).
        deserializer.deserialize_any(ExecOverrideVisitor)
    }
}

/// Per-agent `bash_exec` command-policy overrides layered on the static read-only
/// catalog. An effective decision for a command is `overrides.get(name).decision`
/// if present, else the catalog's baseline verdict (supplied by the caller, since
/// the catalog lives in the runtime crate).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExecPolicy {
    #[serde(default)]
    pub overrides: BTreeMap<String, ExecOverride>,
}

impl ExecPolicy {
    /// Validate that `self` is at least as strict as `other` (a parent).
    ///
    /// Compares *effective* decisions over the union of override keys, using
    /// `baseline(name)` as the fallback for a key absent from a side's map.
    /// `baseline(name)` is the catalog's name-level verdict (`Allow` if listed,
    /// `Ask` if absent) — a per-name least-strict value, NOT the per-invocation
    /// verdict. Per-invocation constraint verdicts (e.g. `find -delete`) remain
    /// authoritative at classify time and are invisible to this comparison, which
    /// only needs the name-level lattice to enforce monotonicity. The catalog is
    /// not visible from this crate, so the baseline is supplied by the caller.
    ///
    /// A parent **narrowing** override (e.g. `ls -> ask`) is viral: a child that
    /// drops it inherits the looser catalog baseline and is rejected. A parent
    /// **widening** override (`cargo -> allow` on an absent command) is not viral:
    /// a child may stay stricter (catalog default).
    pub fn validate_at_least_as_strict_as(
        &self,
        other: &ExecPolicy,
        baseline: impl Fn(&str) -> ExecDecision,
    ) -> Result<(), Vec<String>> {
        let effective = |policy: &ExecPolicy, name: &str| -> ExecDecision {
            policy
                .overrides
                .get(name)
                .map(|o| o.decision)
                .unwrap_or_else(|| baseline(name))
        };

        let mut violations = Vec::new();
        let names: std::collections::BTreeSet<&str> = self
            .overrides
            .keys()
            .chain(other.overrides.keys())
            .map(String::as_str)
            .collect();
        for name in names {
            let mine = effective(self, name);
            let theirs = effective(other, name);
            if mine < theirs {
                violations.push(format!(
                    "{name}: {mine} is less strict than parent's {theirs}",
                ));
            }
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// Look up the override for a command name, if any. Returns the raw override
    /// (carrying its optional reason) only — not the catalog-baseline fallback —
    /// hence `override_for` rather than a "decision" that implies an effective
    /// verdict.
    pub fn override_for(&self, name: &str) -> Option<&ExecOverride> {
        self.overrides.get(name)
    }

    /// Lowercase every override key in place. Command names are matched
    /// case-insensitively (the classifier lowercases `cmd_name`), so mixed-case
    /// keys would silently never match.
    pub fn lowercase_keys(&mut self) {
        let normalized: BTreeMap<String, ExecOverride> = self
            .overrides
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        self.overrides = normalized;
    }
}

#[cfg(test)]
mod exec_policy_tests {
    use super::{ExecDecision, ExecOverride, ExecPolicy};
    use ExecDecision::*;

    /// Baseline resolver mirroring the runtime's `classifier::exec_baseline`:
    /// listed commands → Allow, absent → Ask. `ls`/`find` are "listed"; `cargo`/`rm`
    /// are "absent" in this test fixture.
    fn baseline(name: &str) -> ExecDecision {
        match name {
            "ls" | "find" => Allow,
            _ => Ask,
        }
    }

    fn policy(pairs: &[(&str, ExecDecision)]) -> ExecPolicy {
        let mut e = ExecPolicy::default();
        for (k, v) in pairs {
            e.overrides.insert((*k).to_string(), (*v).into());
        }
        e
    }

    #[test]
    fn child_matching_parent_is_accepted() {
        let parent = policy(&[("ls", Ask)]);
        let child = policy(&[("ls", Ask)]);
        assert!(
            child
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_ok()
        );
        // Stricter child is fine too.
        let stricter = policy(&[("ls", Deny)]);
        assert!(
            stricter
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_ok()
        );
    }

    #[test]
    fn child_dropping_parent_narrowing_is_rejected() {
        // Parent narrows `ls` (baseline Allow) to Ask. Child with no override
        // inherits baseline Allow → less strict → violation.
        let parent = policy(&[("ls", Ask)]);
        let child = ExecPolicy::default();
        assert!(
            child
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_err()
        );
    }

    #[test]
    fn parent_widening_is_not_viral() {
        // Parent widens `cargo` (baseline Ask) to Allow. Child with no override
        // inherits baseline Ask (stricter) → fine.
        let parent = policy(&[("cargo", Allow)]);
        let child = ExecPolicy::default();
        assert!(
            child
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_ok()
        );
    }

    #[test]
    fn child_widening_beyond_baseline_is_rejected() {
        // Parent silent on cargo (baseline Ask). Child sets cargo→Allow → less
        // strict than parent's effective baseline → violation.
        let parent = ExecPolicy::default();
        let child = policy(&[("cargo", Allow)]);
        assert!(
            child
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_err()
        );
    }

    #[test]
    fn lowercase_keys_normalizes() {
        let mut p = policy(&[("LS", Allow), ("Cargo", Ask)]);
        p.lowercase_keys();
        assert_eq!(p.overrides.get("ls").map(|o| o.decision), Some(Allow));
        assert_eq!(p.overrides.get("cargo").map(|o| o.decision), Some(Ask));
        assert!(!p.overrides.contains_key("LS"));
    }

    // -------------------------------------------------------------------------
    // ExecOverride serde: dual-form (bare string vs {decision, reason} table)
    // -------------------------------------------------------------------------

    #[test]
    fn deserialize_bare_string_form() {
        let toml = "overrides = { sed = \"deny\" }\n";
        let p: ExecPolicy = toml::from_str(toml).unwrap();
        let ov = p.override_for("sed").unwrap();
        assert_eq!(ov.decision, Deny);
        assert!(ov.reason.is_none());
    }

    #[test]
    fn deserialize_table_form_carries_reason() {
        let toml =
            "overrides = { sed = { decision = \"deny\", reason = \"make changes manually\" } }\n";
        let p: ExecPolicy = toml::from_str(toml).unwrap();
        let ov = p.override_for("sed").unwrap();
        assert_eq!(ov.decision, Deny);
        assert_eq!(ov.reason.as_deref(), Some("make changes manually"));
    }

    #[test]
    fn deserialize_reason_accepted_on_ask() {
        let toml = "overrides = { cargo = { decision = \"ask\", reason = \"build scripts\" } }\n";
        let p: ExecPolicy = toml::from_str(toml).unwrap();
        let ov = p.override_for("cargo").unwrap();
        assert_eq!(ov.decision, Ask);
        assert_eq!(ov.reason.as_deref(), Some("build scripts"));
    }

    #[test]
    fn deserialize_rejects_unknown_field() {
        // Typo'd `reason` key must error, not silently drop the reason.
        let toml = "overrides = { sed = { decision = \"deny\", reson = \"oops\" } }\n";
        let err = toml::from_str::<ExecPolicy>(toml).unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "expected unknown_field error, got: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_empty_table_missing_decision() {
        // An empty table has no `decision`; this must fail loudly, not silently
        // default to anything.
        let toml = "overrides = { sed = {} }\n";
        let err = toml::from_str::<ExecPolicy>(toml).unwrap_err();
        assert!(
            err.to_string().contains("missing field"),
            "expected missing_field error, got: {err}"
        );
        assert!(
            err.to_string().contains("decision"),
            "error should name the missing `decision` field: {err}"
        );
    }

    #[test]
    fn deserialize_bare_string_form_via_json() {
        // The HTTP PUT path (Json<ExecPolicy>) deserializes via serde_json, so
        // the bare-string form must work there too, not just under TOML.
        let json = r#"{"overrides":{"sed":"deny"}}"#;
        let p: ExecPolicy = serde_json::from_str(json).unwrap();
        let ov = p.override_for("sed").unwrap();
        assert_eq!(ov.decision, Deny);
        assert!(ov.reason.is_none());
    }

    #[test]
    fn serialize_reason_less_entry_is_bare_string() {
        let mut p = ExecPolicy::default();
        p.overrides.insert("sed".into(), ExecOverride::new(Deny));
        let toml = toml::to_string(&p).unwrap();
        // Legacy bare form, byte-identical to the pre-reason era.
        assert!(toml.contains("sed = \"deny\""), "got:\n{toml}");
    }

    #[test]
    fn serialize_reasoned_entry_is_table() {
        let mut p = ExecPolicy::default();
        p.overrides.insert(
            "sed".into(),
            ExecOverride::new(Deny).with_reason("make changes manually"),
        );
        let toml = toml::to_string(&p).unwrap();
        assert!(toml.contains("decision = \"deny\""), "got:\n{toml}");
        assert!(
            toml.contains("reason = \"make changes manually\""),
            "got:\n{toml}"
        );
    }

    #[test]
    fn override_round_trips_both_forms() {
        // A policy mixing the bare and reasoned forms round-trips through TOML
        // (the on-disk format), preserving each entry's shape and reason.
        let mut p = ExecPolicy::default();
        p.overrides.insert("ls".into(), ExecOverride::new(Allow));
        p.overrides.insert(
            "sed".into(),
            ExecOverride::new(Ask).with_reason("build scripts; review"),
        );

        let s = toml::to_string(&p).unwrap();
        let back: ExecPolicy = toml::from_str(&s).unwrap();

        let ls = back.override_for("ls").unwrap();
        assert_eq!(ls.decision, Allow);
        assert!(ls.reason.is_none());
        let sed = back.override_for("sed").unwrap();
        assert_eq!(sed.decision, Ask);
        assert_eq!(sed.reason.as_deref(), Some("build scripts; review"));
    }

    #[test]
    fn reason_with_special_chars_survives_json_roundtrip() {
        let original =
            ExecOverride::new(Ask).with_reason("contains \" quotes, { braces }\nand newline");
        let json = serde_json::to_string(&original).unwrap();
        let back: ExecOverride = serde_json::from_str(&json).unwrap();
        assert_eq!(original, back);
    }

    // -------------------------------------------------------------------------
    // Lattice ignores reason (monotonicity is purely on decision)
    // -------------------------------------------------------------------------

    #[test]
    fn validate_ignores_reason_only_decision_matters() {
        // Same decision, different reasons: a child cannot be "less strict", and
        // differing reasons alone never constitute a violation either way.
        let parent = ExecPolicy {
            overrides: [(
                "sed".into(),
                ExecOverride::new(Deny).with_reason("parent reason"),
            )]
            .into_iter()
            .collect(),
        };
        let child = ExecPolicy {
            overrides: [(
                "sed".into(),
                ExecOverride::new(Deny).with_reason("child reason"),
            )]
            .into_iter()
            .collect(),
        };
        assert!(
            child
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_ok()
        );
        assert!(
            parent
                .validate_at_least_as_strict_as(&child, baseline)
                .is_ok()
        );
    }
}

#[cfg(test)]
mod preset_tests {
    use super::PolicyPreset;
    use PolicyPreset::*;

    #[test]
    fn display_from_str_roundtrip() {
        for p in [Default, Auto, AllowAll] {
            let s = p.to_string();
            assert_eq!(s.parse::<PolicyPreset>().unwrap(), p);
        }
    }

    #[test]
    fn from_str_rejects_invalid() {
        assert!("classify".parse::<PolicyPreset>().is_err());
        assert!("ask-all".parse::<PolicyPreset>().is_err());
        assert!("gibberish".parse::<PolicyPreset>().is_err());
    }
}
