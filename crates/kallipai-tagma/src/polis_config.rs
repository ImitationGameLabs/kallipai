//! The tagma's own `polis.toml`: the relay entries this host serves, and the
//! one writer that changes them.
//!
//! Boot reads the document once (`resolve_relay_entries` in `lib.rs`); the
//! `/polis` routes read and rewrite it through this module. One parser and one
//! writer means the file the operator owns cannot be read by a rule that the
//! writer does not also enforce. Two properties hold on every write:
//!
//! - the write is atomic. The new document is staged in a same-directory temp
//!   file, fsynced, re-parsed as the round-trip proof, and committed by an
//!   atomic rename (see `settings::write_verified`), so a failure anywhere
//!   leaves the previous bytes in place and no reader ever sees a
//!   half-written file;
//! - an `enrollment_code` is written back for every entry a call did not
//!   spend. The code an entry carries is that entry's next chance to attach
//!   (a first-run boot that never reached the platform leaves one behind), so
//!   only the entry a call actually redeemed has its code cleared; carrying
//!   a spent code forward would recreate the stale-code shape the boot warns
//!   about (`EnrollEntry::StoredIgnoringCode`).
//!
//! Writes are serialized by one process-wide mutex: the read-modify-write of
//! `add` and `remove` runs under it, so two requests cannot each read the old
//! document and lose one another's entry.

use std::path::{Path, PathBuf};

use crate::RelayEntry;

/// The file, under the instance data root, that declares the relay entries.
const POLIS_FILE: &str = "polis.toml";

/// The longest entry name, in bytes: it becomes a directory name under the
/// credentials root, so it stays inside one filesystem component.
pub(crate) const MAX_ENTRY_NAME: usize = 64;

/// Why a read or a write failed, in the terms the route that asked reports:
/// `Invalid` is the request's fault, `Broken` is the document on disk.
#[derive(Debug)]
pub(crate) enum PolisConfigError {
    /// The request named something the entry rules refuse: a name outside the
    /// slug grammar, or an origin that is blank once trimmed.
    Invalid(String),
    /// The document could not be read, parsed, validated or committed.
    Broken(String),
    /// The document is fine; removing it failed. Its own variant because the
    /// two facts call for different actions: a broken file needs an edit, a
    /// file that would not go away needs a look at the directory (and this one
    /// names what state the caller is left in).
    NotRemoved(String),
}

impl std::fmt::Display for PolisConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Broken(message) | Self::NotRemoved(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for PolisConfigError {}

/// The `[[polis]]` array of `polis.toml`.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct PolisFile {
    #[serde(default)]
    polis: Vec<PolisTomlEntry>,
}

/// One entry as the file holds it. `enrollment_code` is a pending code: read
/// by the boot and by `add`, written back unless the writing call spent it.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct PolisTomlEntry {
    name: String,
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enrollment_code: Option<String>,
}

/// `<data_root>/polis.toml`.
pub(crate) fn polis_path() -> anyhow::Result<PathBuf> {
    crate::data_root().map(|root| root.join(POLIS_FILE))
}

/// A relay name must be a DNS-label-like slug (`[a-z0-9][a-z0-9-]*`, at most
/// [`MAX_ENTRY_NAME`] bytes): it is a path component (`credentials/<name>/`)
/// and a log key, so it stays flat, lowercase, unambiguous, and inside the
/// filesystem's component limit.
pub(crate) fn valid_entry_name(name: &str) -> bool {
    name.len() <= MAX_ENTRY_NAME && {
        let mut chars = name.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }
}

/// Why `name` cannot be an entry name, if it cannot. Both faces call this, so a
/// name over the limit and a name outside the grammar are two sentences with
/// one wording, whichever face refuses.
fn name_error(name: &str) -> Option<String> {
    if name.len() > MAX_ENTRY_NAME {
        return Some(format!(
            "entry name {name:?} is {} bytes: a name must be at most \
             {MAX_ENTRY_NAME} bytes (it is a credentials/<name>/ path component)",
            name.len()
        ));
    }
    if !valid_entry_name(name) {
        return Some(format!(
            "entry name {name:?} is not a slug matching [a-z0-9][a-z0-9-]* \
             (it is a credentials/<name>/ path component)"
        ));
    }
    None
}

