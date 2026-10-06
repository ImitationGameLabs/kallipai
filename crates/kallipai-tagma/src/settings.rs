//! Runtime settings persisted in the instance config directory.
//!
//! One TOML document at the XDG config instance root
//! (`config_dir_root()`/`settings.toml`, owner-only), currently carrying
//! the display timezone. Agents and CLI renders read it through
//! `GET /settings/timezone`; the operator writes it through
//! `PUT /settings/timezone`.

use crate::profile_source::ProfileSourceKind;
use anyhow::Context;
use std::path::PathBuf;

const SETTINGS_FILE: &str = "settings.toml";

pub(crate) fn settings_path() -> anyhow::Result<PathBuf> {
    kallipai_adk::persistence::config_dir_root().map(|d| d.join(SETTINGS_FILE))
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

/// Rewrite one settings key, preserving every other key and section in
/// the file. The current document is parsed first — a hand-edited syntax
/// error aborts the write instead of being silently rebuilt away — then
/// the new value is staged through a same-directory temp file (fsync +
/// re-parse round-trip check) and committed by atomic rename, following
/// the blob-store rewrite discipline.
///
/// `key_path` names nested tables (e.g. `["profiles", "source", "mode"]`
/// for `[profiles.source] mode`); `None` removes the key. Intermediates
/// are created as tables when absent.
fn write_preserving(key_path: &[&str], value: Option<&toml::Value>) -> anyhow::Result<()> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut doc: toml::Value = match std::fs::read_to_string(&path) {
        Ok(raw) => toml::from_str(&raw).with_context(|| {
            format!(
                "parsing {} before a settings write; fix the file by hand — a syntax error must not be rebuilt away",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(Default::default())
        }
        Err(e) => return Err(anyhow::Error::new(e)).with_context(|| {
            format!("reading {} for a settings write", path.display())
        }),
    };
    let (last, parents) = key_path
        .split_last()
        .expect("a settings key path is never empty");
    let mut cursor = &mut doc;
    for segment in parents {
        let entry = cursor
            .as_table_mut()
            .expect("the cursor is always a table")
            .entry((*segment).to_owned())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        if !entry.is_table() {
            anyhow::bail!(
                "settings key path collides with a non-table value at '{}'",
                segment
            );
        }
        cursor = entry;
    }
    let table = cursor.as_table_mut().expect("the cursor is always a table");
    match value {
        Some(v) => {
            table.insert((*last).to_owned(), v.clone());
        }
        None => {
            table.remove(*last);
        }
    }
    let body = toml::to_string_pretty(&doc)?;
    let staged = path
        .parent()
        .expect("settings_path always has a parent")
        .join(format!("{}.tmp{}", SETTINGS_FILE, std::process::id()));
    if let Err(error) = stage_and_verify(&staged, &body, key_path, value) {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    std::fs::rename(&staged, &path).with_context(|| format!("committing {}", path.display()))?;
    // The rename's directory entry is what a power loss could steal;
    // syncing the directory makes the commit durable (rewrite.rs).
    if let Some(dir) = path.parent().and_then(|p| std::fs::File::open(p).ok()) {
        let _ = dir.sync_all();
    }
    crate::credentials::set_owner_only(&path)?;
    Ok(())
}
/// Write the staged bytes, fsync them, and re-parse them as the
/// round-trip proof: what lands on disk must be valid TOML carrying
/// exactly the intended key before the commit rename (the rewrite.rs
/// discipline applied to a settings document).
fn stage_and_verify(
    staged: &std::path::Path,
    body: &str,
    key_path: &[&str],
    value: Option<&toml::Value>,
) -> anyhow::Result<()> {
    std::fs::write(staged, body).with_context(|| format!("staging {}", staged.display()))?;
    std::fs::File::open(staged)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("fsyncing {}", staged.display()))?;
    let written = std::fs::read_to_string(staged)?;
    let reparsed: toml::Value = toml::from_str(&written)
        .with_context(|| format!("re-parsing staged {} failed", staged.display()))?;
    let (last, parents) = key_path
        .split_last()
        .expect("a settings key path is never empty");
    let mut cursor = &reparsed;
    for segment in parents {
        cursor = cursor.get(*segment).ok_or_else(|| {
            anyhow::anyhow!(
                "staged round-trip lost table '{}' — refusing to commit",
                segment
            )
        })?;
    }
    match value {
        Some(want) => {
            let got = cursor.get(*last).ok_or_else(|| {
                anyhow::anyhow!("staged round-trip lost key '{}' — refusing to commit", last)
            })?;
            if got != want {
                anyhow::bail!(
                    "staged round-trip mismatch for key '{}': set {:?}, re-read {:?}",
                    last,
                    want,
                    got
                );
            }
        }
        None => {
            if cursor.get(*last).is_some() {
                anyhow::bail!(
                    "staged round-trip still carries key '{}' after removal — refusing to commit",
                    last
                );
            }
        }
    }
    Ok(())
}

/// Persist the configured IANA timezone name; `None` removes the key.
/// The write preserves every other section (the blob compression
/// tables survive a timezone change).
pub fn save_timezone(name: Option<&str>) -> anyhow::Result<()> {
    let value = name.map(toml::Value::from);
    write_preserving(&["timezone"], value.as_ref())
}

/// The profile source mode from `[profiles.source].mode`. An absent key
/// (or file) means local, the default; a present but unknown
/// spelling is a hard error — the mode decides which face serves
/// profiles, so guessing past a typo could silently flip the tagma's
/// shape.
pub fn load_profile_source_mode() -> anyhow::Result<ProfileSourceKind> {
    Ok(explicit_profile_source_mode()?.unwrap_or(ProfileSourceKind::Local))
}

/// The explicit `[profiles.source].mode` value, `None` when the file,
/// the table, or the key is absent. The shared read under the pure-toml
/// loader above and the boot resolver below.
fn explicit_profile_source_mode() -> anyhow::Result<Option<ProfileSourceKind>> {
    let path = settings_path()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e)).with_context(|| {
                format!("reading {} for the profile source mode", path.display())
            });
        }
    };
    let value: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("parsing {} for the profile source mode", path.display()))?;
    let mode = value
        .get("profiles")
        .and_then(|p| p.get("source"))
        .and_then(|s| s.get("mode"));
    match mode {
        None => Ok(None),
        Some(toml::Value::String(word)) => {
            ProfileSourceKind::from_wire_str(word).map(Some).ok_or_else(|| {
                anyhow::anyhow!(
                    "settings.toml [profiles.source].mode = {word:?} is not a known value (expected \"local\" or \"model-gateway\")"
                )
            })
        }
        Some(_) => anyhow::bail!("settings.toml [profiles.source].mode must be a string"),
    }
}

