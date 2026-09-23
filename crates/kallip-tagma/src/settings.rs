//! Runtime settings persisted in the instance config directory.
//!
//! One TOML document at the XDG config instance root
//! (`config_dir_root()`/`settings.toml`, owner-only), currently carrying
//! the display timezone. Agents and CLI renders read it through
//! `GET /settings/timezone`; the operator writes it through
//! `PUT /settings/timezone`.

use anyhow::Context;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
struct SettingsDoc<'a> {
    timezone: &'a str,
}

const SETTINGS_FILE: &str = "settings.toml";

fn settings_path() -> anyhow::Result<PathBuf> {
    kallip_runtime::persistence::config_dir_root().map(|d| d.join(SETTINGS_FILE))
}

/// The configured IANA timezone name, or `None` when unset or when the
/// file cannot be read (callers degrade to the machine-local zone).
pub fn load_timezone() -> Option<String> {
    let raw = std::fs::read_to_string(settings_path().ok()?).ok()?;
    let value: toml::Value = toml::from_str(&raw).ok()?;
    value
        .get("timezone")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Whether `name` resolves as an IANA timezone. The write path refuses
/// anything else so a typo cannot silently degrade every rendering
/// surface to UTC.
pub fn timezone_name_valid(name: &str) -> bool {
    jiff::tz::TimeZone::get(name).is_ok()
}

/// Persist the configured IANA timezone name; `None` encodes an unset
/// key (an empty document). TOML serialization escapes every string
/// safely, so an arbitrary PUT body cannot produce a malformed file.
pub fn save_timezone(name: Option<&str>) -> anyhow::Result<()> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = match name {
        Some(n) => toml::to_string_pretty(&SettingsDoc { timezone: n })?,
        None => String::new(),
    };
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    crate::credentials::set_owner_only(&path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip through the pinned config root: the same serial-guarded
    /// fixture pattern as the guard tests in `test_helpers`.
    #[test]
    #[serial_test::serial]
    fn timezone_setting_round_trips_through_disk() {
        crate::test_helpers::ensure_test_data_dir();
        save_timezone(Some("Asia/Shanghai")).unwrap();
        assert_eq!(load_timezone().as_deref(), Some("Asia/Shanghai"));
        save_timezone(None).unwrap();
        assert_eq!(load_timezone(), None);
    }

    #[test]
    fn timezone_name_validation_rejects_typos() {
        assert!(timezone_name_valid("Asia/Shanghai"));
        assert!(timezone_name_valid("America/New_York"));
        assert!(timezone_name_valid("UTC"));
        assert!(!timezone_name_valid("Mars/Olympus"));
        assert!(!timezone_name_valid(""));
    }
}
