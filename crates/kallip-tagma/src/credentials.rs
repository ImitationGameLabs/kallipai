//! The tagma's persisted credentials for authenticating to the relay fleet
//! (the Ed25519 device key + the tagma id/token issued at agora enrollment).
//!
//! These are the tagma's authentication material — distinct from the
//! [`crate::relay`] connector, which *consumes* them to hold the live tunnel.
//! Secrets live under `KALLIP_DATA_DIR/credentials/` (resolved via
//! `kallip_runtime::persistence::data_dir_root`), written owner-only (`0o600`);
//! the leaf dir is `0o700`.

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

/// Load stored `(tagma_id, token)` credentials, if a prior enrollment persisted them.
pub(crate) fn load_tagma(credentials_dir: &Path) -> Option<(String, String)> {
    let id = std::fs::read_to_string(credentials_dir.join("tagma.id")).ok()?;
    let token = std::fs::read_to_string(credentials_dir.join("tagma.token")).ok()?;
    Some((id.trim().to_owned(), token.trim().to_owned()))
}

/// Persist `(tagma_id, token)` for reuse across restarts.
pub(crate) fn save_tagma(credentials_dir: &Path, tagma_id: &str, tagma_token: &str) {
    let _ = std::fs::write(credentials_dir.join("tagma.id"), tagma_id);
    if let Err(e) = write_secret(&credentials_dir.join("tagma.token"), tagma_token.as_bytes()) {
        tracing::error!(
            error = %format!("{e:#}"),
            "failed to persist tagma token; next restart will require re-enrollment"
        );
    }
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