/// The spawn-time env seed for the profile source mode: a one-click
/// instance spawn sets it so the spawned tagma boots against the model
/// gateway without anyone editing settings.toml. It is a seed, not a
/// control — an explicit settings.toml mode always wins, so a later
/// runtime switch (which writes the explicit key) cannot be flipped
/// back by the stale env a restarted process still carries.
const ENV_PROFILE_SOURCE: &str = "KALLIPAI_TAGMA_PROFILE_SOURCE";

/// The profile source mode a boot resolves: the explicit settings.toml
/// key above, else the spawn env seed (same two spellings, same hard
/// error on a typo), else local. [`load_profile_source_mode`] stays
/// pure-toml for its runtime callers — the switch face reads the
/// persisted record, not the seed.
pub fn resolve_boot_profile_source_mode() -> anyhow::Result<ProfileSourceKind> {
    if let Some(kind) = explicit_profile_source_mode()? {
        return Ok(kind);
    }
    match std::env::var(ENV_PROFILE_SOURCE) {
        Ok(word) => ProfileSourceKind::from_wire_str(&word).ok_or_else(|| {
            anyhow::anyhow!(
                "{ENV_PROFILE_SOURCE} = {word:?} is not a known value (expected \"local\" or \"model-gateway\")"
            )
        }),
        Err(std::env::VarError::NotPresent) => Ok(ProfileSourceKind::Local),
        Err(_) => anyhow::bail!("{ENV_PROFILE_SOURCE} is not valid UTF-8"),
    }
}