/// One entry's name and origin as a request brought them, checked against the
/// same rules the file is: a slug name, and an origin `polis_origin` accepts.
/// The route validates before it makes a network call, so a typo costs no
/// enrollment.
pub(crate) fn check_entry(name: &str, url: &str) -> Result<(String, String), PolisConfigError> {
    if let Some(message) = name_error(name) {
        return Err(PolisConfigError::Invalid(message));
    }
    let origin = kallipai_common::polis::polis_origin(Some(url.to_owned()))
        .map_err(|error| PolisConfigError::Invalid(format!("{error}")))?;
    Ok((name.to_owned(), origin))
}

/// The entries `path` declares, validated exactly as the boot validates them:
/// every name a slug, no name twice, and every url normalized by
/// `polis_origin`. No file is no entries. A file that cannot be read, parsed
/// or validated is `Broken` rather than silently repaired: the boot refuses
/// the same shapes, and a rewrite must not paper over one.
pub(crate) fn read_entries_at(path: &Path) -> Result<Vec<RelayEntry>, PolisConfigError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(PolisConfigError::Broken(format!(
                "read {}: {error}",
                path.display()
            )));
        }
    };
    let file: PolisFile = toml::from_str(&raw)
        .map_err(|error| PolisConfigError::Broken(format!("parse {}: {error}", path.display())))?;
    validated(file.polis)
}

/// The entries of this instance's `polis.toml` (see [`read_entries_at`]).
pub(crate) fn read_entries() -> Result<Vec<RelayEntry>, PolisConfigError> {
    let path = polis_path().map_err(|error| PolisConfigError::Broken(error.to_string()))?;
    read_entries_at(&path)
}

/// Validate a parsed document into boot entries: the rules the boot fail-fast
/// applies, with the same wording, so a route and a restart agree about what
/// the file means.
fn validated(entries: Vec<PolisTomlEntry>) -> Result<Vec<RelayEntry>, PolisConfigError> {
    let mut seen = std::collections::HashSet::new();
    let mut validated = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(message) = name_error(&entry.name) {
            return Err(PolisConfigError::Broken(format!(
                "relay name from the file: {message}"
            )));
        }
        if !seen.insert(entry.name.clone()) {
            return Err(PolisConfigError::Broken(format!(
                "relay name {:?} repeats",
                entry.name
            )));
        }
        let polis_url = kallipai_common::polis::polis_origin(Some(entry.url))
            .map_err(|error| PolisConfigError::Broken(error.to_string()))?;
        validated.push(RelayEntry {
            name: entry.name,
            polis_url,
            enrollment_code: entry.enrollment_code,
        });
    }
    Ok(validated)
}

/// Add one entry, or replace the origin of the entry already carrying `name`
/// (the entry keeps its position). Returns the document as it now sits on
/// disk, which is what the caller reports.
pub(crate) fn add(name: &str, url: &str) -> Result<Vec<RelayEntry>, PolisConfigError> {
    let path = polis_path().map_err(|error| PolisConfigError::Broken(error.to_string()))?;
    add_at(&path, name, url)
}

/// [`add`] against an explicit document path.
pub(crate) fn add_at(
    path: &Path,
    name: &str,
    url: &str,
) -> Result<Vec<RelayEntry>, PolisConfigError> {
    let (name, polis_url) = check_entry(name, url)?;
    let _guard = write_lock();
    let mut entries = read_entries_at(path)?;
    match entries.iter_mut().find(|entry| *entry.name == name) {
        Some(existing) => {
            existing.polis_url = polis_url;
            existing.enrollment_code = None;
        }
        None => entries.push(RelayEntry {
            name,
            polis_url,
            enrollment_code: None,
        }),
    }
    write_entries_at(path, &entries)?;
    Ok(entries)
}

/// Remove the entry carrying `name`; `false` when the document does not have
/// it (nothing is written then).
pub(crate) fn remove(name: &str) -> Result<bool, PolisConfigError> {
    let path = polis_path().map_err(|error| PolisConfigError::Broken(error.to_string()))?;
    remove_at(&path, name)
}

