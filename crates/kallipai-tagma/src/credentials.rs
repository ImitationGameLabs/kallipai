//! The tagma's persisted credentials for authenticating to the relay fleet
//! (the Ed25519 device key + the tagma id/token issued at archeion enrollment).
//!
//! These are the tagma's authentication material — distinct from the
//! [`crate::relay`] connector, which *consumes* them to hold the live tunnel.
//! Secrets live under the instance data root's `credentials/` (resolved
//! via `kallipai_adk::persistence::data_dir_root`), written owner-only (`0o600`);
//! the leaf dir is `0o700`.
//! A third file, `archeion.url`, records the enrollment origin (non-secret:
//! it mirrors the configured env var) so a later enrollment code at a
//! different archeion can be told apart from a stale same-archeion code. Whether
//! stored credentials count as an enrollment is not decided here: this crate and
//! the daemon both ask `kallipai_daemon_common::credentials`.

use std::path::Path;

use anyhow::{Context, Result};
use kallipai_daemon_common::credentials::{TAGMA_ID_FILE, TAGMA_TOKEN_FILE};
use kallipai_e2ee::DeviceKey;

/// Load the device key from `credentials_dir/device.key`, or generate + persist
/// a new one. The key is a 32-byte Ed25519 seed.
pub(crate) fn load_or_create_device(credentials_dir: &Path) -> Result<DeviceKey> {
    let path = credentials_dir.join("device.key");
    if let Ok(seed_bytes) = std::fs::read(&path)
        && let Ok(seed) = seed_bytes.as_slice().try_into()
    {
        return Ok(DeviceKey::from_seed(seed));
    }
    let device = DeviceKey::generate();
    write_credential_file(&path, &device.seed())?;
    Ok(device)
}

/// Stored enrollment material for one relay entry: the archeion-issued
/// (id, token) pair plus the origin the enrollment happened at. The origin
/// is absent on credentials written before origin recording began — the
/// next stored boot backfills it.
pub(crate) struct StoredTagma {
    pub(crate) id: String,
    pub(crate) token: String,
    pub(crate) archeion_url: Option<String>,
}

/// Load stored credentials, if a prior enrollment persisted them.
///
/// Persisted is the shared predicate
/// [`kallipai_daemon_common::credentials::read_entry`]: both files readable
/// and non-blank. An interrupted write leaves a blank file behind, and that
/// has to read as not-enrolled here exactly as it does in the daemon's scan,
/// replay filter and adopt probe.
pub(crate) fn load_tagma(credentials_dir: &Path) -> Option<StoredTagma> {
    let stored = kallipai_daemon_common::credentials::read_entry(credentials_dir)?;
    let archeion_url = std::fs::read_to_string(credentials_dir.join("archeion.url"))
        .ok()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty());
    Some(StoredTagma {
        id: stored.id,
        token: stored.token,
        archeion_url,
    })
}

/// Persist `(tagma_id, tagma_token)` for reuse across restarts, recording
/// the enrollment origin alongside (non-secret: it mirrors the configured
/// env var, and lets a later code-plus-different-archeion boot be told apart
/// from a stale same-archeion code).
///
/// Write order carries the crash story. The origin lands first, because it is
/// hygiene data: a failure there leaves no enrollment published, so the caller
/// may degrade without the disk disagreeing. The stored id is cleared next,
/// the token written, and the id written back last — any interruption in
/// between reads as not-enrolled, since the shared predicate needs both halves
/// and a missing id settles it on its own. Each file is written atomically by
/// [`write_credential_file`], and a failure travels back to the caller instead of being
/// logged, so a returned error always means no new enrollment reached the
/// disk. The one caller reaches here on the first-run branch only, where the
/// pair being replaced is already unusable under that predicate, so clearing
/// the id cannot destroy a working enrollment.
pub(crate) fn save_tagma(
    credentials_dir: &Path,
    tagma_id: &str,
    tagma_token: &str,
    archeion_url: &str,
) -> Result<()> {
    write_credential_file(
        &credentials_dir.join("archeion.url"),
        archeion_url.as_bytes(),
    )?;
    let id_path = credentials_dir.join(TAGMA_ID_FILE);
    match std::fs::remove_file(&id_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("clear {} before re-enrolling", id_path.display()));
        }
    }
    write_credential_file(
        &credentials_dir.join(TAGMA_TOKEN_FILE),
        tagma_token.as_bytes(),
    )?;
    write_credential_file(&id_path, tagma_id.as_bytes())?;
    Ok(())
}

