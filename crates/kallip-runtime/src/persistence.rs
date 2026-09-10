//! Agent persistence: atomic JSON serialization to disk.
//!
//! Writes context and approval state to per-agent directories.
//! All writes use atomic rename (temp file + rename) with data and directory
//! fsync to prevent corruption on crash. On tagma restart, [`scan_agents`] scans for agents
//! that can be recovered.

use std::fs;
use std::io::Write as _;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use kallip_common::policy::ExecPolicy;
use serde::{Deserialize, Serialize};

use crate::approval::ApprovalStore;
use crate::context::ContextStore;
use just_llm_client::types::chat::ChatMessage;
use kallip_common::AgentId;

/// Resolve the shared data root under which `agents/`, `archived/`, and
/// `skills/` live: `<platform_data_dir>/kallipai/tagmata/<KALLIP_TAGMA_SLUG>`.
///
/// The instance identity comes solely from `KALLIP_TAGMA_SLUG` — the daemon
/// injects it for managed instances, and every direct run (container,
/// benchmark, test) names itself the same way. The `kallipai` namespace
/// is the product-wide data home and `tagmata/` holds one directory per
/// instance. An unset `KALLIP_TAGMA_SLUG` is an error: there is no fallback
/// leaf — a process that cannot name itself must not guess where its
/// data lives.
///
/// Both `agents_base` and `archived_base` route through this so the live and
/// archived trees share one root. When that root is on a single filesystem,
/// `archive_agent_dir`'s `rename` is atomic; if the root is symlinked across a
/// filesystem boundary the `rename` raises `EXDEV` and the archive falls back to
/// a recursive copy + delete (see `archive_agent_dir`).
/// An explicit `KALLIP_TAGMA_DATA_DIR` wins first: the daemon hands the
/// instance its data directory at spawn, so the two sides no longer need
/// to assume a shared tree layout underneath. Without it the root is
/// slug-derived (`<platform_data_dir>/kallipai/tagmata/<KALLIP_TAGMA_SLUG>`).
pub fn data_dir_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("KALLIP_TAGMA_DATA_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::data_dir()
        .context("could not determine platform data directory")?
        .join("kallipai")
        .join("tagmata")
        .join(instance_slug()?))
}

/// The instance name every derived root hangs from: `KALLIP_TAGMA_SLUG`, set
/// for managed instances by the daemon and by every direct run
/// (container, benchmark, test) itself. Unset is an error: a process
/// that cannot name itself must not guess where its data lives.
fn instance_slug() -> Result<String> {
    std::env::var("KALLIP_TAGMA_SLUG")
        .ok()
        .filter(|s| !s.is_empty())
        .context(
            "KALLIP_TAGMA_SLUG is not set; the root is derived from it — name the instance with KALLIP_TAGMA_SLUG",
        )
}

/// Resolve the per-instance config root:
/// `<platform_config_dir>/kallipai/tagmata/<KALLIP_TAGMA_SLUG>`. Declared
/// configuration (`profiles.toml`, `exec_hooks.toml`) is operator intent
/// rather than runtime data, so it lives under the config home; the same
/// slug names the leaf both trees share. Errors when `KALLIP_TAGMA_SLUG` is
/// unset or the platform config home cannot be determined — callers
/// choose between degrading one config file and aborting boot.
pub fn config_dir_root() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("could not determine platform config directory")?
        .join("kallipai")
        .join("tagmata")
        .join(instance_slug()?))
}
/// Resolve the state home's `kallipai` namespace — where pure output
/// residue lives (instance logs), symmetric to [`data_dir_root`]'s data
/// side but under `$XDG_STATE_HOME`. State is material that persists
/// across restarts yet is not portable enough to belong in the data
/// home — XDG puts logs and history here, not in cache (logs are not
/// regenerable) and not inside the portable instance tree.
pub fn state_dir_root() -> Result<PathBuf> {
    Ok(dirs::state_dir()
        .context("could not determine platform state directory")?
        .join("kallipai"))
}

/// Canonicalize the data root for path-overlap comparison.
///
/// The data root may not exist yet on a fresh install (no agent ever created), in
/// which case [`std::fs::canonicalize`] would fail. Fall back to canonicalizing the
/// parent (which must exist — the platform data dir the slug-derived root hangs
/// under) and re-appending the leaf,
/// yielding the canonical path the data root *would* have. This keeps the overlap
/// check sound without forcing the data dir to exist.
fn canonical_data_root() -> Result<PathBuf> {
    let root = data_dir_root()?;
    match root.canonicalize() {
        Ok(c) => Ok(c),
        // The root may not exist yet on a fresh install — and neither may the
        // `kallipai/tagmata` namespace above it. Walk up to the nearest
        // existing ancestor (the platform data dir always exists) and
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

/// Resolve the base live-agents directory.
fn agents_base() -> Result<PathBuf> {
    Ok(data_dir_root()?.join("agents"))
}

/// Resolve the base archived-agents directory (sibling of `agents/`).
///
/// Archived agents live here, fully transparent to [`scan_agents`] and the live
/// registry. See [`archive_agent_dir`].
fn archived_base() -> Result<PathBuf> {
    Ok(data_dir_root()?.join("archived"))
}

/// Resolve the base deactivated (inactive) agents directory (sibling of
/// `agents/`).
///
/// Inactive agents are parked here by the declarative-team converge flow:
/// their declaration no longer references them, but the lock still holds
/// their role↔id binding, so they can be restored identity-intact. Fully
/// transparent to [`scan_agents`] and the live registry — see
/// [`deactivate_agent_dir`].
fn inactive_base() -> Result<PathBuf> {
    Ok(data_dir_root()?.join("agents-inactive"))
}

/// Inactive directory for a given agent (sibling of [`agent_dir`]).
///
/// Public for the tagma's team status face, which probes whether a lock
/// record's id is parked here (the restore-vs-spawn input); the layout
/// knowledge stays in this module.
pub fn inactive_dir(agent_id: &AgentId) -> Result<PathBuf> {
    Ok(inactive_base()?.join(agent_id.as_ref()))
}
/// Archived directory for a given agent (sibling of [`agent_dir`]).
pub fn archived_dir(agent_id: &AgentId) -> Result<PathBuf> {
    Ok(archived_base()?.join(agent_id.as_ref()))
}

/// Agent directory for a given agent.
pub fn agent_dir(agent_id: &AgentId) -> Result<PathBuf> {
    Ok(agents_base()?.join(agent_id.as_ref()))
}

/// Create agent directory and write initial meta.json.
pub fn create_agent_dir(agent_id: &AgentId, meta: AgentMeta) -> Result<PathBuf> {
    let dir = agent_dir(agent_id)?;
    std::fs::create_dir_all(&dir)?;

    atomic_write(
        &dir.join("meta.json"),
        &serde_json::to_string_pretty(&meta)?,
    )?;

    Ok(dir)
}

/// Read-modify-write `meta.json` to update `role`, `description`, and/or the
///
/// Used by `PUT /agents/{id}/metadata` and `PUT /agents/{id}/profile-set`.
/// Reads the current meta, applies the values, and atomically rewrites —
/// preserving whatever it did not touch. `None` leaves a field unchanged;
/// `Some(s)` sets it (for `profile_set`, the exact set name).
///
/// Callers hold the registry write-lock across the file I/O and the in-memory
/// update (persist-first, then `AgentConfig`) so disk and memory commit as
/// one unit. `check_meta` is startup-only, so the meta writers never run
/// concurrently.
pub fn rewrite_meta(
    dir: &Path,
    role: Option<&str>,
    description: Option<&str>,
    profile_set: Option<&str>,
    permissions_class: Option<crate::config::PermissionClass>,
) -> Result<()> {
    let path = dir.join("meta.json");
    let json = fs::read_to_string(&path).context("reading meta.json")?;
    let mut meta: AgentMeta = serde_json::from_str(&json).context("parsing meta.json")?;
    if let Some(r) = role {
        meta.role = r.to_owned();
    }
    if let Some(d) = description {
        meta.description = d.to_owned();
    }
    if let Some(s) = profile_set {
        meta.profile_set = Some(s.to_owned());
    }
    if let Some(c) = permissions_class {
        meta.permissions_class = c;
    }
    atomic_write(&path, &serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

/// Move a live agent directory into the inactive area (declarative-team
/// deactivation).
///
/// The rename/copy semantics mirror [`archive_agent_dir`] (atomic on one
/// filesystem, EXDEV copy+delete fallback). The inactive destination must
/// not already exist — a stale sibling would mean two preserved bodies for
/// one identity, and guessing which is current is exactly what this flow
/// must never do.
pub fn deactivate_agent_dir(agent_id: &AgentId) -> Result<()> {
    let src = agent_dir(agent_id)?;
    if !src.exists() {
        anyhow::bail!("cannot deactivate agent {agent_id}: no live directory");
    }
    let dst = inactive_dir(agent_id)?;
    if dst.exists() {
        anyhow::bail!(
            "inactive destination already exists for agent {agent_id} — refusing to overwrite (stale inactive body; resolve manually)"
        );
    }
    std::fs::create_dir_all(dst.parent().context("inactive path has no parent")?)?;
    move_agent_dir(&src, &dst, agent_id, "deactivation")
}

/// Move an inactive agent directory back into the live area (declarative-
/// team restoration). Identity-intact by construction: the whole directory
/// (context, approvals, meta) moves verbatim; the caller runs the regular
/// restore path afterwards — no prompt is injected.
///
/// The live destination must not already exist (pre-check before calling:
/// a live body for the same id means the restore plan is stale).
pub fn reactivate_agent_dir(agent_id: &AgentId) -> Result<()> {
    let src = inactive_dir(agent_id)?;
    if !src.exists() {
        anyhow::bail!("cannot reactivate agent {agent_id}: no inactive directory");
    }
    let dst = agent_dir(agent_id)?;
    if dst.exists() {
        anyhow::bail!(
            "live destination already exists for agent {agent_id} — refusing to overwrite"
        );
    }
    std::fs::create_dir_all(dst.parent().context("live path has no parent")?)?;
    move_agent_dir(&src, &dst, agent_id, "reactivation")
}

/// Shared rename/copy core for [`archive_agent_dir`],
/// [`deactivate_agent_dir`], and [`reactivate_agent_dir`]: atomic rename
/// on one filesystem, EXDEV copy+delete fallback across a boundary.
fn move_agent_dir(src: &Path, dst: &Path, agent_id: &AgentId, what: &str) -> Result<()> {
    if let Err(e) = std::fs::rename(src, dst) {
        if e.kind() != std::io::ErrorKind::CrossesDevices {
            return Err(e).context("moving agent directory");
        }
        // The caller's `dst.exists()` bail covers the rename path; the copy
        // fallback must hold the same invariant.
        if dst.exists() {
            anyhow::bail!(
                "{what} destination appeared during cross-device move of agent {agent_id} — refusing to overwrite"
            );
        }
        copy_dir_all(src, dst).context("cross-device agent dir copy")?;
        // Only delete the source once the copy fully succeeds — a failed copy
        // leaves `src` intact (and a partial `dst`) for manual recovery rather
        // than losing the agent's data.
        std::fs::remove_dir_all(src).context("removing source after cross-device move")?;
    }
    Ok(())
}

/// Move a lived agent's directory from `agents/` to `archived/` on remove.
///
/// The agent's data (history, `context.json` with cumulative usage, approvals,
/// exec_policy, meta) is preserved verbatim — removal becomes archival, not
/// destruction. `archived/` is a sibling of `agents/`, so it is invisible to
/// [`scan_agents`] and the live registry.
///
/// Idempotent: a missing source is a no-op. Bails if the destination already
/// exists — agent ids are `Uuid::new_v4()`, so a pre-existing destination is an
/// anomaly (backup restore / tampering / bug), not a collision; surfacing it
/// loudly beats silently overwriting a prior archive.
///
/// The move itself goes through the shared `move_agent_dir` core for
/// every agent-directory move (archive, deactivation, reactivation):
/// atomic `rename` on one filesystem, copy + delete across a boundary.
///
/// Rollback of never-alive agents (spawn/abort failure) stays a direct
/// [`std::fs::remove_dir_all`] at the call site — those agents never produced
/// data worth retaining.
pub fn archive_agent_dir(agent_id: &AgentId) -> Result<()> {
    let src = agent_dir(agent_id)?;
    if !src.exists() {
        return Ok(());
    }
    let dst = archived_dir(agent_id)?;
    if dst.exists() {
        anyhow::bail!(
            "archived destination already exists for agent {agent_id} — refusing \
             to overwrite (agent id collision should be impossible)"
        );
    }
    // Ensure the archived base exists (parent of `dst`), co-located with `agents_base`.
    std::fs::create_dir_all(dst.parent().context("archived path has no parent")?)?;
    move_agent_dir(&src, &dst, agent_id, "archive")
}

/// Recursively copy a directory tree `src` → `dst` (`dst` must not yet exist).
///
/// Symlinks are recreated rather than dereferenced (`file_type()` does not
/// follow them), so a link inside the tree can neither loop nor pull in the
/// wrong target. Agent dirs hold only regular files and dirs in practice, but
/// the symlink path keeps this correct if that ever changes. On any error a
/// partial `dst` is left in place for diagnosis — the caller must NOT delete
/// `src` unless this returns `Ok`.
///
/// Uses `fs::copy`, so the source mode bits are preserved — nix-store files
/// (0444) land read-only, which is the desired contract for seeded curated
/// skills (the agent authors NEW files alongside them, not over them).
pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // `DirEntry::file_type` lstats — it reports the symlink itself, not its
        // target, which is what the recreate-don't-dereference rule needs.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            let target = fs::read_link(&from)?;
            std::os::unix::fs::symlink(&target, &to)?;
        } else if file_type.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            // Regular file (agent dirs never hold devices/sockets; `fs::copy`
            // would degrade gracefully if one ever appeared).
            std::fs::copy(&from, &to).map(|_| ())?;
        }
    }
    Ok(())
}

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
fn write_with_backup(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        let bak = backup_path(path);
        if let Err(e) = fs::copy(path, &bak) {
            tracing::warn!("backup copy for {} failed: {e}", path.display());
        }
    }
    atomic_write(path, content)
}

