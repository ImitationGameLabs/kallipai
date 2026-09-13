//! The tagma's persisted credentials for authenticating to the relay fleet
//! (the Ed25519 device key + the tagma id/token issued at archeion enrollment).
//!
//! These are the tagma's authentication material — distinct from the
//! [`crate::relay`] connector, which *consumes* them to hold the live tunnel.
//! Secrets live under the instance data root's `credentials/` (resolved
//! via `kallip_runtime::persistence::data_dir_root`), written owner-only (`0o600`);
//! the leaf dir is `0o700`.
//! A third file, `archeion.url`, records the enrollment origin (non-secret:
//! it mirrors the configured env var) so a later enrollment code at a
//! different archeion can be told apart from a stale same-archeion code. The
//! kallip-daemon start path mirrors the stored-credentials predicate
//! (`tagma.id` + `tagma.token`) to decide whether replaying an enrollment
//! code is safe — keep this layout in sync.

use std::path::Path;

use anyhow::{Context, Result};
use kallip_e2ee::DeviceKey;

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
    write_secret(&path, &device.seed())?;
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
pub(crate) fn load_tagma(credentials_dir: &Path) -> Option<StoredTagma> {
    let id = std::fs::read_to_string(credentials_dir.join("tagma.id")).ok()?;
    let token = std::fs::read_to_string(credentials_dir.join("tagma.token")).ok()?;
    let archeion_url = std::fs::read_to_string(credentials_dir.join("archeion.url"))
        .ok()
        .map(|url| url.trim().to_owned());
    Some(StoredTagma {
        id: id.trim().to_owned(),
        token: token.trim().to_owned(),
        archeion_url,
    })
}

/// Persist `(tagma_id, tagma_token)` for reuse across restarts, recording
/// the enrollment origin alongside (non-secret: it mirrors the configured
/// env var, and lets a later code-plus-different-archeion boot be told apart
/// from a stale same-archeion code).
pub(crate) fn save_tagma(
    credentials_dir: &Path,
    tagma_id: &str,
    tagma_token: &str,
    archeion_url: &str,
) {
    let _ = std::fs::write(credentials_dir.join("tagma.id"), tagma_id);
    let _ = std::fs::write(credentials_dir.join("archeion.url"), archeion_url);
    if let Err(e) = write_secret(&credentials_dir.join("tagma.token"), tagma_token.as_bytes()) {
        tracing::error!(
            error = %format!("{e:#}"),
            "failed to persist tagma token; next restart will require re-enrollment"
        );
    }
}

/// Rebind the recorded enrollment origin to the configured one: write when
/// absent (credentials that predate origin recording), overwrite when the
/// recorded origin disagrees (the platform-origin rename migrates stored
/// enrollments without re-enrolling), no-op when equal.
pub(crate) fn backfill_archeion_url(credentials_dir: &Path, archeion_url: &str) {
    let path = credentials_dir.join("archeion.url");
    if std::fs::read_to_string(&path).is_ok_and(|prev| prev.trim() == archeion_url) {
        return;
    }
    let _ = std::fs::write(&path, archeion_url);
}

/// Write a secret (device key, tagma token) with mode `0o600` so other local
/// users cannot read it. Unix-only: `mode` is masked by the process umask, and
/// `0o600 & !umask` stays `0o600` under the usual `0o022`.
pub(crate) fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .with_context(|| format!("write secret to {path:?}"))?;
    Ok(())
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
    let legacy_id = credentials_dir.join("tagma.id");
    let legacy_token = credentials_dir.join("tagma.token");
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
    let new_id = entry_dir.join("tagma.id");
    let new_token = entry_dir.join("tagma.token");
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
        std::fs::write(dir.join("tagma.id"), "tid-1").unwrap();
        write_secret(&dir.join("tagma.token"), b"sk-test").unwrap();
    }

    #[test]
    fn migrates_single_entry_once() {
        let dir = tempfile::TempDir::new().unwrap();
        legacy_pair(dir.path());
        migrate_legacy_layout(dir.path(), &["main".to_string()]).unwrap();
        let entry = dir.path().join("main");
        assert_eq!(
            std::fs::read_to_string(entry.join("tagma.id")).unwrap(),
            "tid-1"
        );
        assert!(entry.join("tagma.token").exists());
        assert!(!dir.path().join("tagma.id").exists());
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
        std::fs::write(dir.path().join("a/tagma.id"), "tid-2").unwrap();
        assert!(migrate_legacy_layout(dir.path(), &["a".to_string()]).is_err());
    }

    #[test]
    fn pre_rename_origin_file_reads_none_then_backfills_new_name() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("tagma.id"), "tid-1").unwrap();
        write_secret(&dir.path().join("tagma.token"), b"sk-test").unwrap();
        // Credentials written before the rename keep the old origin
        // file name; the loader must treat the origin as absent.
        std::fs::write(dir.path().join("agora.url"), "https://agora.example.com").unwrap();
        let stored = load_tagma(dir.path()).unwrap();
        assert!(stored.archeion_url.is_none());
        // The next stored boot backfills the origin under the new
        // name and leaves the stale old file untouched.
        backfill_archeion_url(dir.path(), "https://archeion.example.com");
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
}