/// Persist the profile source mode to `[profiles.source].mode`,
/// preserving every other settings section.
pub fn save_profile_source_mode(kind: ProfileSourceKind) -> anyhow::Result<()> {
    let value = toml::Value::from(kind.as_wire_str());
    write_preserving(&["profiles", "source", "mode"], Some(&value))
}

/// Read the `[profiles.source]` table from settings.toml, or `None`
/// when the file or the table is absent. Shared by the polis and
/// collection loaders; the mode loader predates it and keeps its own
/// inlined read.
fn profile_source_section() -> anyhow::Result<Option<toml::value::Table>> {
    let path = settings_path()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e)).with_context(|| {
                format!("reading {} for the profile source settings", path.display())
            });
        }
    };
    let value: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("parsing {} for the profile source settings", path.display()))?;
    Ok(value
        .get("profiles")
        .and_then(|p| p.get("source"))
        .and_then(|s| s.as_table())
        .cloned())
}

/// The pinned platform origin from `[profiles.source].polis`: the
/// relay entry whose model gateway a model-gateway boot targets.
/// Absent means not yet pinned — the boot then defaults to the first
/// entry carrying a stored enrollment token, and the first switch
/// rewrites this key. Only meaningful for model-gateway boots; a
/// local switch keeps it (mode and pin stay decoupled), so a later
/// gateway boot or polis-less switch resolves the recorded
/// platform.
pub fn load_profile_source_polis() -> anyhow::Result<Option<String>> {
    match profile_source_section()?.and_then(|t| t.get("polis").cloned()) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s)),
        Some(_) => anyhow::bail!("settings.toml [profiles.source].polis must be a string"),
    }
}