fn backup_path(path: &Path) -> PathBuf {
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
fn load_with_backup<T: serde::de::DeserializeOwned>(path: &Path) -> Result<(T, bool)> {
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

// ---------------------------------------------------------------------------
// Restore (read path)
// ---------------------------------------------------------------------------

/// Minimal metadata persisted per agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMeta {
    pub workspace_root: PathBuf,
    /// Supervisor agent ID (for subagents).
    #[serde(default, rename = "created_by")]
    pub created_by: Option<AgentId>,
    /// Short display label ("researcher"). Supervisor-owned; mirrored from
    /// [`AgentConfig`](crate::config::AgentConfig). Empty means unset.
    #[serde(default)]
    pub role: String,
    /// Longer prose ("gathers sources for the plan"). Supervisor-owned.
    #[serde(default)]
    pub description: String,
    /// FS-access permission class (Guest readonly / Normal home-rw). A safety
    /// invariant — persisted so restore can re-validate it against the
    /// supervisor chain (unlike `role`/`description`, which are display-only).
    /// Defaults to Normal for legacy `meta.json` files written before this
    /// field existed.
    #[serde(default)]
    pub permissions_class: crate::config::PermissionClass,
    /// Profile-set binding (by name), resolved at spawn and re-read on
    /// restore. `None` marks a record written before set binding existed;
    /// restore surfaces such agents as unspecified instead of guessing.
    #[serde(default)]
    pub profile_set: Option<String>,
    /// Workspace delegation mode (`carve_out` subdir vs `full_handoff`
    /// whole-workspace). Persisted so restore reproduces the supervisor lock
    /// transfer for a FullHandoff child. Defaults to CarveOut for legacy metas.
    #[serde(default)]
    pub delegation_mode: crate::config::DelegationMode,
}

/// Read an agent's meta.json without side effects.
/// Used for validating supervisor chains at restore time.
pub fn read_meta(agent_id: &AgentId) -> Result<AgentMeta> {
    let path = agent_dir(agent_id)?.join("meta.json");
    let json = fs::read_to_string(&path).context("reading meta.json")?;
    serde_json::from_str(&json).context("parsing meta.json")
}

/// Read an agent's meta.json directly from its directory.
/// Use when the directory path is already known (e.g., budget updates)
/// to avoid re-deriving the path from the agent ID.
pub fn read_meta_from_dir(dir: &Path) -> Result<AgentMeta> {
    let json = fs::read_to_string(dir.join("meta.json")).context("reading meta.json")?;
    serde_json::from_str(&json).context("parsing meta.json")
}

/// Lightweight handle produced by scanning the agents directory.
pub struct PendingRestore {
    pub agent_id: AgentId,
    pub agent_dir: PathBuf,
    pub meta: AgentMeta,
}
/// An agent directory whose meta.json could not be read or parsed at scan
/// time. The tagma registers these as faulted (visible and manageable) so
/// unreadable agents never silently vanish from the fleet.
pub struct RefusedRestore {
    pub agent_id: AgentId,
    pub agent_dir: PathBuf,
    pub error: String,
}

/// An agent fully deserialized and ready to resume.
pub struct RestorableAgent {
    pub agent_id: AgentId,
    pub agent_dir: PathBuf,
    pub store: ContextStore,
    pub approvals: ApprovalStore,
    /// Non-fatal damage absorbed during restore (missing turns, skipped
    /// corrupt lines). Empty on a clean restore. Consumed by the caller for
    /// structured warning logs.
    pub degraded: Vec<Degradation>,
}

/// One non-fatal damage event absorbed during restore.
#[derive(Debug, Clone)]
pub struct Degradation {
    pub kind: DegradationKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradationKind {
    /// History records missing for manifest-referenced turn IDs.
    MissingHistoryTurns,
    /// History lines that failed to parse and were skipped.
    BadHistoryLines,
    /// Tool calls in a restored turn with no matching tool result.
    UnansweredToolCalls,
    /// Tool results in a restored turn answering no call in that turn.
    OrphanToolResults,
    /// manifest.json and its backup both unreadable or missing; window
    /// rebuilt from the history tail within a capped budget, leading
    /// damaged turns dropped.
    TailRecovery,
    /// pins.json and its backup both unreadable or missing; booted with
    /// empty pins.
    PinsLost,
}

/// List the agents directory, mapping "missing" (a fresh install) to `None`.
/// Any other read failure is an error: an unreadable agents directory must
/// not be mistaken for an empty fleet, or a second root gets minted over
/// the unreadable one.
fn read_agents_dir() -> Result<Option<fs::ReadDir>> {
    let base = agents_base().context("cannot resolve agents base")?;
    match fs::read_dir(&base) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!(
            "cannot read agents directory at {}",
            base.display()
        ))),
    }
}

/// Find the on-disk root agent: a directory whose meta.json has no `created_by`.
/// Returns the first hit (a second root is a legacy/corrupt state the tagma's
/// restore already refuses), `None` when the data dir holds no root at all.
/// Meta-read failures are skipped: an unreadable root surfaces through
/// [`scan_agents`] as a refused restore instead. A directory that exists
/// but cannot be read is an error, not "no root" -- callers must refuse
/// to mint rather than guess.
pub fn find_disk_root() -> Result<Option<AgentId>> {
    let entries = match read_agents_dir()? {
        Some(entries) => entries,
        None => return Ok(None),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let is_root = read_meta_from_dir(&path)
            .map(|meta| meta.created_by.is_none())
            .unwrap_or(false);
        if is_root {
            return Ok(path
                .file_name()
                .map(|n| AgentId::from(n.to_string_lossy().into_owned())));
        }
    }
    Ok(None)
}

/// Scan the agents directory and return agents eligible for restore.
///
/// Reads only `meta.json` per agent (lightweight). Directories whose meta is
/// unreadable are returned as [`RefusedRestore`] so the caller can surface
/// them as faulted agents instead of dropping them from view. An agents
/// directory that exists but cannot be listed is an error rather than an
/// empty scan -- see `read_agents_dir`.
pub fn scan_agents() -> Result<(Vec<PendingRestore>, Vec<RefusedRestore>)> {
    let entries = match read_agents_dir()? {
        Some(entries) => entries,
        None => return Ok((Vec::new(), Vec::new())),
    };

    let mut pending = Vec::new();
    let mut refused = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let agent_id = match path
            .file_name()
            .map(|n| AgentId::from(n.to_string_lossy().into_owned()))
        {
            Some(id) => id,
            None => continue,
        };
        match read_meta_from_dir(&path) {
            Ok(meta) => pending.push(PendingRestore {
                agent_id,
                agent_dir: path,
                meta,
            }),
            Err(e) => {
                tracing::warn!(id = %agent_id, "agent directory unreadable: {e:#}");
                refused.push(RefusedRestore {
                    agent_id,
                    agent_dir: path,
                    error: format!("{e:#}"),
                });
            }
        }
    }
    Ok((pending, refused))
}

/// Scan the inactive area for parked bodies: the declarative-team
/// lock-rebuild substrate. Each entry is (agent id, on-disk metadata);
/// unreadable directories are skipped with a warning (a parked body
/// whose meta cannot be read cannot be re-bound by role anyway).
/// Mirrors [`scan_agents`] over the inactive base — the live scan
/// deliberately never enters this area, keeping the two areas disjoint.
pub fn scan_inactive() -> Result<Vec<(AgentId, AgentMeta)>> {
    let base = inactive_base()?;
    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "cannot list the inactive area {}: {err}",
                base.display()
            ));
        }
    };
    let mut parked = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let agent_id = match path
            .file_name()
            .map(|n| AgentId::from(n.to_string_lossy().into_owned()))
        {
            Some(id) => id,
            None => continue,
        };
        match read_meta_from_dir(&path) {
            Ok(meta) => parked.push((agent_id, meta)),
            Err(e) => {
                tracing::warn!(id = %agent_id, "inactive directory unreadable: {e:#}");
            }
        }
    }
    Ok(parked)
}