/// [`remove`] against an explicit document path. Removing the last entry
/// deletes the file instead of committing an empty document: the boot reads
/// an empty `[[polis]]` array as a configuration error and a missing file as
/// a local-only instance, so leaving the empty form behind would make the
/// next start fail.
pub(crate) fn remove_at(path: &Path, name: &str) -> Result<bool, PolisConfigError> {
    let _guard = write_lock();
    let entries = read_entries_at(path)?;
    if !entries.iter().any(|entry| entry.name == name) {
        return Ok(false);
    }
    let kept: Vec<RelayEntry> = entries
        .into_iter()
        .filter(|entry| entry.name != name)
        .collect();
    if kept.is_empty() {
        // Zero entries and no file mean the same thing to the boot.
        std::fs::remove_file(path).map_err(|error| {
            PolisConfigError::NotRemoved(format!("{}: {error}", path.display()))
        })?;
        crate::settings::sync_parent_dir(path);
        return Ok(true);
    }
    write_entries_at(path, &kept)?;
    Ok(true)
}

/// The process-wide write lock: one read-modify-write of the document at a
/// time. Poisoning is ignored (a panicking writer left the file committed or
/// untouched, never half-written) so one bad request cannot wedge the route.
fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

/// Commit `entries` as the whole document, in their order, keeping each entry's
/// pending enrollment code. The body is staged and re-parsed before the rename;
/// the re-parse must name exactly these entries at exactly these origins, or
/// nothing is committed.
fn write_entries_at(path: &Path, entries: &[RelayEntry]) -> Result<(), PolisConfigError> {
    let document = PolisFile {
        polis: entries
            .iter()
            .map(|entry| PolisTomlEntry {
                name: entry.name.clone(),
                url: entry.polis_url.clone(),
                enrollment_code: entry.enrollment_code.clone(),
            })
            .collect(),
    };
    let body = toml::to_string_pretty(&document).map_err(|error| {
        PolisConfigError::Broken(format!("serialize {}: {error}", path.display()))
    })?;
    let want: Vec<(String, String)> = entries
        .iter()
        .map(|entry| (entry.name.clone(), entry.polis_url.clone()))
        .collect();
    crate::settings::write_verified(path, &body, |written| {
        let reparsed: PolisFile = toml::from_str(written).map_err(|error| {
            anyhow::anyhow!("the staged {} does not parse back: {error}", path.display())
        })?;
        let got: Vec<(String, String)> = reparsed
            .polis
            .iter()
            .map(|entry| (entry.name.clone(), entry.url.clone()))
            .collect();
        anyhow::ensure!(
            got == want,
            "the staged {} names different entries than were meant to be written; refusing to commit",
            path.display()
        );
        Ok(())
    })
    .map_err(|error| PolisConfigError::Broken(format!("{error:#}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(entries: &[(&str, &str)]) -> String {
        entries
            .iter()
            .map(|(name, url)| format!("[[polis]]\nname = \"{name}\"\nurl = \"{url}\"\n"))
            .collect()
    }

    #[test]
    fn the_document_rejects_the_old_relay_table_name() {
        // Rename guard: a file kept from the old relays.toml shape must fail
        // loudly naming the rename point, not parse as an empty config
        // (deny_unknown_fields at both levels).
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        std::fs::write(
            &path,
            "[[relay]]\nname = \"main\"\nurl = \"https://api.example.com\"\n",
        )
        .unwrap();
        let error = read_entries_at(&path).unwrap_err();
        assert!(
            format!("{error}").contains("unknown field `relay`"),
            "{error}"
        );
    }
    #[test]
    fn an_unknown_field_inside_an_entry_is_refused() {
        // The entry table is deny_unknown_fields too: a typo in a key name
        // must not be silently dropped while the entry keeps working.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        std::fs::write(
            &path,
            "[[polis]]\nname = \"main\"\nurl = \"https://api.example.test\"\ncod = \"x\"\n",
        )
        .unwrap();
        let error = read_entries_at(&path).unwrap_err();
        assert!(format!("{error}").contains("unknown field"), "{error}");
    }

    #[test]
    fn a_name_over_the_limit_is_refused_before_any_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        let long = "a".repeat(MAX_ENTRY_NAME + 1);
        let error = add_at(&path, &long, "https://api.example.test").unwrap_err();
        assert!(format!("{error}").contains("at most"), "{error}");
        assert!(!path.exists(), "nothing was written");
    }

    #[test]
    fn removing_the_last_entry_removes_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        add_at(&path, "alpha", "https://one.example.test").unwrap();
        assert!(remove_at(&path, "alpha").unwrap());
        assert!(!path.exists(), "a zero-entry document is a boot error");
        assert!(read_entries_at(&path).unwrap().is_empty());
    }

    #[test]
    fn a_first_add_creates_the_document_without_any_code() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        let entries = add_at(&path, "alpha", "https://api.example.test/").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].polis_url, "https://api.example.test");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("name = \"alpha\""));
        assert!(
            !raw.contains("enrollment_code"),
            "the writer must never emit an enrollment code: {raw}"
        );
    }

    #[test]
    fn a_second_add_for_the_same_name_rebinds_the_origin_in_place() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        add_at(&path, "alpha", "https://one.example.test").unwrap();
        add_at(&path, "beta", "https://two.example.test").unwrap();
        let entries = add_at(&path, "alpha", "https://three.example.test").unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"], "the entry keeps its position");
        assert_eq!(entries[0].polis_url, "https://three.example.test");
        let reread = read_entries_at(&path).unwrap();
        assert_eq!(reread, entries, "the disk carries what the call reports");
    }

    #[test]
    fn a_rewrite_keeps_siblings_and_their_pending_codes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        let mut raw = document(&[
            ("alpha", "https://one.example.test"),
            ("beta", "https://two.example.test"),
        ]);
        raw.push_str("[[polis]]\nname = \"gamma\"\nurl = \"https://three.example.test\"\nenrollment_code = \"pending\"\n");
        std::fs::write(&path, raw).unwrap();
        let entries = add_at(&path, "delta", "https://four.example.test").unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "gamma", "delta"]);
        assert_eq!(entries[2].polis_url, "https://three.example.test");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("enrollment_code = \"pending\""),
            "an untouched entry keeps its unused code: {raw}"
        );
        // The entry a call touches is the one whose code that call spent.
        let entries = add_at(&path, "gamma", "https://five.example.test").unwrap();
        assert_eq!(entries[2].enrollment_code, None);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("pending"), "{raw}");
    }

    #[test]
    fn a_remove_drops_one_entry_and_keeps_the_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        std::fs::write(
            &path,
            document(&[
                ("alpha", "https://one.example.test"),
                ("beta", "https://two.example.test"),
                ("gamma", "https://three.example.test"),
            ]),
        )
        .unwrap();
        assert!(remove_at(&path, "beta").unwrap());
        let entries = read_entries_at(&path).unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["alpha", "gamma"]);
        assert!(!remove_at(&path, "beta").unwrap(), "nothing left to remove");
    }

    #[test]
    fn an_unparsable_document_is_refused_with_its_bytes_untouched() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        let broken = "[[polis]]\nname = \"alpha\"\n";
        std::fs::write(&path, broken).unwrap();
        let error = add_at(&path, "beta", "https://two.example.test").unwrap_err();
        assert!(matches!(error, PolisConfigError::Broken(_)), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        assert!(remove_at(&path, "alpha").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn a_bad_name_or_origin_is_refused_and_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        for (name, url) in [
            ("Alpha", "https://api.example.test"),
            ("", "https://api.example.test"),
            ("alpha", "   "),
        ] {
            let error = add_at(&path, name, url).unwrap_err();
            assert!(matches!(error, PolisConfigError::Invalid(_)), "{error}");
        }
        assert!(!path.exists(), "a refused request creates no file");
    }

    #[test]
    fn a_document_with_a_repeated_name_is_broken_rather_than_rewritten() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        let repeated = document(&[
            ("alpha", "https://one.example.test"),
            ("alpha", "https://two.example.test"),
        ]);
        std::fs::write(&path, &repeated).unwrap();
        assert!(add_at(&path, "beta", "https://three.example.test").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), repeated);
    }

    #[test]
    fn concurrent_adds_for_different_names_both_land() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(POLIS_FILE);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut workers = Vec::new();
        for index in 0..8 {
            let path = path.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                add_at(&path, &format!("entry-{index}"), "https://api.example.test").unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let entries = read_entries_at(&path).unwrap();
        assert_eq!(entries.len(), 8, "{entries:?}");
    }
}