/// Persist the pinned platform origin (or remove the key when `None`),
/// preserving every other settings section.
pub fn save_profile_source_polis(origin: Option<&str>) -> anyhow::Result<()> {
    let value = origin.map(toml::Value::from);
    write_preserving(&["profiles", "source", "polis"], value.as_ref())
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

    /// The profile-source mode round-trips through the preserving
    /// write, and unknown spellings are loud errors instead of
    /// silent defaults.
    #[test]
    #[serial_test::serial]
    fn profile_source_mode_round_trips_and_rejects_unknown_words() {
        crate::test_helpers::ensure_test_data_dir();
        let path = settings_path().unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            load_profile_source_mode().unwrap(),
            ProfileSourceKind::Local
        );
        save_profile_source_mode(ProfileSourceKind::Proxy).unwrap();
        assert_eq!(
            load_profile_source_mode().unwrap(),
            ProfileSourceKind::Proxy
        );
        save_profile_source_mode(ProfileSourceKind::Local).unwrap();
        assert_eq!(
            load_profile_source_mode().unwrap(),
            ProfileSourceKind::Local
        );
        // The settings face accepts exactly two spellings (local,
        // model-gateway); anything else is a boot-stopping error,
        // not a default.
        std::fs::write(&path, "[profiles.source]\nmode = \"proxy\"\n").unwrap();
        assert!(load_profile_source_mode().is_err());
        std::fs::write(&path, "[profiles.source]\nmode = 7\n").unwrap();
        assert!(load_profile_source_mode().is_err());
        let _ = std::fs::remove_file(&path);
    }

    /// The boot resolver precedence: an explicit settings.toml mode
    /// beats the spawn env seed, the seed applies only when the file
    /// carries no mode, both absent boots local, and a typo'd seed is a
    /// boot-stopping error (the same semantics as a typo'd toml key).
    #[test]
    #[serial_test::serial]
    fn boot_resolver_prefers_explicit_toml_over_the_env_seed() {
        crate::test_helpers::ensure_test_data_dir();
        let path = settings_path().unwrap();
        let _ = std::fs::remove_file(&path);
        // Both absent: local, the default.
        temp_env::with_vars_unset([ENV_PROFILE_SOURCE], || {
            assert_eq!(
                resolve_boot_profile_source_mode().unwrap(),
                ProfileSourceKind::Local
            );
        });
        // The seed applies when the file carries no mode.
        temp_env::with_vars([(ENV_PROFILE_SOURCE, Some("model-gateway"))], || {
            assert_eq!(
                resolve_boot_profile_source_mode().unwrap(),
                ProfileSourceKind::Proxy
            );
        });
        // A typo'd seed is a boot-stopping error, not a silent default.
        temp_env::with_vars([(ENV_PROFILE_SOURCE, Some("gateway"))], || {
            let err = resolve_boot_profile_source_mode().unwrap_err();
            assert!(
                format!("{err:#}").contains("not a known value"),
                "got: {err:#}"
            );
        });
        // An explicit settings.toml mode beats a contradicting seed:
        // after a runtime switch wrote the record, a stale spawn env
        // cannot flip the mode back on restart.
        save_profile_source_mode(ProfileSourceKind::Local).unwrap();
        temp_env::with_vars([(ENV_PROFILE_SOURCE, Some("model-gateway"))], || {
            assert_eq!(
                resolve_boot_profile_source_mode().unwrap(),
                ProfileSourceKind::Local
            );
        });
        save_profile_source_mode(ProfileSourceKind::Proxy).unwrap();
        temp_env::with_vars([(ENV_PROFILE_SOURCE, Some("local"))], || {
            assert_eq!(
                resolve_boot_profile_source_mode().unwrap(),
                ProfileSourceKind::Proxy
            );
        });
        let _ = std::fs::remove_file(&path);
    }

    /// The polis pin round-trips through the preserving write
    /// (absent = None, a value stays a value, removal returns to
    /// absent). The retired collections key, when a stale
    /// settings.toml still carries it, is simply ignored.
    #[test]
    #[serial_test::serial]
    fn profile_source_polis_round_trip() {
        crate::test_helpers::ensure_test_data_dir();
        let path = settings_path().unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_profile_source_polis().unwrap(), None);
        save_profile_source_polis(Some("https://api.example.com")).unwrap();
        assert_eq!(
            load_profile_source_polis().unwrap().as_deref(),
            Some("https://api.example.com")
        );
        // Removal returns to absent (the local-switch pin clear).
        save_profile_source_polis(None).unwrap();
        assert_eq!(load_profile_source_polis().unwrap(), None);
        // A stale collections key from an older tagma is inert: no
        // reader consults it.
        std::fs::write(&path, "[profiles.source]\ncollections = [\"alpha\"]\n").unwrap();
        assert_eq!(load_profile_source_polis().unwrap().as_deref(), None);
        let _ = std::fs::remove_file(&path);
    }

    /// A preserving write keeps every other section: switching the
    /// profile source must not eat the timezone or the blob tables,
    /// and a timezone write must not eat them either.
    #[test]
    #[serial_test::serial]
    fn preserving_writes_keep_unrelated_sections() {
        crate::test_helpers::ensure_test_data_dir();
        let path = settings_path().unwrap();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            &path,
            "timezone = \"UTC\"\n[blob.tasks]\ncompression = \"off\"\n[profiles.source]\nmode = \"local\"\n",
        )
        .unwrap();
        save_profile_source_mode(ProfileSourceKind::Proxy).unwrap();
        assert_eq!(load_timezone().as_deref(), Some("UTC"));
        assert_eq!(
            load_task_blob_compression(),
            kallipai_blob_store::Compression::Off
        );
        assert_eq!(
            load_profile_source_mode().unwrap(),
            ProfileSourceKind::Proxy
        );
        save_timezone(None).unwrap();
        assert_eq!(load_timezone(), None);
        assert_eq!(
            load_task_blob_compression(),
            kallipai_blob_store::Compression::Off
        );
        assert_eq!(
            load_profile_source_mode().unwrap(),
            ProfileSourceKind::Proxy
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A hand-edited syntax error must stop the write: the file is left
    /// exactly as it was, never rebuilt from a partial view.
    #[test]
    #[serial_test::serial]
    fn write_refuses_to_rebuild_an_unparsable_document() {
        crate::test_helpers::ensure_test_data_dir();
        let path = settings_path().unwrap();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let broken = "timezone = \"UTC\" not toml [[[";
        std::fs::write(&path, broken).unwrap();
        assert!(save_profile_source_mode(ProfileSourceKind::Proxy).is_err());
        assert!(save_timezone(Some("Asia/Shanghai")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        let _ = std::fs::remove_file(&path);
    }
}