/// Deserialize a single agent from its directory.
/// Loads the context via split persistence when `manifest.json` is present
/// (hydrating the conversation window from history by turn ID), otherwise
/// through the legacy `context.json` path — which then migrates in place to
/// the split format. Either way the on-load migrations, token re-estimation,
/// and restart-message injection still run. Non-fatal damage is collected in
/// `degraded` rather than failing the restore. `tail_budget` caps the
/// conversation window rebuilt from the history tail when manifest.json and
/// its backup are both unreadable (see [`DegradationKind::TailRecovery`]).
pub fn restore_agent(
    agent_id: &AgentId,
    dir: &Path,
    tail_budget: usize,
) -> Result<RestorableAgent> {
    let mut degraded = Vec::new();
    let (mut store, migrate_pending) = load_store(dir, tail_budget, &mut degraded)?;

    let approvals: ApprovalStore = match fs::read_to_string(dir.join("approvals.json")) {
        Ok(json) => serde_json::from_str(&json).context("parsing approvals.json")?,
        Err(_) => ApprovalStore::new(),
    };

    // Fold the legacy `pinned` vec (pre-unification format) into pinned turns at the front of
    // `turns`. No-op for new-format stores.
    store.migrate_legacy_pinned();

    // Migrate legacy summary field to a pinned turn.
    store.migrate_legacy_summary();

    // Tool-call/result pairing is guaranteed at record time
    // (`tool_execution::synthesize_unanswered_results`); damage persisted before that
    // guarantee (or written by hand) surfaces here as degradations. Report-only:
    // repairing is a separate explicit operation, never something a restore does
    // silently.
    check_tool_pairing(&store, &mut degraded);
    // Recompute every cached token estimate via the current estimator, so persisted estimates
    // (possibly from a prior estimator version, e.g. the old char/4 heuristic or stale legacy
    // pins) are brought up to date. Idempotent.
    store.reestimate_cached_tokens();

    // A restore may follow an agent-version upgrade that changed the system prompt or tool set,
    // invalidating the persisted `last_prompt_tokens` anchor. Force a full estimate on the first
    // post-restore round so the gate recomputes from the current config rather than trusting a
    // cross-version anchor. (migrate/pin/unpin above also set the flag; this is the canonical
    // restore statement and covers the clean-restore case.) See ContextStore::needs_full_estimate.
    store.mark_needs_full_estimate();
    // Deferred legacy→split migration: load_store only reads the legacy
    // document. Writing the split files here, after the folds above, is
    // what makes them carry the folded pins and summary instead of the
    // raw legacy shape.
    if migrate_pending {
        migrate_legacy_to_split(dir, &store)?;
    }
    // Drop any restart notice carried over from a prior restore (legacy
    // stores via `load_store`, split manifests written by older binaries
    // whose projection did not exclude injected turns) so each restore
    // leaves exactly one fresh notice below.
    strip_restart_turns(&mut store);

    let restart_msgs = vec![ChatMessage::user(RESTART_MESSAGE)];
    let (restart_id, estimated_tokens) = store.push_turn(restart_msgs.clone());
    // The restart notice is an on-the-spot prompt with no history record;
    // keep its ID out of the manifest projection (see `injected_turn_ids`).
    store.register_injected_turn(restart_id);

    // Record agent restore event in history.
    // Uses direct HistoryWriter (no AgentContext exists at restore time).
    {
        let history = crate::history::HistoryWriter::new(dir.to_owned());
        if let Err(e) = history.append(
            None,
            &restart_msgs,
            estimated_tokens,
            crate::history::RecordKind::System,
            Some(crate::history::SystemEvent::AgentRestore),
        ) {
            tracing::warn!("history restore record failed: {e:#}");
        }
    }

    Ok(RestorableAgent {
        agent_id: agent_id.clone(),
        agent_dir: dir.to_owned(),
        store,
        approvals,
        degraded,
    })
}

/// Validate tool-call/result pairing across every turn and record violations
/// as degradations — at most one finding per turn per direction, naming the
/// turn and the offending ids. Report-only by design: a dangling call would
/// be rejected by the provider on the next round (which is what the warning
/// buys time to notice), and the explicit repair operation —
/// [`repair_agent_context`], not restore — is what rewrites history to
/// fix it. Pinned turns carry no tool traffic, so they pass through clean.
fn check_tool_pairing(store: &ContextStore, degraded: &mut Vec<Degradation>) {
    for turn in store.turns() {
        let unanswered = crate::tool_execution::unanswered_call_ids(&turn.messages);
        if !unanswered.is_empty() {
            let ids: Vec<&str> = unanswered.iter().map(|(id, _)| id.as_str()).collect();
            degraded.push(Degradation {
                kind: DegradationKind::UnansweredToolCalls,
                detail: format!(
                    "turn {} declares tool calls with no result: {ids:?}",
                    turn.id.0
                ),
            });
        }
        let orphan = crate::tool_execution::orphan_result_ids(&turn.messages);
        if !orphan.is_empty() {
            degraded.push(Degradation {
                kind: DegradationKind::OrphanToolResults,
                detail: format!(
                    "turn {} carries tool results answering no call: {orphan:?}",
                    turn.id.0
                ),
            });
        }
    }
}

/// One turn's pairing damage and the fix applied to it.
#[derive(Debug, Clone)]
pub struct RepairAction {
    /// The damaged turn's ID — the repaired history record keeps it.
    pub turn_id: u64,
    /// Dangling tool-call ids that got a synthesized not-executed result.
    pub unanswered_calls: Vec<String>,
    /// Orphan tool-result ids whose messages were removed.
    pub orphan_results: Vec<String>,
}

/// Outcome of a [`repair_agent_context`] pass.
#[derive(Debug)]
pub struct RepairReport {
    /// Damage absorbed while loading (backup fallback, tail recovery, …):
    /// repair rides the normal degradation chain, so the report doubles as
    /// a health check.
    pub degraded: Vec<Degradation>,
    /// The repairs applied — or, in a dry run, the repairs that would be.
    pub actions: Vec<RepairAction>,
}

/// Repair tool-call/result pairing damage in an agent's persisted context.
///
/// Every conversation turn violating the pairing invariant — dangling calls
/// (answered with a synthesized not-executed error, the honest outcome for
/// a call that never ran) or orphan results (removed) — is fixed in memory,
/// and the repaired messages are appended to today's history file under the
/// same turn ID. Both history readers scan newest-first, so the appended
/// record wins over the damaged one, the log stays append-only, and the
/// manifest needs no write: turn IDs are unchanged, and split persistence
/// keeps message content only in history.
///
/// `execute: false` is a dry run — `actions` lists what would be done
/// and nothing is written, not even the deferred legacy→split migration,
/// which only a restore completes (see the comment at `load_store`).
///
/// Idempotent: a repaired record is itself clean, so a second pass finds
/// nothing to append.
pub fn repair_agent_context(
    agent_dir: &Path,
    tail_budget: usize,
    execute: bool,
) -> Result<RepairReport> {
    let mut degraded = Vec::new();
    // Repair never persists the store, so a legacy directory's deferred
    // migration stays pending for the next restore to finish.
    let (mut store, _migrate_pending) = load_store(agent_dir, tail_budget, &mut degraded)?;
    let history = crate::history::HistoryWriter::new(agent_dir.to_owned());
    let mut actions = Vec::new();

    for turn in store.turns_mut() {
        // Pinned turns carry no tool traffic (see `check_tool_pairing`).
        if turn.is_pinned() {
            continue;
        }
        let unanswered = crate::tool_execution::unanswered_call_ids(&turn.messages);
        let orphan = crate::tool_execution::orphan_result_ids(&turn.messages);
        if unanswered.is_empty() && orphan.is_empty() {
            continue;
        }
        actions.push(RepairAction {
            turn_id: turn.id.0,
            unanswered_calls: unanswered.iter().map(|(id, _)| id.clone()).collect(),
            orphan_results: orphan.clone(),
        });
        if !execute {
            continue;
        }

        // Synthesis mirrors `tool_execution::synthesize_unanswered_results`
        // minus the live-loop specifics: no park target is recorded for a
        // dangling `break`, so every dangling call — `break` included — gets
        // the honest not-executed error.
        for (id, name) in unanswered {
            let content = crate::policy::error_result(
                &name,
                "not executed: no result was ever recorded for this call (offline repair)"
                    .to_owned(),
            );
            turn.messages.push(ChatMessage::tool_result(content, id));
        }
        let orphan_ids: Vec<&str> = orphan.iter().map(String::as_str).collect();
        turn.messages.retain(|m| {
            m.tool_call_id()
                .is_none_or(|rid| !orphan_ids.contains(&rid))
        });

        turn.estimated_tokens = crate::context::Turn::estimate_tokens(&turn.messages);
        history
            .append(
                Some(turn.id.0),
                &turn.messages,
                turn.estimated_tokens,
                crate::history::RecordKind::Turn,
                None,
            )
            .with_context(|| format!("appending repaired turn {} to history", turn.id.0))?;
    }

    Ok(RepairReport { degraded, actions })
}

/// Load the context store in the format the directory holds.
///
/// `manifest.json` (or its backup) present → split format: parse both
/// documents and hydrate the conversation window from history by turn
/// ID. A leftover `context.json` (migration interrupted before the
/// rename) is finished off. Only `context.json` → legacy format: load
/// it and strip restart notices; the split documents are written by
/// the caller after its transformations (the flag in the return says
/// when). Neither split document but history exists → the window is
/// rebuilt from the history tail. Nothing at all → fresh store.
fn load_store(
    dir: &Path,
    tail_budget: usize,
    degraded: &mut Vec<Degradation>,
) -> Result<(ContextStore, bool)> {
    let manifest_path = dir.join("manifest.json");
    let legacy_path = dir.join("context.json");

    if manifest_path.exists() || backup_path(&manifest_path).exists() {
        if legacy_path.exists()
            && let Err(e) = fs::rename(&legacy_path, legacy_archive_path(dir))
        {
            tracing::warn!("finishing legacy rename failed: {e:#}");
        }

        let manifest =
            match load_with_backup::<crate::context::manifest::ManifestDoc>(&manifest_path) {
                Ok((doc, false)) => doc,
                Ok((doc, true)) => {
                    tracing::warn!("manifest.json unreadable or missing, recovered from backup");
                    doc
                }
                // Both wrecked: rebuild the window from the history tail instead
                // of faulting the agent (the manifest alone is not worth a
                // Faulted — history and pins survive independently).
                Err(e) => {
                    return Ok((
                        rebuild_window_from_tail(dir, tail_budget, degraded, e),
                        false,
                    ));
                }
            };
        let pins = load_pins(dir, degraded);

        let (convo, report) = crate::history::hydrate_turns(dir, &manifest.conversation_turn_ids);
        if report.bad_lines > 0 {
            degraded.push(Degradation {
                kind: DegradationKind::BadHistoryLines,
                detail: format!(
                    "{} history lines failed to parse and were skipped",
                    report.bad_lines
                ),
            });
        }
        if !report.missing_ids.is_empty() {
            degraded.push(Degradation {
                kind: DegradationKind::MissingHistoryTurns,
                detail: format!("no history record for turns {:?}", report.missing_ids),
            });
        }
        return Ok((ContextStore::from_persisted(&pins, convo, &manifest), false));
    }

    if legacy_path.exists() {
        let mut store: ContextStore = serde_json::from_str(
            &fs::read_to_string(&legacy_path).context("reading context.json")?,
        )
        .context("parsing context.json")?;
        strip_restart_turns(&mut store);
        // Deferred migration: the caller writes the split documents after
        // its folds; writing here would persist the raw legacy shape.
        return Ok((store, true));
    }

    // Neither split document: with no history this is a fresh directory;
    // with history the manifest went missing, and a fresh store would
    // renumber turn IDs from zero, aliasing every history record.
    if crate::history::has_history(dir) {
        return Ok((
            rebuild_window_from_tail(
                dir,
                tail_budget,
                degraded,
                anyhow::anyhow!("manifest.json and its backup are missing"),
            ),
            false,
        ));
    }
    Ok((ContextStore::new(), false))
}