/// Rebind the recorded enrollment origin to the configured one: write when
/// absent (credentials that predate origin recording), overwrite when the
/// recorded origin disagrees (the platform-origin rename migrates stored
/// enrollments without re-enrolling), no-op when equal.
///
/// Failure travels back rather than aborting: the caller warns and boots on,
/// because the enrollment itself is intact and only the recorded origin is
/// stale.
pub(crate) fn backfill_archeion_url(credentials_dir: &Path, archeion_url: &str) -> Result<()> {
    let path = credentials_dir.join("archeion.url");
    if std::fs::read_to_string(&path).is_ok_and(|prev| prev.trim() == archeion_url) {
        return Ok(());
    }
    write_credential_file(&path, archeion_url.as_bytes())
}

/// Write one credential file with mode `0o600`, atomically: the secrets (the
/// device key, the tagma token) and the non-secret files (the id, the origin)
/// all go through here.
///
/// This is the single mode policy point for the credential files; the
/// mechanics come from `kallipai_common::secret_file::write_atomic`: the
/// bytes land in a temp file in the destination directory and are renamed
/// over the target, so a crash or a full disk can never leave a truncated
/// file where a credential is expected.
fn write_credential_file(path: &Path, bytes: &[u8]) -> Result<()> {
    kallipai_common::secret_file::write_atomic(path, bytes, 0o600)
        .with_context(|| format!("write credential file {path:?}"))
}

/// Set a directory's permissions to owner-only (`0o700`), Unix-only.
pub(crate) fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set permissions on {path:?}"))?;
    Ok(())
}

