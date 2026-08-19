//! Environment-knob resolvers for tagma startup.
//!
//! Two small readers — the tagma-global policy preset and the root agent's
//! permission class — both following the env-knob convention: an unrecognized
//! value is a fatal misconfiguration and panics. Re-exported by `crate::config`.

use super::permissions::PermissionClass;
use kallip_common::policy::PolicyPreset;

/// Resolve the tagma-global `bash_exec` classify preset from
/// `KALLIP_POLICY_PRESET`.
///
/// Unset or empty → [`PolicyPreset::Default`] (strict). Accepts `default`, `auto`,
/// and `allow-all`. An unrecognized value is a fatal misconfiguration (the preset
/// is structural to the sandbox), so it panics — matching the env-knob convention
/// of [`permission_class_from_env`]. Only read once at tagma startup; the preset
/// is immutable for the tagma's lifetime.
pub fn policy_preset_from_env() -> PolicyPreset {
    let Ok(raw) = std::env::var("KALLIP_POLICY_PRESET") else {
        return PolicyPreset::Default;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return PolicyPreset::Default;
    }
    raw.parse::<PolicyPreset>().unwrap_or_else(|e| {
        panic!("KALLIP_POLICY_PRESET: {e}");
    })
}
/// Resolve the root agent's permission class from `KALLIP_ROOT_AGENT_PERMISSION_CLASS`.
///
/// Root-only test knob, parallel to [`policy_preset_from_env`]: read in the
/// tagma's root-create branch, never on the subagent or restore paths
/// (subagents derive their class from `ceiling_for_tier`; restore uses the
/// persisted `meta.json`). Accepts lowercase `"normal"` / `"guest"` — the env-var
/// convention, distinct from the PascalCase serde form persisted in `meta.json`.
/// Panics on an invalid value, matching [`policy_preset_from_env`]'s misconfig behavior.
pub fn permission_class_from_env() -> PermissionClass {
    let Ok(raw) = std::env::var("KALLIP_ROOT_AGENT_PERMISSION_CLASS") else {
        return PermissionClass::default();
    };
    // Trim here, not inside FromStr: the wire/env convention trims surrounding
    // whitespace, but FromStr stays trim-free so the tagma rejects untrimmed
    // client input verbatim.
    let raw = raw.trim();
    match raw.parse::<PermissionClass>() {
        Ok(class) => class,
        Err(_) => panic!(
            "KALLIP_ROOT_AGENT_PERMISSION_CLASS: invalid permission class '{raw}' (expected normal or guest)"
        ),
    }
}