/// Load pins.json through its backup chain, degrading to empty pins
/// (recorded) when both are unreadable. Pins are re-creatable by re-pinning,
/// so a wrecked pins file boots the agent rather than faulting it.
fn load_pins(dir: &Path, degraded: &mut Vec<Degradation>) -> crate::context::manifest::PinsDoc {
    match load_with_backup::<crate::context::manifest::PinsDoc>(&dir.join("pins.json")) {
        Ok((doc, false)) => doc,
        Ok((doc, true)) => {
            tracing::warn!("pins.json unreadable or missing, recovered from backup");
            doc
        }
        Err(e) => {
            degraded.push(Degradation {
                kind: DegradationKind::PinsLost,
                detail: format!(
                    "pins.json and its backup are unreadable or missing ({e:#}); booted with empty pins"
                ),
            });
            crate::context::manifest::PinsDoc {
                version: crate::context::manifest::FORMAT_VERSION,
                pins: Vec::new(),
            }
        }
    }
}

/// Last-resort window rebuild when manifest.json and its backup are both
/// unreadable: pins keep their own file (and backup chain), the conversation
/// window is rehydrated from the newest history turns that fit `tail_budget`,
/// and `next_turn_id` is rescanned from history (`max + 1`) — reusing an ID
/// would silently alias history records and manifest references with
/// different content. Leading turns with pairing damage are dropped up to
/// the first clean boundary: a dangling call or orphan result at the window
/// head is a guaranteed provider rejection on the next round. Usage counters
/// and the retry log lived only in the manifest and are lost (reset to zero,
/// recorded).
fn rebuild_window_from_tail(
    dir: &Path,
    tail_budget: usize,
    degraded: &mut Vec<Degradation>,
    manifest_err: anyhow::Error,
) -> ContextStore {
    let pins = load_pins(dir, degraded);
    let mut turns = crate::history::tail_turns_within_budget(dir, tail_budget);
    let tail_len = turns.len();
    let damaged_leading = turns
        .iter()
        .take_while(|t| pairing_damaged(&t.messages))
        .count();
    turns.drain(..damaged_leading);
    let next_turn_id = crate::history::max_turn_id(dir) + 1;
    degraded.push(Degradation {
        kind: DegradationKind::TailRecovery,
        detail: format!(
            "manifest.json and its backup are unreadable or missing ({manifest_err:#}); rebuilt the window from the history tail: kept {} of {tail_len} turns within a {tail_budget}-token budget (dropped {damaged_leading} pairing-damaged leading turns); next_turn_id rescanned to {next_turn_id}; cumulative usage and retry log lost (reset)",
            turns.len()
        ),
    });
    let manifest = crate::context::manifest::ManifestDoc {
        version: crate::context::manifest::FORMAT_VERSION,
        conversation_turn_ids: turns.iter().map(|t| t.id.0).collect(),
        cumulative_usage: Default::default(),
        next_turn_id,
        retry_log: Vec::new(),
    };
    ContextStore::from_persisted(&pins, turns, &manifest)
}

/// Whether a turn's messages violate tool-call/result pairing in either
/// direction (`tool_execution::unanswered_call_ids` and its mirror).
fn pairing_damaged(messages: &[ChatMessage]) -> bool {
    !crate::tool_execution::unanswered_call_ids(messages).is_empty()
        || !crate::tool_execution::orphan_result_ids(messages).is_empty()
}

fn legacy_archive_path(dir: &Path) -> PathBuf {
    dir.join("context.legacy.json")
}

/// Remove prior restart notices from a legacy store before migration: they
/// are on-the-spot prompts, meaningless across restarts, and have no
/// history record to hydrate from. Matched by content against the current
/// message; wording drift in an older notice leaves the turn in place, where
/// the missing-ID degradation absorbs it.
fn strip_restart_turns(store: &mut ContextStore) {
    let restart_positions: Vec<usize> = store
        .turns()
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            !t.is_pinned()
                && t.messages.len() == 1
                && t.messages[0].content() == Some(RESTART_MESSAGE)
        })
        .map(|(i, _)| i)
        .collect();
    for i in restart_positions.into_iter().rev() {
        store.drain_turns(i..i + 1);
    }
}

/// Migrate a legacy store to split persistence, in an order that leaves no
/// losing intermediate state: pins first, manifest second, legacy rename
/// last. `manifest.json` present is the completed-migration marker, so a
/// crash after pins but before manifest re-runs this whole path from the
/// intact `context.json`; a crash after manifest but before the rename takes
/// the split path on restart and the dispatcher finishes the rename.
fn migrate_legacy_to_split(dir: &Path, store: &ContextStore) -> Result<()> {
    let pins = serde_json::to_string(&store.to_pins_doc()).context("serializing pins.json")?;
    let manifest =
        serde_json::to_string(&store.to_manifest_doc()).context("serializing manifest.json")?;
    write_with_backup(&dir.join("pins.json"), &pins)?;
    write_with_backup(&dir.join("manifest.json"), &manifest)?;
    fs::rename(dir.join("context.json"), legacy_archive_path(dir))?;
    Ok(())
}

const RESTART_MESSAGE: &str = concat!(
    "[system]\n",
    "Agent restored from a previous state. Shell sessions have been reset \u{2014}\n",
    "environment variables, working directory, and background processes are no\n",
    "longer available. Review the current state of the project and re-establish\n",
    "any necessary conditions before continuing. Directory write-locks are managed by\n",
    "the system for the lifetime of your task and were re-established on restore; they\n",
    "need no action from you.\n"
);

#[cfg(test)]
mod tests {
    use super::*;

    use crate::context::AgenticContext as _;
    use crate::history::{HistoryWriter, RecordKind};
    use crate::test_support::{
        TurnMessage, assistant_msg, tool_calls_msg, tool_result_msg, user_msg,
    };
    use serial_test::serial;
    use tempfile::TempDir;

    /// Tail budget for restores that should not truncate: every fixture's
    /// history is far below this. Truncation itself is tested with explicit
    /// budgets.
    const NO_TRUNCATION: usize = 65_536;

    // --- split persistence (manifest/pins) ---

    #[test]
    fn persist_writes_manifest_and_pins_with_backup() {
        let dir = TempDir::new().unwrap();
        let mut store = ContextStore::new();
        store.pin("note", assistant_msg("keep")).unwrap();
        store.push_turn(vec![user_msg("hello")]);
        let first_ids = store.to_manifest_doc().conversation_turn_ids.clone();

        persist_context(&store, dir.path()).unwrap();
        let manifest_v1 = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
        assert!(dir.path().join("pins.json").exists());
        assert!(
            !dir.path().join("manifest.json.bak").exists(),
            "no backup on first write"
        );

        // Second turn changes the manifest; the backup keeps version 1.
        store.push_turn(vec![user_msg("more")]);
        persist_context(&store, dir.path()).unwrap();
        let manifest_v2 = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
        let bak = std::fs::read_to_string(dir.path().join("manifest.json.bak")).unwrap();
        assert_eq!(bak, manifest_v1);
        assert_ne!(manifest_v2, manifest_v1);

        let doc: crate::context::manifest::ManifestDoc =
            serde_json::from_str(&manifest_v2).unwrap();
        assert_eq!(doc.conversation_turn_ids.len(), first_ids.len() + 1);
        assert!(
            !dir.path().join("context.json").exists(),
            "legacy file not written"
        );
    }

    #[test]
    fn persist_excludes_injected_turns_from_manifest() {
        let dir = TempDir::new().unwrap();
        let mut store = ContextStore::new();
        let kept = store.push_turn(vec![user_msg("kept")]).0;
        let injected = store.push_turn(vec![user_msg("[system] restart")]).0;
        store.register_injected_turn(injected);

        persist_context(&store, dir.path()).unwrap();
        let manifest = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
        let doc: crate::context::manifest::ManifestDoc = serde_json::from_str(&manifest).unwrap();
        assert_eq!(doc.conversation_turn_ids, vec![kept.0]);
        assert_eq!(doc.next_turn_id, injected.0 + 1);
    }

    // --- restore: migration + hydration ---

    /// A legacy agent directory: whole-store `context.json` plus matching
    /// history records for the conversation turns.
    fn legacy_fixture() -> (ContextStore, Vec<TurnMessage>) {
        let mut store = ContextStore::new();
        store.pin("note", assistant_msg("pinned note")).unwrap();
        store.push_turn(vec![user_msg("first question")]);
        store.push_turn(vec![assistant_msg("first answer")]);
        store.retry_log.push(kallip_common::retry::RetryRecord {
            timestamp: 9,
            round: 0,
            attempt: 1,
            max_attempts: 3,
            error: "legacy".into(),
            delay_secs: 1.0,
            endpoint: None,
            kind: kallip_common::retry::RetryKind::Transport,
            quota_reset: None,
        });
        // Expected conversation window, in order, for history.
        let convo = vec![user_msg("first question"), assistant_msg("first answer")];
        (store, convo)
    }

    fn write_legacy(
        dir: &TempDir,
        store: &ContextStore,
        with_history: bool,
        convo: &[TurnMessage],
    ) {
        std::fs::write(
            dir.path().join("context.json"),
            serde_json::to_string(store).unwrap(),
        )
        .unwrap();
        if with_history {
            let history = HistoryWriter::new(dir.path().to_path_buf());
            let ids: Vec<u64> = store
                .turns()
                .iter()
                .filter(|t| !t.is_pinned())
                .map(|t| t.id.0)
                .collect();
            for (id, msg) in ids.iter().zip(convo) {
                history
                    .append(
                        Some(*id),
                        std::slice::from_ref(msg),
                        8,
                        RecordKind::Turn,
                        None,
                    )
                    .unwrap();
            }
        }
    }

