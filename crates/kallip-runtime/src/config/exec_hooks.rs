//! Exec-hook rules for the policy classifier's hook phase.
//!
//! Owns the `exec_hooks.toml` surface: operator-declared overrides layered
//! over the builtin preset rules, loaded once at tagma startup.
//! [`load_exec_hook_rules`] is the sole public entry; the builtin/merge/parse
//! helpers and the serde shims stay private. Re-exported by `crate::config`.
use crate::policy::classifier::hooks::WRITE_REDIRECT_KEY;
use crate::policy::{HookPhase, HookRule, Trigger};
use kallip_shell::tools::names;

/// Operator-declared exec-hook overrides from `exec_hooks.toml` (tagma-wide),
/// layered over the builtin preset rules.
///
/// v1 observes `bash_exec` commands only (the exec domain) — hence *exec*
/// hooks, the sibling of `exec_policy.toml`. The operator surface mirrors
/// it: a builtin rule set ships with the tagma (no file needed — the
/// flagship `git commit` note is on out of the box), and the file carries
/// only overrides:
///
/// ```toml
/// [overrides]
/// "git commit" = "my own wording"                            # replaces the builtin note
/// "cargo publish" = { note = "publishing is irreversible" }  # table form: room for v1.1 keys
/// "@write-redirect" = "off"                                       # structural rule: any write redirect
/// ```
///
/// A prefix key is whitespace-split into argv tokens (lowercased; matched
/// per simple-command segment by the classifier's parse). `@`-prefixed keys
/// are reserved structural pseudo-prefixes (`@write-redirect` fires on any
/// write redirection that is not a pure sink); any other `@`-key is
/// rejected at startup, so a reserved key never passes silently.
/// missing file means builtin rules only. A present-but-malformed file —
/// bad TOML, a typo'd top-level table, a value that is neither a note
/// string, `off`, nor a `{ note }` table, or an unknown key in the table
/// form — panics at startup (fail-closed, matching
/// [`policy_preset_from_env`](super::policy_preset_from_env)): the operator asked for hooks and would
/// silently lose them otherwise. An empty/whitespace prefix key or an empty
/// note is dropped with a warning — the prefix would match every call, the
/// note would emit nothing. Distinct raw keys that canonicalize to the same
/// prefix warn; the byte-order-last wins. Read once at tagma startup; edits
/// take effect on the next tagma start.
pub fn load_exec_hook_rules(path: &std::path::Path) -> Vec<HookRule> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => panic!("{}: cannot read exec hook rules: {e}", path.display()),
    };
    let overrides = parse_exec_hook_overrides(&raw)
        .unwrap_or_else(|e| panic!("{}: invalid exec hook rules TOML: {e}", path.display()));
    merge_exec_hook_rules(builtin_exec_hook_rules(), overrides)
}

/// The builtin preset: rules on out of the box, no file required. Kept
/// deliberately tiny — every entry fires for every install — and phrased
/// self-conditionally, naming no specific skill (paths shift; the note only
/// asks whether the relevant ones were applied). aifed is the one exception:
/// a fixed, special tool stable enough to name outright.
fn builtin_exec_hook_rules() -> Vec<HookRule> {
    vec![
        HookRule {
            tool: names::BASH_EXEC.into(),
            trigger: Trigger::Prefix(vec!["git".into(), "commit".into()]),
            phase: HookPhase::Post,
            note: "a git commit command just ran; were the relevant skills applied to the task, the change, and the commit message?".into(),
        },
        HookRule {
            tool: names::BASH_EXEC.into(),
            trigger: Trigger::WriteRedirect,
            phase: HookPhase::Post,
            note: "the system detected a possible text edit via shell redirection; if this edited an existing file, prefer editing it with aifed, and make sure the aifed skill is loaded before use. creating a new file via redirection is fine.".into(),
        },
    ]
}

/// Layer `overrides` (key = canonical prefix or the `@write-redirect`
/// pseudo-key, `None` note = `off`) over the builtin set: a matching key
/// replaces the builtin rule, a new key adds one, and `off` removes it
/// (builtin or not).
fn merge_exec_hook_rules(
    builtin: Vec<HookRule>,
    overrides: std::collections::BTreeMap<String, HookSpec>,
) -> Vec<HookRule> {
    let mut merged: std::collections::BTreeMap<String, HookSpec> = builtin
        .into_iter()
        .map(|r| (r.override_key(), HookSpec { note: Some(r.note) }))
        .collect();
    merged.extend(overrides);
    merged
        .into_iter()
        .filter_map(|(key, spec)| {
            let note = spec.note?; // `off` → removed
            let trigger = if key == WRITE_REDIRECT_KEY {
                Trigger::WriteRedirect
            } else {
                Trigger::Prefix(key.split(' ').map(str::to_owned).collect())
            };
            Some(HookRule {
                tool: names::BASH_EXEC.into(),
                trigger,
                phase: HookPhase::Post,
                note,
            })
        })
        .collect()
}

