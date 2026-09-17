use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context as _, Result};

use kallip_common::AgentId;

/// The three per-instance directory roots a runtime process needs: the
/// data home, the declared-configuration home, and the state home.
///
/// The runtime is identity-free: it does not know how the host named it
/// or where the host derives its directories from. The host (the tagma
/// binary, the `kallip` CLI) resolves the three roots from its own
/// identity configuration and installs them once at boot with
/// [`install_instance_roots`]. Every derived path in the runtime hangs
/// from these roots; nothing is resolved from the environment here.
#[derive(Debug, Clone)]
pub struct InstanceRoots {
    /// The data home: the agent trees (`agents/` with their `active/`,
    /// `inactive/`, and `archived/` life stages) and `skills/` live here.
    pub data: PathBuf,
    /// The declared-configuration home (`profiles.toml`,
    /// `exec_hooks.toml`): operator intent rather than runtime data.
    pub config: PathBuf,
    /// The state home namespace: pure output residue (instance logs)
    /// that persists across restarts yet is not portable enough to
    /// belong in the data home.
    pub state: PathBuf,
}

static INSTANCE_ROOTS: RwLock<Option<InstanceRoots>> = RwLock::new(None);

/// Install the process-wide instance roots.
///
/// The host calls this exactly once at boot, before any runtime
/// subsystem resolves a path. A second call is an error: the roots are
/// process identity, and silently replacing them would scatter data
/// across trees.
pub fn install_instance_roots(roots: InstanceRoots) -> Result<()> {
    let mut slot = INSTANCE_ROOTS
        .write()
        .map_err(|_| anyhow::anyhow!("instance roots lock poisoned"))?;
    if slot.is_some() {
        anyhow::bail!("instance roots already installed; install them once at boot");
    }
    *slot = Some(roots);
    Ok(())
}

/// The installed instance roots.
fn roots() -> Result<InstanceRoots> {
    INSTANCE_ROOTS
        .read()
        .map_err(|_| anyhow::anyhow!("instance roots lock poisoned"))?
        .clone()
        .context("instance roots not installed; the host must install them at boot")
}

/// Available with the `testutils` feature (and during this crate's own
/// tests): overwrite the installed instance roots. Test-only — the
/// production install path is [`install_instance_roots`], which refuses
/// a second install.
#[cfg(any(test, feature = "testutils"))]
pub fn set_instance_roots_for_tests(roots: Option<InstanceRoots>) {
    if let Ok(mut slot) = INSTANCE_ROOTS.write() {
        *slot = roots;
    }
}

/// Resolve the shared data root under which the agent trees and
/// `skills/` live.
///
/// All three life-stage bases route through this so every life-stage tree
/// shares one root. When that root is on a single filesystem,
/// `archive_agent_dir`'s `rename` is atomic; if the root is symlinked across a
/// filesystem boundary the `rename` raises `EXDEV` and the archive falls back to
/// a recursive copy + delete (see `archive_agent_dir`).
pub fn data_dir_root() -> Result<PathBuf> {
    Ok(roots()?.data)
}

/// Resolve the per-instance config root.
///
/// Declared configuration (`profiles.toml`, `exec_hooks.toml`) is
/// operator intent rather than runtime data, so it lives under the
/// config home rather than the data home.
pub fn config_dir_root() -> Result<PathBuf> {
    Ok(roots()?.config)
}

/// Resolve the state home namespace — where pure output residue lives
/// (instance logs), symmetric to [`data_dir_root`]'s data side but under
/// `$XDG_STATE_HOME`. State is material that persists across restarts
/// yet is not portable enough to belong in the data home — XDG puts
/// logs and history here, not in cache (logs are not regenerable) and
/// not inside the portable instance tree.
pub fn state_dir_root() -> Result<PathBuf> {
    Ok(roots()?.state)
}
/// Canonicalize the data root for path-overlap comparison.
///
/// The data root may not exist yet on a fresh install (no agent ever created), in
/// which case [`std::fs::canonicalize`] would fail. Fall back to canonicalizing the
/// nearest existing ancestor and re-appending the leaf,
/// yielding the canonical path the data root *would* have. This keeps the overlap
/// check sound without forcing the data dir to exist.
fn canonical_data_root() -> Result<PathBuf> {
    let root = data_dir_root()?;
    match root.canonicalize() {
        Ok(c) => Ok(c),
        // The root may not exist yet on a fresh install — and neither may the
        // instance namespace above it. Walk up to the nearest existing ancestor and
        // re-append the components that exist only notionally, yielding the
        // canonical path the root *would* have.
        Err(_) => {
            let mut tail: Vec<std::ffi::OsString> = Vec::new();
            let mut cur: &Path = root.as_path();
            loop {
                let Some(name) = cur.file_name() else {
                    anyhow::bail!("data root {root:?} has no existing ancestor");
                };
                let parent = cur
                    .parent()
                    .with_context(|| format!("data root {root:?} has no parent"))?;
                match parent.canonicalize() {
                    Ok(base) => {
                        let mut canon = base;
                        canon.push(name);
                        for part in tail.into_iter().rev() {
                            canon.push(part);
                        }
                        return Ok(canon);
                    }
                    Err(_) => {
                        tail.push(name.to_os_string());
                        cur = parent;
                    }
                }
            }
        }
    }
}