    #[test]
    fn migrate_writes_split_files_renames_legacy_and_strips_restart_turns() {
        let dir = TempDir::new().unwrap();
        let (mut store, convo) = legacy_fixture();
        // A restart notice from a previous restore lives in the legacy store.
        store.push_turn(vec![user_msg(RESTART_MESSAGE)]);
        write_legacy(&dir, &store, true, &convo);
        let legacy_next = store.to_manifest_doc().next_turn_id;

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        // Split files exist; the legacy file is archived, not deleted.
        assert!(dir.path().join("manifest.json").exists());
        assert!(dir.path().join("pins.json").exists());
        assert!(dir.path().join("context.legacy.json").exists());
        assert!(!dir.path().join("context.json").exists());

        let manifest = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
        let doc: crate::context::manifest::ManifestDoc = serde_json::from_str(&manifest).unwrap();
        assert_eq!(
            doc.conversation_turn_ids,
            vec![1, 2],
            "restart notice excluded"
        );
        assert_eq!(doc.next_turn_id, legacy_next);
        assert!(
            restored.degraded.is_empty(),
            "clean migration degrades nothing"
        );
    }

    #[test]
    fn migration_interrupted_after_pins_reruns_legacy_path_idempotently() {
        let dir = TempDir::new().unwrap();
        let (store, convo) = legacy_fixture();
        write_legacy(&dir, &store, true, &convo);
        // Simulate a crash after pins.json but before manifest.json.
        std::fs::write(
            dir.path().join("pins.json"),
            serde_json::to_string(&store.to_pins_doc()).unwrap(),
        )
        .unwrap();

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        // The dispatcher saw no manifest → full legacy migration re-ran.
        assert!(
            dir.path().join("manifest.json").exists(),
            "migration completed"
        );
        assert!(dir.path().join("context.legacy.json").exists());
        assert!(!dir.path().join("context.json").exists());
        assert!(restored.degraded.is_empty());
    }

    #[test]
    fn migration_interrupted_before_rename_takes_split_path_losing_nothing() {
        let dir = TempDir::new().unwrap();
        let (store, convo) = legacy_fixture();
        write_legacy(&dir, &store, true, &convo);
        // Simulate a crash after both split writes but before the rename:
        // manifest exists, context.json still in place.
        std::fs::write(
            dir.path().join("pins.json"),
            serde_json::to_string(&store.to_pins_doc()).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            serde_json::to_string(&store.to_manifest_doc()).unwrap(),
        )
        .unwrap();

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        // Split path taken; the interrupted rename is finished here.
        assert!(dir.path().join("context.legacy.json").exists());
        assert!(!dir.path().join("context.json").exists());
        let window = restored.store.turns();
        assert_eq!(window.len(), 4, "pin + 2 hydrated + fresh restart notice");
        assert!(restored.degraded.is_empty());
    }

    #[test]
    fn legacy_restore_hydrates_equivalent_store() {
        let dir = TempDir::new().unwrap();
        let (store, convo) = legacy_fixture();
        write_legacy(&dir, &store, true, &convo);
        let legacy_ids: Vec<u64> = store
            .turns()
            .iter()
            .filter(|t| !t.is_pinned())
            .map(|t| t.id.0)
            .collect();
        let legacy_next = store.to_manifest_doc().next_turn_id;

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        let turns = restored.store.turns();
        assert_eq!(
            turns.front().unwrap().label(),
            Some("note"),
            "pinned layer rebuilt first"
        );
        let hydrated: Vec<u64> = turns.iter().skip(1).take(2).map(|t| t.id.0).collect();
        assert_eq!(hydrated, legacy_ids, "conversation window identical by ID");
        assert_eq!(
            turns.get(1).unwrap().messages[0].content(),
            Some("first question")
        );
        assert_eq!(restored.store.retry_log.len(), 1);
        assert_eq!(restored.store.retry_log[0].error, "legacy");
        // A second restore rehydrates from the split files, not the archive.
        let again =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
        let ids2: Vec<u64> = again
            .store
            .turns()
            .iter()
            .skip(1)
            .take(2)
            .map(|t| t.id.0)
            .collect();
        assert_eq!(ids2, legacy_ids);
        assert_eq!(again.store.retry_log.len(), 1);
        assert!(again.degraded.is_empty());
        // The rehydrated store assigns the next ID from the manifest, not below
        // any historical ID.
        assert_eq!(again.store.turns().back().unwrap().id.0, legacy_next);
    }

    /// The legacy→split write happens after restore's folds, so pins.json
    /// carries the folded legacy state (the summary pin here) rather than
    /// the raw legacy shape — a crash between load and write leaves
    /// context.json intact for the next attempt.
    #[test]
    fn migration_writes_split_documents_after_restore_folds() {
        let dir = TempDir::new().unwrap();
        let (store, convo) = legacy_fixture();
        write_legacy(&dir, &store, true, &convo);
        // Inject the legacy `summary` field: restore folds it into a pinned
        // turn after load, so the split documents must carry the fold.
        let mut doc = serde_json::to_value(&store).unwrap();
        doc["summary"] = serde_json::json!("condensed history");
        std::fs::write(dir.path().join("context.json"), doc.to_string()).unwrap();

        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        let pins = std::fs::read_to_string(dir.path().join("pins.json")).unwrap();
        assert!(
            pins.contains("condensed history"),
            "pins.json carries the folded summary: {pins}"
        );
        assert!(
            !dir.path().join("context.json").exists(),
            "archived only after the split documents landed"
        );
    }
    /// Each restore pushes a fresh restart notice; the window must never
    /// accumulate notices across restores (the strip before the push is what
    /// keeps repeated restarts from growing the context).
    #[test]
    fn restore_twice_leaves_a_single_restart_notice() {
        let dir = TempDir::new().unwrap();
        let (mut store, convo) = legacy_fixture();
        // A notice from a previous restore lives in the legacy store.
        store.push_turn(vec![user_msg(RESTART_MESSAGE)]);
        write_legacy(&dir, &store, true, &convo);
        let id = AgentId::from("twice".to_owned());
        let _ = restore_agent(&id, dir.path(), NO_TRUNCATION).unwrap();
        // Second restore now loads the split documents the first one wrote.
        let again = restore_agent(&id, dir.path(), NO_TRUNCATION).unwrap();
        let notices = again
            .store
            .turns()
            .iter()
            .filter(|t| {
                !t.is_pinned()
                    && t.messages.len() == 1
                    && t.messages[0].content() == Some(RESTART_MESSAGE)
            })
            .count();
        assert_eq!(
            notices, 1,
            "exactly one fresh notice after the second restore"
        );
        assert!(again.degraded.is_empty());
    }

    /// Inspecting (or repairing) a legacy directory writes nothing: the
    /// deferred migration is completed only by a restore.
    #[test]
    fn repair_dry_run_leaves_legacy_directory_untouched() {
        let dir = TempDir::new().unwrap();
        let (store, convo) = legacy_fixture();
        write_legacy(&dir, &store, true, &convo);

        repair_agent_context(dir.path(), NO_TRUNCATION, false).unwrap();

        assert!(!dir.path().join("manifest.json").exists());
        assert!(dir.path().join("context.json").exists());
    }
    #[test]
    fn missing_history_records_degrade_instead_of_failing() {
        let dir = TempDir::new().unwrap();
        let (store, convo) = legacy_fixture();
        // History records only the first turn; the second goes missing.
        write_legacy(&dir, &store, true, &convo[..1]);

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        // The first restore migrates: the full legacy window is in memory,
        // so nothing is degraded yet. The gap surfaces on the next restore,
        // which hydrates from the split files.
        assert!(restored.degraded.is_empty());

        let degraded =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        assert_eq!(degraded.degraded.len(), 1);
        assert_eq!(
            degraded.degraded[0].kind,
            DegradationKind::MissingHistoryTurns
        );
        let turns = degraded.store.turns();
        assert_eq!(turns.len(), 3, "pin + one hydratable turn + restart notice");
    }

    /// Pairing damage in a hydrated turn is reported, not repaired: a
    /// declared-but-unanswered call and a result answering nothing each
    /// produce their own degradation, and the persisted messages come back
    /// untouched.
    /// A persisted 400-bug-shaped turn: `c2` declared but never answered,
    /// `c9` answering nothing — both pairing directions damaged at once.
    fn pairing_damaged_dir() -> (TempDir, Vec<TurnMessage>, u64) {
        let dir = TempDir::new().unwrap();
        let mut store = ContextStore::new();
        let damaged = vec![
            tool_calls_msg(&[("c1", "read"), ("c2", "edit")]),
            tool_result_msg("ok", "c1"),
            tool_result_msg("ghost", "c9"),
        ];
        let (turn_id, _) = store.push_turn(damaged.clone());
        let history = HistoryWriter::new(dir.path().to_path_buf());
        history
            .append(Some(turn_id.0), &damaged, 8, RecordKind::Turn, None)
            .unwrap();
        persist_context(&store, dir.path()).unwrap();
        (dir, damaged, turn_id.0)
    }

    /// Total non-empty lines across the agent's history files — the
    /// dry-run/idempotency "nothing was appended" oracle.
    fn history_line_count(dir: &Path) -> usize {
        fs::read_dir(dir.join("history"))
            .unwrap()
            .map(|entry| {
                let content = fs::read_to_string(entry.unwrap().path()).unwrap();
                content.lines().filter(|l| !l.is_empty()).count()
            })
            .sum()
    }
    #[test]
    fn pairing_damage_degrades_instead_of_repairing() {
        let (dir, damaged, _turn_id) = pairing_damaged_dir();

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        let kinds: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
        assert!(kinds.contains(&DegradationKind::UnansweredToolCalls));
        assert!(kinds.contains(&DegradationKind::OrphanToolResults));
        // Report-only: the hydrated turn is byte-for-byte what history holds.
        let turns = restored.store.turns();
        assert_eq!(turns.front().unwrap().messages, damaged);
    }

    /// Repair fixes both damage directions and lands in history: a fresh
    /// restore hydrates the repaired record (newest wins) with zero
    /// pairing findings.
    #[test]
    fn repair_agent_context_fixes_pairing_damage() {
        let (dir, _damaged, turn_id) = pairing_damaged_dir();

        let report = repair_agent_context(dir.path(), NO_TRUNCATION, true).unwrap();
        assert_eq!(report.actions.len(), 1);
        let action = &report.actions[0];
        assert_eq!(action.turn_id, turn_id);
        assert_eq!(action.unanswered_calls, vec!["c2"]);
        assert_eq!(action.orphan_results, vec!["c9"]);

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
        let kinds: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
        assert!(!kinds.contains(&DegradationKind::UnansweredToolCalls));
        assert!(!kinds.contains(&DegradationKind::OrphanToolResults));

        // c2 answered with a not-executed error, c9's orphan result
        // removed, c1's real result kept.
        let turn = restored
            .store
            .turns()
            .iter()
            .find(|t| t.id.0 == turn_id)
            .unwrap();
        let ids: Vec<&str> = turn
            .messages
            .iter()
            .filter_map(|m| m.tool_call_id())
            .collect();
        assert_eq!(ids, vec!["c1", "c2"]);
        let c2 = turn
            .messages
            .iter()
            .find(|m| m.tool_call_id() == Some("c2"))
            .unwrap();
        assert!(c2.content().unwrap().contains("not executed"));
    }

