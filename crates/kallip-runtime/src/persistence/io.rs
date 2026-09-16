use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use kallip_common::policy::ExecPolicy;

use crate::context::ContextStore;

/// Atomically and durably write content to a file via temp file + rename.
/// Durability closes the power-loss window the plain rename left open: the temp
/// file is `sync_data`d before the rename so a crash never promotes a half-
/// written file, and the parent directory is synced after the rename so the
/// rename itself survives power loss (many filesystems journal directory
/// entries lazily — without the dir fsync a crash can leave the target
/// missing or zero-length). A dir-sync failure is logged and downgraded to a
/// warning: by then the rename has already landed, so propagating an error
/// would overstate the damage. Neither sync path is unit-testable; this is
/// verified by walkthrough.
pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp_path = parent.join(format!(".{file_name}.tmp"));

    let mut file = fs::File::create(&temp_path)?;
    file.write_all(content.as_bytes())?;
    file.sync_data()?;
    drop(file);

    fs::rename(&temp_path, path)?;

    if let Err(e) = fs::File::open(parent).and_then(|dir| dir.sync_all()) {
        tracing::warn!("directory fsync failed for {}: {e}", parent.display());
    }
    Ok(())
}

/// Project the context store and write it as `manifest.json` + `pins.json`.
/// Split persistence (see `context::manifest`): the store is no longer
/// serialized whole, so a per-turn persist writes kilobytes instead of the
/// full 100KB+ window. Each document goes out durably with the previous
/// version kept as `.bak` — the first fallback if a parse ever fails.
pub fn persist_context(store: &ContextStore, dir: &Path) -> Result<()> {
    let manifest =
        serde_json::to_string(&store.to_manifest_doc()).context("serializing manifest.json")?;
    let pins = serde_json::to_string(&store.to_pins_doc()).context("serializing pins.json")?;
    write_with_backup(&dir.join("manifest.json"), &manifest)?;
    write_with_backup(&dir.join("pins.json"), &pins)?;
    Ok(())
}

/// Durable write that keeps the previous version as `<name>.bak`.
/// A failed backup copy is logged and tolerated: the fresh write is atomic
/// and durable on its own, and proceeding beats refusing to persist. A
/// missing previous file (first write) simply skips the backup.
pub(crate) fn write_with_backup(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        let bak = backup_path(path);
        if let Err(e) = fs::copy(path, &bak) {
            tracing::warn!("backup copy for {} failed: {e}", path.display());
        }
    }
    atomic_write(path, content)
}

pub(crate) fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    name.push_str(".bak");
    path.with_file_name(name)
}

/// Read-and-parse a split-persistence document, falling back to its `.bak`
/// when the primary is unreadable or corrupt. `Ok((doc, true))` means the
/// backup won. The `Err` carries the backup's failure, chained after the
/// primary's — the backup error is the actionable one (both are bad).
pub(crate) fn load_with_backup<T: serde::de::DeserializeOwned>(path: &Path) -> Result<(T, bool)> {
    let read = |p: &Path| {
        fs::read_to_string(p)
            .with_context(|| format!("reading {}", p.display()))
            .and_then(|json| {
                serde_json::from_str(&json).with_context(|| format!("parsing {}", p.display()))
            })
    };
    match read(path) {
        Ok(doc) => Ok((doc, false)),
        Err(primary) => read(&backup_path(path))
            .map(|doc| (doc, true))
            .map_err(|backup| backup.context(format!("primary also failed: {primary:#}"))),
    }
}

/// Serialize and write approval store to approvals.json.
pub fn persist_approvals(json: &str, dir: &Path) -> Result<()> {
    atomic_write(&dir.join("approvals.json"), json)
}

/// Serialize and write the `bash_exec` exec-policy overrides to exec_policy.toml.
pub fn persist_exec_policy(dir: &Path, policy: &ExecPolicy) -> Result<()> {
    let toml_str = toml::to_string_pretty(policy).context("serializing exec_policy.toml")?;
    atomic_write(&dir.join("exec_policy.toml"), &toml_str)
}

/// Load exec-policy overrides from exec_policy.toml.
///
/// Returns the default (empty) policy when the file is absent: agents created
/// before this feature shipped have no exec_policy.toml, and restore must not
/// fail for them. Hard read/parse failures still error.
///
/// Keys are normalized to lowercase on load (the file is an untrusted boundary,
/// like `meta.json`): command names are matched case-insensitively by the
/// classifier, so a mixed-case or hand-edited key would otherwise silently never
/// match. This mirrors the PUT handler's `lowercase_keys`.
pub fn load_exec_policy(dir: &Path) -> Result<ExecPolicy> {
    let path = dir.join("exec_policy.toml");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ExecPolicy::default()),
        Err(e) => return Err(e).context("reading exec_policy.toml"),
    };
    let mut policy: ExecPolicy = toml::from_str(&content).context("parsing exec_policy.toml")?;
    policy.lowercase_keys();
    Ok(policy)
}

#[cfg(test)]
mod tests;