/// Parse the `[overrides]` table of `exec_hooks.toml` into canonical keys
/// (lowercased, whitespace-split, rejoined) — parsing and merging stay
/// separately testable.
fn parse_exec_hook_overrides(
    raw: &str,
) -> Result<std::collections::BTreeMap<String, HookSpec>, toml::de::Error> {
    let file: ExecHooksFile = toml::from_str(raw)?;
    // Raw keys arrive in BTreeMap byte order (toml sorts at deserialize), so a
    // canonical-key collision resolves last-in-byte-order — warn rather than
    // decide silently.
    let mut canonical_overrides = std::collections::BTreeMap::new();
    for (key, spec) in file.overrides {
        let canonical = key
            .split_whitespace()
            .map(|tok| tok.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        if canonical.is_empty() {
            tracing::warn!(
                "exec hook override dropped: empty command prefix would match every call"
            );
            continue;
        }
        // `@`-prefixed keys are reserved structural pseudo-prefixes; a
        // typo'd one would silently become a never-matching prefix rule.
        if canonical.starts_with('@') && canonical != WRITE_REDIRECT_KEY {
            return Err(<toml::de::Error as serde::de::Error>::custom(format!(
                "unknown pseudo-prefix key \"{canonical}\" (only \"{WRITE_REDIRECT_KEY}\" is reserved)"
            )));
        }
        if spec.note.as_deref() == Some("") {
            tracing::warn!("exec hook override dropped: \"{key}\" carries an empty note");
            continue;
        }
        if canonical_overrides
            .insert(canonical.clone(), spec)
            .is_some()
        {
            tracing::warn!(
                "exec hook overrides collide on canonical prefix \"{canonical}\"; the byte-order-last key wins"
            );
        }
    }
    Ok(canonical_overrides)
}

/// The exec_hooks.toml document: one [overrides] table, nothing else — a
/// typo'd top-level table ([hooks], [override]) fails the parse rather
/// than being ignored.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecHooksFile {
    #[serde(default)]
    overrides: std::collections::BTreeMap<String, HookSpec>,
}

/// One override's value: the note text (bare string), `off` (remove the
/// rule), or a `{ note }` table (room for v1.1 keys such as
/// unless_skill_loaded) — ExecOverride's dual form plus a disable
/// spelling. A table note of literal "off" is rejected: disabling is the
/// bare form's job, and ambiguity there would be fail-open.
#[derive(Debug)]
struct HookSpec {
    note: Option<String>,
}

impl<'de> serde::Deserialize<'de> for HookSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct HookSpecVisitor;

        impl<'de> serde::de::Visitor<'de> for HookSpecVisitor {
            type Value = HookSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a note string, `off`, or a table { note }")
            }

            fn visit_str<E>(self, v: &str) -> Result<HookSpec, E>
            where
                E: serde::de::Error,
            {
                // Bare `off` disables the rule (builtin included); anything
                // else is the note text.
                let note = (v != "off").then(|| v.to_owned());
                Ok(HookSpec { note })
            }

            fn visit_map<A>(self, mut map: A) -> Result<HookSpec, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut note: Option<String> = None;
                while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    match key.as_ref() {
                        "note" => {
                            // Typed: a non-string note names its actual type in
                            // the error (mirrors ExecOverride's visitor).
                            let text = map.next_value::<std::borrow::Cow<'de, str>>()?;
                            if text == "off" {
                                return Err(serde::de::Error::invalid_value(
                                    serde::de::Unexpected::Str(text.as_ref()),
                                    &"a note other than \"off\" (use the bare `off` to disable)",
                                ));
                            }
                            note = Some(text.into_owned())
                        }
                        other => return Err(serde::de::Error::unknown_field(other, &["note"])),
                    }
                }
                note.map(|note| HookSpec { note: Some(note) })
                    .ok_or_else(|| serde::de::Error::missing_field("note"))
            }
        }

        deserializer.deserialize_any(HookSpecVisitor)
    }
}

#[cfg(test)]
mod tests;