    /// Dry run reports the same actions and writes nothing: no history
    /// record, no manifest churn, damage still present on the next restore.
    #[test]
    fn repair_agent_context_dry_run_writes_nothing() {
        let (dir, _damaged, _turn_id) = pairing_damaged_dir();
        let manifest_before = fs::read(dir.path().join("manifest.json")).unwrap();
        let lines_before = history_line_count(dir.path());

        let report = repair_agent_context(dir.path(), NO_TRUNCATION, false).unwrap();
        assert_eq!(report.actions.len(), 1);

        assert_eq!(history_line_count(dir.path()), lines_before);
        assert_eq!(
            fs::read(dir.path().join("manifest.json")).unwrap(),
            manifest_before
        );

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
        let kinds: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
        assert!(kinds.contains(&DegradationKind::UnansweredToolCalls));
        assert!(kinds.contains(&DegradationKind::OrphanToolResults));
    }

    /// A repaired record is clean: a second pass plans nothing and
    /// appends nothing.
    #[test]
    fn repair_agent_context_is_idempotent() {
        let (dir, _damaged, _turn_id) = pairing_damaged_dir();

        let first = repair_agent_context(dir.path(), NO_TRUNCATION, true).unwrap();
        assert_eq!(first.actions.len(), 1);
        let lines_after_first = history_line_count(dir.path());

        let second = repair_agent_context(dir.path(), NO_TRUNCATION, true).unwrap();
        assert!(second.actions.is_empty());
        assert_eq!(history_line_count(dir.path()), lines_after_first);
    }

    // --- degradation chain: backup fallback + tail recovery ---

    /// A split-format directory with real damage to inflict: a pin, three
    /// conversation turns (the newest pairing-damaged on demand), usage and
    /// retry-log state that only the manifest carries, and matching history.
    fn wreckable_dir(oldest_damaged: bool) -> (TempDir, ContextStore) {
        let dir = TempDir::new().unwrap();
        let mut store = ContextStore::new();
        store.pin("note", assistant_msg("pinned")).unwrap();
        let histories: [Vec<TurnMessage>; 3] = [
            if oldest_damaged {
                vec![tool_result_msg("orphan", "ghost"), assistant_msg("reply-a")]
            } else {
                vec![user_msg("ask-a"), assistant_msg("reply-a")]
            },
            vec![user_msg("ask-b"), assistant_msg("reply-b")],
            vec![
                tool_calls_msg(&[("c1", "read")]),
                tool_result_msg("ok", "c1"),
            ],
        ];
        let history = HistoryWriter::new(dir.path().to_path_buf());
        for msgs in histories {
            let (id, _) = store.push_turn(msgs.clone());
            history
                .append(Some(id.0), &msgs, 8, RecordKind::Turn, None)
                .unwrap();
        }
        store.accumulate_usage(&crate::test_support::usage(1000));
        store.retry_log.push(kallip_common::retry::RetryRecord {
            timestamp: 9,
            round: 0,
            attempt: 1,
            max_attempts: 3,
            error: "wreck".into(),
            delay_secs: 1.0,
            endpoint: None,
            kind: kallip_common::retry::RetryKind::Transport,
            quota_reset: None,
        });
        persist_context(&store, dir.path()).unwrap();
        // A second persist leaves the first manifest as the .bak; the churn
        // turn also reaches history, so the rescan below sees it.
        let churn = vec![user_msg("churn")];
        let (churn_id, _) = store.push_turn(churn.clone());
        let history = HistoryWriter::new(dir.path().to_path_buf());
        history
            .append(Some(churn_id.0), &churn, 8, RecordKind::Turn, None)
            .unwrap();
        persist_context(&store, dir.path()).unwrap();
        (dir, store)
    }

    /// manifest.json corrupt but the backup intact: the backup wins, the
    /// store rehydrates normally, and nothing is degraded.
    #[test]
    fn corrupt_manifest_falls_back_to_backup() {
        let (dir, _store) = wreckable_dir(false);
        std::fs::write(dir.path().join("manifest.json"), "{wreck").unwrap();

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        assert!(restored.degraded.is_empty(), "backup recovery is silent");
        let turns = restored.store.turns();
        assert_eq!(turns.len(), 5, "pin + 3 hydrated + restart notice");
        assert!(
            restored
                .store
                .usage_snapshot()
                .cumulative_usage
                .prompt_tokens
                > 0
        );
        assert_eq!(restored.store.retry_log.len(), 1);
    }

    /// manifest.json deleted but its backup intact: the backup feeds the
    /// restore (previously the existence gate skipped the whole chain),
    /// and the primary is rewritten by the next persist, not by the load.
    #[test]
    fn missing_manifest_falls_back_to_backup() {
        let (dir, _store) = wreckable_dir(false);
        std::fs::remove_file(dir.path().join("manifest.json")).unwrap();

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        assert!(restored.degraded.is_empty(), "backup recovery is silent");
        assert_eq!(
            restored.store.turns().len(),
            5,
            "pin + 3 hydrated + restart notice"
        );
        assert!(
            !dir.path().join("manifest.json").exists(),
            "rewrite happens at persist, not during restore"
        );
    }

    /// manifest.json and its backup deleted while history survives: the
    /// window rebuilds from the tail instead of booting a silently empty
    /// store that would renumber turn IDs from zero and alias history.
    #[test]
    fn missing_manifest_and_backup_rebuild_from_tail() {
        let (dir, store) = wreckable_dir(false);
        std::fs::remove_file(dir.path().join("manifest.json")).unwrap();
        std::fs::remove_file(dir.path().join("manifest.json.bak")).unwrap();

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        let kinds: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
        assert_eq!(kinds, vec![DegradationKind::TailRecovery]);
        assert!(
            restored.degraded[0].detail.contains("missing"),
            "detail names the missing-documents case: {}",
            restored.degraded[0].detail
        );
        assert_eq!(
            restored.store.turns().len(),
            6,
            "pin + 4 tail turns + restart notice (full budget keeps all)"
        );
        // Rescanned from history: the restart notice took history max + 1,
        // the first fresh turn one past that — no ID is reused.
        let history_max = store.to_manifest_doc().next_turn_id - 1;
        let mut live = restored.store;
        let (fresh_id, _) = live.push_turn(vec![user_msg("after loss")]);
        assert_eq!(fresh_id.0, history_max + 2, "rescanned past history max");
    }
    /// manifest.json and .bak both corrupt: the window rebuilds from the
    /// history tail within the budget, manifest-only state (usage, retry
    /// log) is lost and recorded, and `next_turn_id` is rescanned from
    /// history so no ID is ever reused.
    #[test]
    fn double_corrupt_manifest_rebuilds_from_tail() {
        let (dir, store) = wreckable_dir(false);
        std::fs::write(dir.path().join("manifest.json"), "{wreck").unwrap();
        std::fs::write(dir.path().join("manifest.json.bak"), "{wreck").unwrap();

        let restored = restore_agent(&AgentId::from("a".to_owned()), dir.path(), 24).unwrap();

        let tail: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
        assert_eq!(tail, vec![DegradationKind::TailRecovery]);
        let turns = restored.store.turns();
        // 24-token budget: the three newest turns (churn, tool round, ask-b)
        // fit exactly; ask-a would cross — and the restart notice is added after.
        assert_eq!(turns.len(), 5, "pin + 3 tail turns + restart notice");
        let ids: Vec<u64> = turns.iter().skip(1).take(3).map(|t| t.id.0).collect();
        assert_eq!(ids, vec![2, 3, 4], "newest three, ascending, ask-a dropped");
        assert_eq!(
            restored
                .store
                .usage_snapshot()
                .cumulative_usage
                .prompt_tokens,
            0
        );
        assert!(restored.store.retry_log.is_empty());

        // Rescanned next_turn_id: history max is the churn turn's ID, and
        // the manifest rewrite references only kept turns — no collision.
        let history_max = store.to_manifest_doc().next_turn_id - 1;
        let mut live = restored.store;
        let fresh = vec![user_msg("after wreck")];
        let (fresh_id, _) = live.push_turn(fresh.clone());
        // The agent task records every turn to history before persisting;
        // mirror that so the rehydrate below finds the fresh turn.
        HistoryWriter::new(dir.path().to_path_buf())
            .append(Some(fresh_id.0), &fresh, 8, RecordKind::Turn, None)
            .unwrap();
        // The restart notice (pushed by restore) took history_max + 1; the
        // first fresh turn is one past that — neither reuses a historical ID.
        assert_eq!(
            fresh_id.0,
            history_max + 2,
            "rescanned from history, never reused"
        );
        persist_context(&live, dir.path()).unwrap();
        let again =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
        assert!(
            again.degraded.is_empty(),
            "wrecked files were rewritten clean"
        );
    }

    /// A pairing-damaged turn at the truncation boundary is dropped (up to
    /// the first clean turn), and the drop count is recorded — a dangling
    /// call or orphan result at the window head would be rejected by the
    /// provider on the next round.
    #[test]
    fn tail_recovery_drops_damaged_leading_turns() {
        let (dir, _store) = wreckable_dir(true);
        std::fs::write(dir.path().join("manifest.json"), "{wreck").unwrap();
        std::fs::write(dir.path().join("manifest.json.bak"), "{wreck").unwrap();

        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

        assert_eq!(restored.degraded[0].kind, DegradationKind::TailRecovery);
        assert!(restored.degraded[0].detail.contains("dropped 1"));
        let turns = restored.store.turns();
        assert_eq!(turns.len(), 5, "pin + 3 clean turns + restart notice");
        assert_eq!(turns.get(1).unwrap().messages[0].content(), Some("ask-b"));
    }