/// Whether `workspace_root` and the tagma data root share an ancestor/descendant
/// relationship — i.e. one contains the other.
///
/// Such overlap must be rejected by the tagma: an agent whose workspace *is* (or
/// *contains*) the data tree could write tagma bookkeeping (`meta.json`,
/// `context.json`, `exec_policy.toml`, peers' `agents/<id>/`, ...). With the overlap
/// eliminated, landlock alone enforces the data-dir integrity baseline (the agent's
/// writable set never covers the data tree).
///
/// Both sides are canonicalized; a canonicalize failure of either side yields `Err`
/// so the caller fails closed. `workspace_root` is expected to already be
/// canonicalized by `AgentConfig::load`, but is re-canonicalized here defensively.
pub fn workspace_overlaps_data_root(workspace_root: &Path) -> Result<bool> {
    let data = canonical_data_root()?;
    let ws = workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {workspace_root:?}"))?;
    Ok(data.starts_with(&ws) || ws.starts_with(&data))
}

/// Reject a workspace that overlaps the tagma data tree.
///
/// Shared by `create_agent` and restore so the message and verdict come from one
/// place. Returns `Ok(())` when the workspace is safely disjoint; otherwise an
/// error whose message is suitable to surface to the operator. Callers fail
/// closed on the underlying canonicalize errors (propagated via `?`).
pub fn ensure_workspace_disjoint(workspace_root: &Path) -> Result<()> {
    if workspace_overlaps_data_root(workspace_root)? {
        anyhow::bail!(
            "workspace_root {} overlaps the tagma data directory; choose a workspace \
             outside the data tree",
            workspace_root.display()
        );
    }
    Ok(())
}

/// Resolve the base live-agents directory (`agents/active/`).
pub(crate) fn agents_base() -> Result<PathBuf> {
    Ok(data_dir_root()?.join("agents").join("active"))
}

/// Resolve the base archived-agents directory (`agents/archived/`).
///
/// Archived agents live here, fully transparent to [`crate::persistence::scan_agents`] and the live
/// registry. See [`archive_agent_dir`].
fn archived_base() -> Result<PathBuf> {
    Ok(data_dir_root()?.join("agents").join("archived"))
}

/// Resolve the base inactive agents directory (`agents/inactive/`).
///
/// Inactive agents are parked here by the declarative-team converge flow:
/// their declaration no longer references them, but the lock still holds
/// their role↔id binding, so they can be restored identity-intact. Fully
/// transparent to [`crate::persistence::scan_agents`] and the live registry — see
/// [`deactivate_agent_dir`].
pub(crate) fn inactive_base() -> Result<PathBuf> {
    Ok(data_dir_root()?.join("agents").join("inactive"))
}

/// Inactive directory for a given agent (under `inactive_base`).
///
/// Public for the tagma's team status face, which probes whether a lock
/// record's id is parked here (the restore-vs-spawn input); the layout
/// knowledge stays in this module.
pub fn inactive_dir(agent_id: &AgentId) -> Result<PathBuf> {
    Ok(inactive_base()?.join(agent_id.as_ref()))
}
/// Archived directory for a given agent (under `archived_base`).
pub fn archived_dir(agent_id: &AgentId) -> Result<PathBuf> {
    Ok(archived_base()?.join(agent_id.as_ref()))
}

/// Agent directory for a given agent.
pub fn agent_dir(agent_id: &AgentId) -> Result<PathBuf> {
    Ok(agents_base()?.join(agent_id.as_ref()))
}

#[cfg(test)]
mod tests;