/// One-time migration from the pre-multi-archeion flat layout
/// (`credentials/tagma.id` + `credentials/tagma.token`) into the entry's
/// subdirectory. Runs at boot before any entry is resolved.
///
/// Exactly one configured entry is the only unambiguous case: the flat pair
/// belongs to the single archeion the tagma was enrolled with, and `fs::rename`
/// moves the files (mode 0o600 and content untouched) into
/// `credentials/<name>/`. Every other state fails fast with both exits named
/// — guessing a destination would risk attaching an existing identity to
/// the wrong archeion:
///
/// - zero entries with leftover flat files: nothing to attach them to;
/// - multiple entries: the flat pair cannot be attributed to one of them;
/// - the entry's subdirectory already holding credentials: stale duplicate.
pub(crate) fn migrate_legacy_layout(credentials_dir: &Path, entries: &[String]) -> Result<()> {
    let legacy_id = credentials_dir.join(TAGMA_ID_FILE);
    let legacy_token = credentials_dir.join(TAGMA_TOKEN_FILE);
    if !legacy_id.exists() && !legacy_token.exists() {
        return Ok(()); // already on the per-entry layout (the common case)
    }
    let name = match entries.first() {
        Some(n) => n,
        None => anyhow::bail!(
            concat!(
                "legacy flat credentials exist in {} but no relay entry is ",
                " configured; delete them if the enrollment is defunct"
            ),
            credentials_dir.display()
        ),
    };
    anyhow::ensure!(
        entries.len() == 1,
        concat!(
            "legacy flat credentials exist in {} but {} relay entries are ",
            " configured, so the flat files cannot be attributed to one entry; ",
            " move them into the right credentials/<name>/ manually"
        ),
        credentials_dir.display(),
        entries.len()
    );
    let entry_dir = credentials_dir.join(name);
    let new_id = entry_dir.join(TAGMA_ID_FILE);
    let new_token = entry_dir.join(TAGMA_TOKEN_FILE);
    anyhow::ensure!(
        !new_id.exists() && !new_token.exists(),
        concat!(
            "conflicting credentials: legacy flat files exist in {} and the ",
            " entry directory {} already holds credentials; delete the stale set"
        ),
        credentials_dir.display(),
        entry_dir.display()
    );
    std::fs::create_dir_all(&entry_dir).context("create entry credentials dir")?;
    set_owner_only(&entry_dir)?;
    if legacy_id.exists() {
        std::fs::rename(&legacy_id, &new_id)
            .with_context(|| format!("migrate {} to {}", legacy_id.display(), new_id.display()))?;
    }
    if legacy_token.exists() {
        std::fs::rename(&legacy_token, &new_token).with_context(|| {
            format!(
                "migrate {} to {}",
                legacy_token.display(),
                new_token.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_pair(dir: &Path) {
        std::fs::write(dir.join(TAGMA_ID_FILE), "tid-1").unwrap();
        write_credential_file(&dir.join(TAGMA_TOKEN_FILE), b"sk-test").unwrap();
    }

    #[test]
    fn migrates_single_entry_once() {
        let dir = tempfile::TempDir::new().unwrap();
        legacy_pair(dir.path());
        migrate_legacy_layout(dir.path(), &["main".to_string()]).unwrap();
        let entry = dir.path().join("main");
        assert_eq!(
            std::fs::read_to_string(entry.join(TAGMA_ID_FILE)).unwrap(),
            "tid-1"
        );
        assert!(entry.join(TAGMA_TOKEN_FILE).exists());
        assert!(!dir.path().join(TAGMA_ID_FILE).exists());
        // Idempotent: a second boot sees no legacy files and is a no-op.
        migrate_legacy_layout(dir.path(), &["main".to_string()]).unwrap();
    }

    #[test]
    fn rejects_ambiguous_or_conflicting_states() {
        let dir = tempfile::TempDir::new().unwrap();
        legacy_pair(dir.path());
        // Zero entries: nothing to attach the flat pair to.
        assert!(migrate_legacy_layout(dir.path(), &[]).is_err());
        // Multiple entries: cannot attribute the flat pair.
        assert!(migrate_legacy_layout(dir.path(), &["a".to_string(), "b".to_string()]).is_err());
        // Entry dir already holds credentials: stale duplicate.
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a").join(TAGMA_ID_FILE), "tid-2").unwrap();
        assert!(migrate_legacy_layout(dir.path(), &["a".to_string()]).is_err());
    }

    #[test]
    fn pre_rename_origin_file_reads_none_then_backfills_new_name() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(TAGMA_ID_FILE), "tid-1").unwrap();
        write_credential_file(&dir.path().join(TAGMA_TOKEN_FILE), b"sk-test").unwrap();
        // Credentials written before the rename keep the old origin
        // file name; the loader must treat the origin as absent.
        std::fs::write(dir.path().join("agora.url"), "https://agora.example.com").unwrap();
        let stored = load_tagma(dir.path()).unwrap();
        assert!(stored.archeion_url.is_none());
        // The next stored boot backfills the origin under the new
        // name and leaves the stale old file untouched.
        backfill_archeion_url(dir.path(), "https://archeion.example.com")
            .expect("backfill writes the origin");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("archeion.url")).unwrap(),
            "https://archeion.example.com"
        );
        assert!(dir.path().join("agora.url").exists());
        // The other form: an existing new-name file is read back.
        let stored = load_tagma(dir.path()).unwrap();
        assert_eq!(
            stored.archeion_url.as_deref(),
            Some("https://archeion.example.com")
        );
    }
    #[test]
    fn blank_or_whitespace_credentials_are_not_enrolled() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(TAGMA_ID_FILE), "tid-1").unwrap();
        write_credential_file(&dir.path().join(TAGMA_TOKEN_FILE), b"   \n").unwrap();
        assert!(
            load_tagma(dir.path()).is_none(),
            "a whitespace-only token is not an enrollment"
        );
        write_credential_file(&dir.path().join(TAGMA_TOKEN_FILE), b"sk-test").unwrap();
        std::fs::write(dir.path().join(TAGMA_ID_FILE), " \n").unwrap();
        assert!(
            load_tagma(dir.path()).is_none(),
            "a whitespace-only id is not an enrollment"
        );
        std::fs::write(dir.path().join(TAGMA_ID_FILE), " tid-1 \n").unwrap();
        let stored = load_tagma(dir.path()).expect("a complete pair is an enrollment");
        assert_eq!(stored.id, "tid-1");
        assert_eq!(stored.token, "sk-test");
    }

    #[test]
    fn a_blank_origin_file_reads_as_no_origin() {
        let dir = tempfile::TempDir::new().unwrap();
        legacy_pair(dir.path());
        std::fs::write(dir.path().join("archeion.url"), "  \n").unwrap();
        let stored = load_tagma(dir.path()).expect("the pair is complete");
        assert_eq!(stored.archeion_url, None);
    }

    #[test]
    fn a_failed_token_write_leaves_the_store_unenrolled() {
        let dir = tempfile::TempDir::new().unwrap();
        legacy_pair(dir.path());
        // A directory where the token belongs fails the token write
        // deterministically, without depending on uid or permissions.
        std::fs::remove_file(dir.path().join(TAGMA_TOKEN_FILE)).unwrap();
        std::fs::create_dir(dir.path().join(TAGMA_TOKEN_FILE)).unwrap();
        let error = save_tagma(dir.path(), "tid-2", "sk-2", "https://archeion.example.com")
            .expect_err("the token write must fail");
        assert!(
            format!("{error:#}").contains("write credential file"),
            "{error:#}"
        );
        assert!(
            load_tagma(dir.path()).is_none(),
            "a save that failed mid-way must not look enrolled"
        );
        assert!(
            !dir.path().join(TAGMA_ID_FILE).exists(),
            "the id is written last, so a failed save leaves none"
        );
    }

    #[test]
    fn a_refused_clear_keeps_the_old_token_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        legacy_pair(dir.path());
        // A directory where the id belongs: the origin lands, clearing the id
        // then fails, so the save stops before touching the token.
        std::fs::remove_file(dir.path().join(TAGMA_ID_FILE)).unwrap();
        std::fs::create_dir(dir.path().join(TAGMA_ID_FILE)).unwrap();
        assert!(
            save_tagma(dir.path(), "tid-2", "sk-2", "https://archeion.example.com").is_err(),
            "the refused clear is reported"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(TAGMA_TOKEN_FILE)).unwrap(),
            "sk-test",
            "the previous token is left intact, not truncated"
        );
        assert!(load_tagma(dir.path()).is_none());
    }

    #[test]
    fn a_failed_origin_write_leaves_the_stored_pair_usable() {
        let dir = tempfile::TempDir::new().unwrap();
        legacy_pair(dir.path());
        // A directory where the origin belongs: the origin is written first,
        // so this failure lands before anything else moves and the pair
        // already on disk has to survive as a working enrollment.
        std::fs::create_dir(dir.path().join("archeion.url")).unwrap();
        assert!(
            save_tagma(dir.path(), "tid-2", "sk-2", "https://archeion.example.com").is_err(),
            "the refused origin write is reported"
        );
        let stored = load_tagma(dir.path()).expect("the old pair is still an enrollment");
        assert_eq!(stored.id, "tid-1");
        assert_eq!(stored.token, "sk-test");
    }

    #[test]
    fn a_successful_save_replaces_rather_than_truncates() {
        use std::io::Read;
        let dir = tempfile::TempDir::new().unwrap();
        legacy_pair(dir.path());
        let mut held = std::fs::File::open(dir.path().join(TAGMA_TOKEN_FILE)).unwrap();
        save_tagma(dir.path(), "tid-2", "sk-2", "https://archeion.example.com")
            .expect("save succeeds");
        let mut old = String::new();
        held.read_to_string(&mut old).unwrap();
        assert_eq!(
            old, "sk-test",
            "the old inode keeps its bytes: rename, not truncate-in-place"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(TAGMA_TOKEN_FILE)).unwrap(),
            "sk-2"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(TAGMA_ID_FILE)).unwrap(),
            "tid-2"
        );
    }

    #[test]
    fn saved_credentials_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        save_tagma(dir.path(), "tid-1", "sk-1", "https://archeion.example.com").unwrap();
        for name in [TAGMA_ID_FILE, TAGMA_TOKEN_FILE, "archeion.url"] {
            let mode = std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name} is owner-only");
        }
    }
}