    /// pins.json corrupt falls back to its backup; both corrupt boots with
    /// empty pins, recorded as a degradation.
    #[test]
    fn corrupt_pins_falls_back_then_boots_empty() {
        let (dir, _store) = wreckable_dir(false);

        // Backup intact: silent recovery, pin still there.
        std::fs::write(dir.path().join("pins.json"), "{wreck").unwrap();
        let restored =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
        assert!(restored.degraded.is_empty());
        assert_eq!(
            restored.store.turns().front().unwrap().label(),
            Some("note")
        );

        // Both wrecked: empty pins, recorded, conversation window intact.
        std::fs::write(dir.path().join("pins.json.bak"), "{wreck").unwrap();
        let degraded =
            restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
        assert_eq!(degraded.degraded.len(), 1);
        assert_eq!(degraded.degraded[0].kind, DegradationKind::PinsLost);
        let turns = degraded.store.turns();
        assert!(turns.front().unwrap().label().is_none(), "no pin layer");
        assert_eq!(turns.len(), 5, "4 hydrated turns + restart notice");
    }
    /// Manual migration drill against a copy of a real agent directory:
    /// `KALLIP_MIGRATION_DRILL_DIR=<agent dir> cargo test -p kallip-runtime
    /// real_dir_migration_drill -- --ignored --nocapture`. Verifies the
    /// production-shape directory migrates, rehydrates losslessly, and
    /// prints the size split. Never touches the source directory.
    #[test]
    #[ignore = "manual drill: set KALLIP_MIGRATION_DRILL_DIR"]
    fn real_dir_migration_drill() {
        let src = std::env::var("KALLIP_MIGRATION_DRILL_DIR")
            .expect("set KALLIP_MIGRATION_DRILL_DIR to a real agent directory");
        let tmp = TempDir::new().unwrap();
        copy_dir_all(std::path::Path::new(&src), tmp.path()).unwrap();
        let id = AgentId::from(
            std::path::Path::new(&src)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );

        let first = restore_agent(&id, tmp.path(), NO_TRUNCATION).unwrap();
        println!(
            "first restore (migration): {} degradation(s)",
            first.degraded.len()
        );
        for d in &first.degraded {
            println!("  {:?}: {}", d.kind, d.detail);
        }

        let second = restore_agent(&id, tmp.path(), NO_TRUNCATION).unwrap();
        println!(
            "second restore (rehydrate): {} degradation(s)",
            second.degraded.len()
        );
        for d in &second.degraded {
            println!("  {:?}: {}", d.kind, d.detail);
        }
        assert_eq!(second.store.turn_count(), first.store.turn_count());
        assert_eq!(second.store.retry_log.len(), first.store.retry_log.len());

        let size = |name: &str| {
            std::fs::metadata(tmp.path().join(name))
                .map(|m| m.len())
                .unwrap_or(0)
        };
        println!(
            "legacy context.json: {} bytes -> manifest.json: {} + pins.json: {} (per-turn writes)",
            size("context.legacy.json"),
            size("manifest.json"),
            size("pins.json"),
        );
    }

    #[test]
    fn agent_meta_round_trips() {
        let meta = AgentMeta {
            workspace_root: PathBuf::from("/app"),
            created_by: None,
            role: "researcher".into(),
            description: "gathers sources".into(),
            profile_set: Some("research".into()),
            permissions_class: crate::config::PermissionClass::Guest,
            delegation_mode: crate::config::DelegationMode::CarveOut,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: AgentMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.workspace_root, PathBuf::from("/app"));
        assert_eq!(back.role, "researcher");
        assert_eq!(back.description, "gathers sources");
        assert_eq!(back.profile_set.as_deref(), Some("research"));
        assert_eq!(
            back.permissions_class,
            crate::config::PermissionClass::Guest
        );
        assert_eq!(
            back.delegation_mode,
            crate::config::DelegationMode::CarveOut
        );
        // The snake_case spelling round-trips for FullHandoff too.
        let fh = AgentMeta {
            delegation_mode: crate::config::DelegationMode::FullHandoff,
            ..meta
        };
        let back2: AgentMeta = serde_json::from_str(&serde_json::to_string(&fh).unwrap()).unwrap();
        assert_eq!(
            back2.delegation_mode,
            crate::config::DelegationMode::FullHandoff
        );
    }

