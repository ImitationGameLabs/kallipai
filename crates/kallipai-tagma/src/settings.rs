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
    kallipai_runtime::persistence::config_dir_root().map(|d| d.join(SETTINGS_FILE))
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

// The blob stores' compression policy: each store reads its own
// settings section -- `[blob.tasks]` and `[blob.attachments]` -- and
// the section defaults differ (task archives compress at zstd-3,
// attachment mirrors store raw); `"off"` disables compression in a
// section that would otherwise frame its blobs.

/// Compression policy for the task-archive blob store, read from
/// `[blob.tasks]` in settings.toml. Default: zstd at level 3.
pub fn load_task_blob_compression() -> kallipai_blob_store::Compression {
    load_blob_section("tasks", true)
}

/// Compression policy for the attachment-mirror blob store, read from
/// `[blob.attachments]` in settings.toml. Default: off (raw bytes).
pub fn load_attachment_blob_compression() -> kallipai_blob_store::Compression {
    load_blob_section("attachments", false)
}

/// Parse one `[blob.<section>]` table. Every failure shape (no file,
/// unparsable TOML, missing section, unknown compression value) falls
/// back to the section's default; a value that is present but cannot be
/// understood (an unknown word, or not a string at all) warns and
/// falls back -- the section default is never silently guessed past
/// an explicit mistake.
fn load_blob_section(section: &str, default_on: bool) -> kallipai_blob_store::Compression {
    use kallipai_blob_store::Compression;

    let fallback = if default_on {
        Compression::Zstd { level: 3 }
    } else {
        Compression::Off
    };
    let Some(path) = settings_path().ok() else {
        return fallback;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return fallback;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&raw) else {
        return fallback;
    };
    let Some(section_table) = value.get("blob").and_then(|b| b.get(section)) else {
        return fallback;
    };
    // The compression key decides which arm runs; a section-level
    // `level` override still applies when the key is absent (that is
    // the "compression is on by default, tune the tier" shape).
    let requested = match section_table.get("compression").map(|raw| raw.as_str()) {
        None => None,
        Some(Some(s)) => Some(s),
        // Key present but not a string: same treatment as an unknown
        // string -- warn loudly, then fall back.
        Some(None) => {
            tracing::warn!(
                "settings.toml [blob.{section}] compression must be a string; using the section default"
            );
            return fallback;
        }
    };
    match requested {
        Some("off") => Compression::Off,
        Some("zstd") | None => {
            // Clamp instead of trusting the file: an out-of-range level
            // would make every ingest fail at encode time.
            let level = section_table
                .get("level")
                .and_then(|l| l.as_integer())
                .unwrap_or(3)
                .clamp(1, 22);
            Compression::Zstd {
                level: level as i32,
            }
        }
        Some(other) => {
            // A value the config doesn't name is treated as a
            // configuration mistake: warn loudly, don't silently
            // guess, and fall back to this section's default.
            tracing::warn!(
                "settings.toml [blob.{section}] compression = {other:?} is not a known value; \
                 using the section default"
            );
            fallback
        }
    }
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

    #[test]
    #[serial_test::serial]
    fn blob_compression_defaults_to_zstd_3_and_parses_off() {
        crate::test_helpers::ensure_test_data_dir();
        let path = settings_path().unwrap();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        // Default (no file): tasks compress, attachments stay raw.
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            load_task_blob_compression(),
            kallipai_blob_store::Compression::Zstd { level: 3 }
        );
        assert_eq!(
            load_attachment_blob_compression(),
            kallipai_blob_store::Compression::Off
        );
        // Explicit off for tasks, explicit zstd for attachments: each
        // section decides for its own store.
        std::fs::write(
            &path,
            "[blob.tasks]\ncompression = \"off\"\n[blob.attachments]\ncompression = \"zstd\"\n",
        )
        .unwrap();
        assert_eq!(
            load_task_blob_compression(),
            kallipai_blob_store::Compression::Off
        );
        assert_eq!(
            load_attachment_blob_compression(),
            kallipai_blob_store::Compression::Zstd { level: 3 }
        );
        // Level overrides are clamped on both ends (1..=22): an
        // out-of-range level would make every ingest fail at encode
        // time, so the loader repairs it instead.
        std::fs::write(&path, "[blob.tasks]\nlevel = 99\n").unwrap();
        assert_eq!(
            load_task_blob_compression(),
            kallipai_blob_store::Compression::Zstd { level: 22 }
        );
        std::fs::write(&path, "[blob.tasks]\nlevel = 0\n").unwrap();
        assert_eq!(
            load_task_blob_compression(),
            kallipai_blob_store::Compression::Zstd { level: 1 }
        );
        std::fs::write(&path, "[blob.tasks]\nlevel = -7\n").unwrap();
        assert_eq!(
            load_task_blob_compression(),
            kallipai_blob_store::Compression::Zstd { level: 1 }
        );
        // Unknown value in a section: that section's default, never a panic.
        std::fs::write(&path, "[blob.attachments]\ncompression = \"snappy\"\n").unwrap();
        assert_eq!(
            load_attachment_blob_compression(),
            kallipai_blob_store::Compression::Off
        );
        // A present-but-non-string value warns and falls back the same
        // way (a bare boolean is the plausible typo).
        std::fs::write(&path, "[blob.attachments]\ncompression = true\n").unwrap();
        assert_eq!(
            load_attachment_blob_compression(),
            kallipai_blob_store::Compression::Off
        );
        // Garbage in the file: safe defaults, never a panic.
        std::fs::write(&path, "not toml at all [[[").unwrap();
        assert_eq!(
            load_task_blob_compression(),
            kallipai_blob_store::Compression::Zstd { level: 3 }
        );
        assert_eq!(
            load_attachment_blob_compression(),
            kallipai_blob_store::Compression::Off
        );
        // Restore the timezone settings the other tests in this module pin.
        let _ = std::fs::remove_file(&path);
    }
}