    #[test]
    fn agent_meta_pins_on_disk_enum_spellings() {
        // The two AgentMeta enums serialize in DIFFERENT cases by design:
        // PermissionClass keeps a PascalCase persisted form ("Guest"), while
        // DelegationMode uses snake_case ("full_handoff"). Pin both spellings
        // so a future "normalize the cases" refactor knows exactly what it
        // breaks (legacy meta.json files on disk carry these literals).
        let meta = AgentMeta {
            workspace_root: PathBuf::from("/app"),
            created_by: None,
            role: String::new(),
            description: String::new(),
            profile_set: None,
            permissions_class: crate::config::PermissionClass::Guest,
            delegation_mode: crate::config::DelegationMode::FullHandoff,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(
            json.contains(r#""permissions_class":"Guest""#),
            "PermissionClass persists PascalCase; got: {json}"
        );
        assert!(
            json.contains(r#""delegation_mode":"full_handoff""#),
            "DelegationMode persists snake_case; got: {json}"
        );
    }

    #[test]
    fn agent_meta_loads_legacy_file_without_optional_fields() {
        // A meta.json written before optional fields existed still restores.
        let legacy = r#"{
            "workspace_root": "/app",
            "created_by": null
        }"#;
        let meta: AgentMeta = serde_json::from_str(legacy).unwrap();
        assert_eq!(meta.workspace_root, PathBuf::from("/app"));
        // String fields default to empty; enum fields default to their enum default.
        assert_eq!(meta.role, "");
        assert_eq!(meta.description, "");
        assert_eq!(
            meta.delegation_mode,
            crate::config::DelegationMode::CarveOut
        );
    }

    // ----- archive-on-remove tests -----
    // These mutate the process-global KALLIP_TAGMA_SLUG/XDG_DATA_HOME pair, so they are
    // serialized (serial_test) and each scopes a tempfile::TempDir via temp_env;
    // the data root is `<tmp>/kallipai/tagmata/test-instance` — use
    // `data_dir_root()` inside the closure instead of `tmp.path()` directly.
    fn with_data_dir<R>(f: impl FnOnce(&TempDir) -> R) -> R {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        temp_env::with_vars(
            [
                ("KALLIP_TAGMA_SLUG", Some("test-instance")),
                ("XDG_DATA_HOME", Some(path.as_str())),
            ],
            || f(&tmp),
        )
    }

    // ----- workspace/data-dir overlap guard tests -----
    // The guard backs the data-dir integrity baseline: with no workspace↔data
    // overlap, landlock alone keeps the agent out of tagma bookkeeping.

    #[test]
    #[serial]
    fn overlap_detects_workspace_inside_data_root() {
        with_data_dir(|_| {
            // Workspace nested under the slug-derived root → overlap.
            // Exercises the `ws.starts_with(&data)` direction.
            let ws = data_dir_root().unwrap().join("agents/x");
            std::fs::create_dir_all(&ws).unwrap();
            assert!(
                workspace_overlaps_data_root(&ws).unwrap(),
                "workspace inside data root must be detected as overlap"
            );
        });
    }

    #[test]
    #[serial]
    fn overlap_detects_workspace_equal_to_data_root() {
        with_data_dir(|_| {
            // workspace == data root → overlap (degenerate case); equal
            // paths satisfy both `starts_with` directions.
            let ws = data_dir_root().unwrap();
            // The workspace must exist (AgentConfig canonicalizes it); the
            // fresh data root does not exist yet, which is exactly the
            // notional-ancestor case canonical_data_root handles.
            std::fs::create_dir_all(&ws).unwrap();
            assert!(
                workspace_overlaps_data_root(&ws).unwrap(),
                "workspace equal to data root must be detected as overlap"
            );
        });
    }

    #[test]
    #[serial]
    fn overlap_detects_workspace_containing_data_root() {
        with_data_dir(|tmp| {
            // workspace is a strict ancestor of the data root (the workspace ==
            // $HOME case, the most dangerous: the broad write grant covers the
            // whole data tree). Exercises the `data.starts_with(&ws)` direction.
            // The tmp root itself is the smallest existing on-disk ancestor
            // (the slug-derived root hangs three levels beneath it).
            let ws = tmp.path().to_path_buf();
            assert!(
                workspace_overlaps_data_root(&ws).unwrap(),
                "workspace containing data root must be detected as overlap"
            );
        });
    }

    #[test]
    #[serial]
    fn overlap_rejects_disjoint_workspace() {
        with_data_dir(|_tmp| {
            // A workspace entirely outside the data tree → no overlap.
            let ws = TempDir::new().unwrap();
            assert!(
                !workspace_overlaps_data_root(ws.path()).unwrap(),
                "disjoint workspace must not be flagged as overlap"
            );
        });
    }

    #[test]
    #[serial]
    fn overlap_rejects_sibling_prefix() {
        with_data_dir(|_| {
            // A sibling whose leaf name is a string prefix of the data root's
            // leaf (`test-instance` → `test-instanc`) must NOT be flagged:
            // `Path::starts_with` is component-wise, not byte-wise. Guards
            // against a future regression to a byte-prefix comparison. The
            // candidate must exist on disk so `canonicalize` succeeds (the guard
            // fails closed — returning Err — on a non-existent workspace).
            let root = data_dir_root().unwrap();
            let parent = root.parent().unwrap();
            let leaf = root.file_name().unwrap().to_str().unwrap();
            let ws = parent.join(&leaf[..leaf.len() - 1]);
            std::fs::create_dir_all(&ws).unwrap();
            assert!(
                !workspace_overlaps_data_root(&ws).unwrap(),
                "sibling prefix must not be flagged as overlap (component-wise check)"
            );
        });
    }

    #[test]
    #[serial]
    fn overlap_fails_closed_on_nonexistent_workspace() {
        with_data_dir(|tmp| {
            // A workspace that cannot be canonicalized (does not exist) must
            // surface an error rather than silently allow a potential overlap.
            let ws = tmp.path().join("does-not-exist");
            assert!(
                workspace_overlaps_data_root(&ws).is_err(),
                "non-canonicalizable workspace must fail closed"
            );
        });
    }

    #[test]
    #[serial]
    fn archive_moves_dir_and_preserves_contents() {
        with_data_dir(|_| {
            let id = AgentId::from("archive-rt-1".to_owned());
            let dir = create_agent_dir(
                &id,
                AgentMeta {
                    workspace_root: Path::new("/app").to_path_buf(),
                    created_by: None,
                    role: String::new(),
                    description: String::new(),
                    profile_set: None,
                    permissions_class: crate::config::PermissionClass::Normal,
                    delegation_mode: crate::config::DelegationMode::CarveOut,
                },
            )
            .unwrap();

            // One history record (as the live writer would produce).
            HistoryWriter::new(dir.clone())
                .append(Some(0), &[user_msg("hello")], 16, RecordKind::Turn, None)
                .unwrap();
            // A context.json carrying non-zero cumulative usage.
            std::fs::write(
                dir.join("context.json"),
                r#"{"cumulative_usage":{"prompt_tokens":100,"completion_tokens":50,"cache_hit_tokens":10}}"#,
            )
            .unwrap();

            archive_agent_dir(&id).unwrap();

            assert!(!dir.exists(), "agent dir must be gone from agents/");
            let archived = archived_dir(&id).unwrap();
            assert!(
                archived.join("history").exists(),
                "history survives archival"
            );
            let ctx: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(archived.join("context.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(ctx["cumulative_usage"]["prompt_tokens"], 100);
            // Live and archived trees share one root (the slug-derived data
            // root; single filesystem here, so `rename` is atomic — a cross-fs
            // EXDEV falls back to copy + delete).
            let root = data_dir_root().unwrap();
            assert!(root.join("agents").exists());
            assert!(root.join("archived").exists());
        })
    }

    #[test]
    #[serial]
    fn scan_agents_ignores_archived() {
        with_data_dir(|_| {
            let id = AgentId::from("scan-ignores-1".to_owned());
            create_agent_dir(
                &id,
                AgentMeta {
                    workspace_root: Path::new("/app").to_path_buf(),
                    created_by: None,
                    role: String::new(),
                    description: String::new(),
                    profile_set: None,
                    permissions_class: crate::config::PermissionClass::Normal,
                    delegation_mode: crate::config::DelegationMode::CarveOut,
                },
            )
            .unwrap();
            archive_agent_dir(&id).unwrap();

            let (pending, refused) = scan_agents().expect("scan");
            assert!(
                pending.iter().all(|p| p.agent_id != id),
                "archived agent must not be eligible for restore"
            );
            assert!(refused.is_empty());
        })
    }
    #[test]
    #[serial]
    fn scan_agents_reports_unreadable_meta_as_refused() {
        with_data_dir(|_| {
            let id = AgentId::from("scan-refused-1".to_owned());
            create_agent_dir(
                &id,
                AgentMeta {
                    workspace_root: Path::new("/app").to_path_buf(),
                    created_by: None,
                    role: String::new(),
                    description: String::new(),
                    profile_set: None,
                    permissions_class: crate::config::PermissionClass::Normal,
                    delegation_mode: crate::config::DelegationMode::CarveOut,
                },
            )
            .unwrap();
            // Corrupt the meta so the directory cannot be scanned.
            std::fs::write(agent_dir(&id).unwrap().join("meta.json"), "not json").unwrap();
            let (pending, refused) = scan_agents().expect("scan");
            assert!(pending.iter().all(|p| p.agent_id != id));
            let hit = refused
                .iter()
                .find(|r| r.agent_id == id)
                .expect("refused entry");
            assert!(
                hit.error.contains("meta.json"),
                "error carries the cause: {}",
                hit.error
            );
        })
    }

    #[test]
    #[serial]
    fn find_disk_root_returns_the_unsupervised_agent_only() {
        with_data_dir(|_| {
            assert_eq!(
                find_disk_root().expect("find"),
                None,
                "empty data dir has no root"
            );
            let root = AgentId::from("disk-root-1".to_owned());
            let sub = AgentId::from("disk-sub-1".to_owned());
            create_agent_dir(
                &root,
                AgentMeta {
                    workspace_root: Path::new("/r").to_path_buf(),
                    created_by: None,
                    role: "root".to_string(),
                    description: String::new(),
                    profile_set: None,
                    permissions_class: crate::config::PermissionClass::Normal,
                    delegation_mode: crate::config::DelegationMode::CarveOut,
                },
            )
            .unwrap();
            create_agent_dir(
                &sub,
                AgentMeta {
                    workspace_root: Path::new("/s").to_path_buf(),
                    created_by: Some(root.clone()),
                    role: "sub".to_string(),
                    description: String::new(),
                    profile_set: None,
                    permissions_class: crate::config::PermissionClass::Normal,
                    delegation_mode: crate::config::DelegationMode::CarveOut,
                },
            )
            .unwrap();
            assert_eq!(
                find_disk_root().expect("find"),
                Some(root),
                "the created_by-less agent is the root"
            );
        })
    }

    #[test]
    #[serial]
    fn scan_and_find_error_when_the_agents_dir_is_unreadable() {
        with_data_dir(|_| {
            // agents/ exists but as a regular file: read_dir fails with
            // ENOTDIR, standing in for permission-denied states the runner
            // cannot reproduce (root ignores file modes).
            std::fs::create_dir_all(data_dir_root().unwrap()).unwrap();
            std::fs::write(agents_base().unwrap(), "not a directory").unwrap();
            let err = match scan_agents() {
                Ok(_) => panic!("scan must not swallow an unreadable dir"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("cannot read agents directory"),
                "error names the cause: {err:#}"
            );
            let err = match find_disk_root() {
                Ok(_) => panic!("find must refuse to guess"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("cannot read agents directory"),
                "error names the cause: {err:#}"
            );
        })
    }

    #[test]
    #[serial]
    fn rollback_remove_leaves_no_archive_residue() {
        with_data_dir(|_| {
            let id = AgentId::from("rollback-1".to_owned());
            let dir = create_agent_dir(
                &id,
                AgentMeta {
                    workspace_root: Path::new("/app").to_path_buf(),
                    created_by: None,
                    role: String::new(),
                    description: String::new(),
                    profile_set: None,
                    permissions_class: crate::config::PermissionClass::Normal,
                    delegation_mode: crate::config::DelegationMode::CarveOut,
                },
            )
            .unwrap();
            // Rollback of a never-alive agent removes the live dir directly,
            // never archiving (the abort/create-rollback call sites).
            std::fs::remove_dir_all(&dir).unwrap();
            assert!(!agent_dir(&id).unwrap().exists());
            assert!(!archived_dir(&id).unwrap().exists());
        })
    }

    #[test]
    #[serial]
    fn archive_missing_source_is_noop() {
        with_data_dir(|_| {
            let id = AgentId::from("missing-src-1".to_owned());
            // No create_agent_dir — source is absent.
            archive_agent_dir(&id).unwrap();
            assert!(!agent_dir(&id).unwrap().exists());
            assert!(!archived_dir(&id).unwrap().exists());
        })
    }

    #[test]
    #[serial]
    fn archive_bails_when_destination_exists() {
        with_data_dir(|_| {
            let id = AgentId::from("collision-1".to_owned());
            let dir = create_agent_dir(
                &id,
                AgentMeta {
                    workspace_root: Path::new("/app").to_path_buf(),
                    created_by: None,
                    role: String::new(),
                    description: String::new(),
                    profile_set: None,
                    permissions_class: crate::config::PermissionClass::Normal,
                    delegation_mode: crate::config::DelegationMode::CarveOut,
                },
            )
            .unwrap();
            // Pre-create the archived destination (an anomaly: UUIDs should not collide).
            std::fs::create_dir_all(archived_dir(&id).unwrap()).unwrap();

            let res = archive_agent_dir(&id);
            assert!(res.is_err(), "must bail when destination already exists");
            assert!(dir.exists(), "source must be left intact on bail");
        })
    }

    // `copy_dir_all` is the EXDEV fallback path inside `move_agent_dir` (shared by
    // the archive, deactivate, and reactivate movers); the
    // same-fs archive tests above exercise the `rename` path. Forcing a real
    // cross-device `EXDEV` needs two tmpfs mounts (impractical here), so this
    // tests the copier directly: a nested tree with a file and a symlink must
    // round-trip verbatim, the symlink recreated (not dereferenced).
    #[test]
    fn copy_dir_all_round_trips_tree_with_symlink() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/file.txt"), "body").unwrap();
        std::fs::write(src.join("top.txt"), "top").unwrap();
        std::os::unix::fs::symlink("top.txt", src.join("link")).unwrap();

        let dst = tmp.path().join("dst");
        copy_dir_all(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("top.txt")).unwrap(), "top");
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/file.txt")).unwrap(),
            "body"
        );
        // The symlink is preserved as a symlink (read_link gives the target),
        // not dereferenced into a copy of `top.txt`.
        assert_eq!(
            std::fs::read_link(dst.join("link")).unwrap(),
            std::path::Path::new("top.txt")
        );
        assert!(
            std::fs::symlink_metadata(dst.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    // ----- inactive (declarative-team deactivation) tests -----

    fn meta_for(role: &str) -> AgentMeta {
        AgentMeta {
            workspace_root: PathBuf::from("/app"),
            created_by: None,
            role: role.into(),
            description: String::new(),
            profile_set: None,
            permissions_class: crate::config::PermissionClass::Normal,
            delegation_mode: crate::config::DelegationMode::CarveOut,
        }
    }

    /// The structural restore boundary: deactivating moves the directory out
    /// of `agents/`, so the next scan neither sees nor restores it. The
    /// declarative-team converge flow relies on this — "not referenced by
    /// the declaration" must mean "not resurrected at boot".
    #[test]
    #[serial]
    fn scan_agents_ignores_inactive() {
        with_data_dir(|_| {
            let id = AgentId::from("aaaa1111-1111-4111-8111-111111111111".to_owned());
            create_agent_dir(&id, meta_for("scout")).unwrap();
            assert_eq!(
                scan_agents().unwrap().0.len(),
                1,
                "live before deactivation"
            );

            deactivate_agent_dir(&id).unwrap();
            assert!(!agent_dir(&id).unwrap().exists(), "left the live area");
            assert!(inactive_dir(&id).unwrap().exists(), "parked in inactive");

            let (pending, refused) = scan_agents().unwrap();
            assert!(
                pending.iter().all(|p| p.agent_id != id),
                "inactive agent must not be scanned for restore"
            );
            assert!(refused.is_empty());

            // Round trip: reactivation puts it back where the scan finds it.
            reactivate_agent_dir(&id).unwrap();
            assert_eq!(scan_agents().unwrap().0.len(), 1, "live after reactivation");
        })
    }

    /// The pre-check is bidirectional and refuses to overwrite: a stale body
    /// in the destination area means two preserved bodies for one identity,
    /// and picking between them silently is exactly what must never happen.
    #[test]
    #[serial]
    fn deactivate_and_reactivate_refuse_existing_destination() {
        with_data_dir(|_| {
            let id = AgentId::from("aaaa2222-2222-4222-8222-222222222222".to_owned());
            create_agent_dir(&id, meta_for("scout")).unwrap();
            // Stale inactive body: deactivate must refuse, leaving the live
            // directory untouched.
            std::fs::create_dir_all(inactive_dir(&id).unwrap()).unwrap();
            let err = deactivate_agent_dir(&id).unwrap_err();
            assert!(err.to_string().contains("refusing to overwrite"));
            assert!(agent_dir(&id).unwrap().exists(), "live body intact");

            // Clean the stale body, deactivate for real, then plant a live
            // body: reactivation must refuse the same way.
            std::fs::remove_dir_all(inactive_dir(&id).unwrap()).unwrap();
            deactivate_agent_dir(&id).unwrap();
            create_agent_dir(&id, meta_for("scout")).unwrap();
            let err = reactivate_agent_dir(&id).unwrap_err();
            assert!(err.to_string().contains("refusing to overwrite"));
            assert!(inactive_dir(&id).unwrap().exists(), "inactive body intact");
        })
    }

    /// The archive path is deliberately one-way: deactivate operates on the
    /// live area only, and an archived id never re-enters the flow through
    /// it (archived is terminal: nothing in the lifecycle moves an archived
    /// directory back — reactivation reads the inactive area only).
    #[test]
    #[serial]
    fn deactivate_requires_a_live_directory() {
        with_data_dir(|_| {
            let id = AgentId::from("aaaa3333-3333-4333-8333-333333333333".to_owned());
            let err = deactivate_agent_dir(&id).unwrap_err();
            assert!(err.to_string().contains("no live directory"));
        })
    }
}
