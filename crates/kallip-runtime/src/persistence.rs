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
use time::{Duration as TimeDuration, OffsetDateTime};

use crate::approval::ApprovalStore;
use crate::context::ContextStore;
use just_llm_client::types::chat::ChatMessage;
use kallip_common::AgentId;

/// Resolve the shared data root under which `agents/`, `archived/`, and
/// `skills/` live.
///
/// - If `$KALLIP_DATA_DIR` is set, it IS the data root — used verbatim,
///   no suffix appended. The operator has already named the directory.
/// - Otherwise fall back to the platform data directory namespaced as
///   `<platform_data_dir>/kallip` (XDG convention).
///
/// Both `agents_base` and `archived_base` route through this so the live and
/// archived trees share one root. When that root is on a single filesystem,
/// `archive_agent_dir`'s `rename` is atomic; if the root is symlinked across a
/// filesystem boundary the `rename` raises `EXDEV` and the archive falls back to
/// a recursive copy + delete (see `archive_agent_dir`).
pub fn data_dir_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("KALLIP_DATA_DIR") {
        Ok(PathBuf::from(dir))
    } else {
        Ok(dirs::data_dir()
            .context("could not determine platform data directory")?
            .join("kallip"))
    }
}

/// Canonicalize the data root for path-overlap comparison.
///
/// The data root may not exist yet on a fresh install (no agent ever created), in
/// which case [`std::fs::canonicalize`] would fail. Fall back to canonicalizing the
/// parent (which must exist — the platform data dir for the XDG fallback, or the
/// parent of `$KALLIP_DATA_DIR` when it is set) and re-appending the leaf,
/// yielding the canonical path the data root *would* have. This keeps the overlap
/// check sound without forcing the data dir to exist.
fn canonical_data_root() -> Result<PathBuf> {
    let root = data_dir_root()?;
    match root.canonicalize() {
        Ok(c) => Ok(c),
        Err(_) => {
            let parent = root
                .parent()
                .with_context(|| format!("data root {root:?} has no parent"))?
                .canonicalize()
                .with_context(|| format!("canonicalize parent of data root {root:?}"))?;
            let leaf = root
                .file_name()
                .with_context(|| format!("data root {root:?} has no file name"))?;
            Ok(parent.join(leaf))
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

/// Archived directory for a given agent (sibling of [`agent_dir`]).
fn archived_dir(agent_id: &AgentId) -> Result<PathBuf> {
    Ok(archived_base()?.join(agent_id.as_ref()))
}

/// Agent directory for a given agent.
pub fn agent_dir(agent_id: &AgentId) -> Result<PathBuf> {
    Ok(agents_base()?.join(agent_id.as_ref()))
}

/// Create agent directory and write initial meta.json.
pub fn create_agent_dir(
    agent_id: &AgentId,
    workspace_root: &Path,
    created_by: Option<&AgentId>,
    role: &str,
    description: &str,
    permissions_class: crate::config::PermissionClass,
    delegation_mode: crate::config::DelegationMode,
) -> Result<PathBuf> {
    let dir = agent_dir(agent_id)?;
    std::fs::create_dir_all(&dir)?;

    let meta = AgentMeta {
        workspace_root: workspace_root.to_path_buf(),
        last_restored_at: None,
        consecutive_restart_count: 0,
        created_by: created_by.cloned(),
        role: role.to_owned(),
        description: description.to_owned(),
        permissions_class,
        delegation_mode,
    };
    atomic_write(
        &dir.join("meta.json"),
        &serde_json::to_string_pretty(&meta)?,
    )?;

    Ok(dir)
}

/// Read-modify-write `meta.json` to update `role` and/or `description`.
///
/// Used by `PUT /agents/{id}/metadata`. Reads the current meta, applies the
/// closures' values, and atomically rewrites — preserving
/// `last_restored_at` / `consecutive_restart_count` / `created_by` (the same
/// read-modify-write pattern `check_meta` uses for its restart counters).
/// `None` leaves a field unchanged; `Some(s)` sets it.
///
/// Call this outside the registry lock (file I/O); the caller then updates the
/// in-memory `AgentConfig` under the lock — persist-first-then-memory, mirroring
/// `routes::context::update_exec_policy`. `check_meta` is startup-only, so the two
/// meta writers never run concurrently.
pub fn rewrite_meta(dir: &Path, role: Option<&str>, description: Option<&str>) -> Result<()> {
    let path = dir.join("meta.json");
    let json = fs::read_to_string(&path).context("reading meta.json")?;
    let mut meta: AgentMeta = serde_json::from_str(&json).context("parsing meta.json")?;
    if let Some(r) = role {
        meta.role = r.to_owned();
    }
    if let Some(d) = description {
        meta.description = d.to_owned();
    }
    atomic_write(&path, &serde_json::to_string_pretty(&meta)?)?;
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
/// The move is atomic via `rename` when the data dir is on one filesystem; if
/// the root is symlinked across a filesystem boundary the `rename` fails with
/// `EXDEV` and we fall back to a recursive copy + delete (`copy_dir_all`).
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
    // Atomic when `agents/` and `archived/` share one filesystem. Across a
    // filesystem boundary (a symlinked `$KALLIP_DATA_DIR`) `rename` returns
    // `EXDEV`; fall back to copy + delete so archival still completes.
    if let Err(e) = std::fs::rename(&src, &dst) {
        if e.kind() != std::io::ErrorKind::CrossesDevices {
            return Err(e).context("archiving agent directory");
        }
        // The top-level `dst.exists()` bail above covers the rename path; the
        // copy fallback must hold the same invariant.
        if dst.exists() {
            anyhow::bail!(
                "archived destination appeared during cross-device archive of agent \
                 {agent_id} — refusing to overwrite"
            );
        }
        copy_dir_all(&src, &dst).context("cross-device archive copy")?;
        // Only delete the source once the copy fully succeeds — a failed copy
        // leaves `src` intact (and a partial `dst`) for manual recovery rather
        // than losing the agent's data.
        std::fs::remove_dir_all(&src).context("removing source after cross-device archive")?;
    }
    Ok(())
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
/// verified by walkthrough (design doc §7, context-persistence).
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
    /// Time of the last successful restore.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_restored_at: Option<OffsetDateTime>,
    /// Consecutive rapid restart counter (reset when outside the window).
    #[serde(default)]
    pub consecutive_restart_count: u32,
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
    /// invariant — persisted so restore can re-validate the ceiling chain (unlike
    /// `role`/`description`, which are display-only). Defaults to Normal for legacy
    /// `meta.json` files written before this field existed.
    #[serde(default)]
    pub permissions_class: crate::config::PermissionClass,
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

/// Maximum consecutive rapid restarts before refusing restore.
const MAX_CONSECUTIVE_RESTARTS: u32 = 3;
/// Window in which restarts are considered consecutive.
const CONSECUTIVE_RESTART_WINDOW: TimeDuration = TimeDuration::seconds(60);

/// Lightweight handle produced by scanning the agents directory.
pub struct PendingRestore {
    pub agent_id: AgentId,
    pub agent_dir: PathBuf,
    pub meta: AgentMeta,
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
}

/// Scan the agents directory and return agents eligible for restore.
///
/// Reads only `meta.json` per agent (lightweight). Skips agents that
/// fail crash-loop detection, logging warnings.
pub fn scan_agents() -> Vec<PendingRestore> {
    let base = match agents_base() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("cannot resolve agents base: {e:#}");
            return Vec::new();
        }
    };

    let entries = match fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut pending = Vec::new();
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
        match check_meta(&path) {
            Ok(meta) => pending.push(PendingRestore {
                agent_id,
                agent_dir: path,
                meta,
            }),
            Err(e) => {
                tracing::warn!(id = %agent_id, "skipping agent: {e:#}");
            }
        }
    }
    pending
}

/// Read, validate, and update meta.json for crash-loop detection.
fn check_meta(dir: &Path) -> Result<AgentMeta> {
    let meta_json = fs::read_to_string(dir.join("meta.json")).context("reading meta.json")?;
    let mut meta: AgentMeta = serde_json::from_str(&meta_json).context("parsing meta.json")?;

    let now = OffsetDateTime::now_utc();
    let is_consecutive = meta.last_restored_at.is_some_and(|prev| {
        let elapsed = now - prev;
        elapsed > TimeDuration::ZERO && elapsed < CONSECUTIVE_RESTART_WINDOW
    });

    if is_consecutive {
        meta.consecutive_restart_count += 1;
    } else {
        meta.consecutive_restart_count = 1;
    }

    if meta.consecutive_restart_count > MAX_CONSECUTIVE_RESTARTS {
        anyhow::bail!(
            "agent exceeded {MAX_CONSECUTIVE_RESTARTS} consecutive restarts, \
             refusing restore to break crash loop"
        );
    }

    meta.last_restored_at = Some(now);
    atomic_write(
        &dir.join("meta.json"),
        &serde_json::to_string_pretty(&meta)?,
    )?;

    Ok(meta)
}

/// Deserialize a single agent from its directory.
/// Loads the context via split persistence when `manifest.json` is present
/// (hydrating the conversation window from history by turn ID), otherwise
/// through the legacy `context.json` path — which then migrates in place to
/// the split format. Either way the on-load migrations, token re-estimation,
/// and restart-message injection still run. Non-fatal damage is collected in
/// `degraded` rather than failing the restore.
pub fn restore_agent(agent_id: &AgentId, dir: &Path) -> Result<RestorableAgent> {
    let mut degraded = Vec::new();
    let mut store = load_store(dir, &mut degraded)?;

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
/// buys time to notice), and the explicit repair operation — not restore —
/// is what rewrites history to fix it. Pinned turns carry no tool traffic,
/// so they pass through clean.
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

/// Load the context store in the format the directory holds.
///
/// `manifest.json` present → split format: parse both documents and hydrate
/// the conversation window from history by turn ID. A leftover
/// `context.json` (migration interrupted before the rename) is finished off.
/// Only `context.json` → legacy format: load it, strip restart notices, and
/// migrate to the split format in place. Neither → fresh store.
fn load_store(dir: &Path, degraded: &mut Vec<Degradation>) -> Result<ContextStore> {
    let manifest_path = dir.join("manifest.json");
    let legacy_path = dir.join("context.json");

    if manifest_path.exists() {
        if legacy_path.exists()
            && let Err(e) = fs::rename(&legacy_path, &legacy_archive_path(dir))
        {
            tracing::warn!("finishing legacy rename failed: {e:#}");
        }

        let manifest: crate::context::manifest::ManifestDoc = serde_json::from_str(
            &fs::read_to_string(&manifest_path).context("reading manifest.json")?,
        )
        .context("parsing manifest.json")?;
        let pins: crate::context::manifest::PinsDoc = serde_json::from_str(
            &fs::read_to_string(dir.join("pins.json")).context("reading pins.json")?,
        )
        .context("parsing pins.json")?;

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
        return Ok(ContextStore::from_persisted(&pins, convo, &manifest));
    }

    if legacy_path.exists() {
        let mut store: ContextStore = serde_json::from_str(
            &fs::read_to_string(&legacy_path).context("reading context.json")?,
        )
        .context("parsing context.json")?;
        strip_restart_turns(&mut store);
        migrate_legacy_to_split(dir, &store)?;
        return Ok(store);
    }

    Ok(ContextStore::new())
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
    "any necessary conditions before continuing.\n",
    "Directory write-locks do not survive a tagma restart: all locks were\n",
    "released. Re-acquire any locks you still need (`kallip dirlock acquire\n",
    "<dir>`) before writing shared directories."
);

#[cfg(test)]
mod tests {
    use super::*;

    use crate::context::AgenticContext as _;
    use crate::history::{HistoryWriter, RecordKind};
    use serial_test::serial;
    use tempfile::TempDir;

    // --- split persistence (manifest/pins) ---

    #[test]
    fn persist_writes_manifest_and_pins_with_backup() {
        let dir = TempDir::new().unwrap();
        let mut store = ContextStore::new();
        store.pin("note", ChatMessage::assistant("keep")).unwrap();
        store.push_turn(vec![ChatMessage::user("hello")]);
        let first_ids = store.to_manifest_doc().conversation_turn_ids.clone();

        persist_context(&store, dir.path()).unwrap();
        let manifest_v1 = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
        assert!(dir.path().join("pins.json").exists());
        assert!(
            !dir.path().join("manifest.json.bak").exists(),
            "no backup on first write"
        );

        // Second turn changes the manifest; the backup keeps version 1.
        store.push_turn(vec![ChatMessage::user("more")]);
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
        let kept = store.push_turn(vec![ChatMessage::user("kept")]).0;
        let injected = store
            .push_turn(vec![ChatMessage::user("[system] restart")])
            .0;
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
    fn legacy_fixture() -> (ContextStore, Vec<ChatMessage>) {
        let mut store = ContextStore::new();
        store
            .pin("note", ChatMessage::assistant("pinned note"))
            .unwrap();
        store.push_turn(vec![ChatMessage::user("first question")]);
        store.push_turn(vec![ChatMessage::assistant("first answer")]);
        store.retry_log.push(kallip_common::retry::RetryRecord {
            timestamp: 9,
            round: 0,
            attempt: 1,
            max_attempts: 3,
            error: "legacy".into(),
            delay_secs: 1.0,
            endpoint: None,
        });
        // Expected conversation window, in order, for history.
        let convo = vec![
            ChatMessage::user("first question"),
            ChatMessage::assistant("first answer"),
        ];
        (store, convo)
    }

    fn write_legacy(
        dir: &TempDir,
        store: &ContextStore,
        with_history: bool,
        convo: &[ChatMessage],
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
        store.push_turn(vec![ChatMessage::user(RESTART_MESSAGE)]);
        write_legacy(&dir, &store, true, &convo);
        let legacy_next = store.to_manifest_doc().next_turn_id;

        let restored = restore_agent(&AgentId::from("a".to_owned()), dir.path()).unwrap();

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

        let restored = restore_agent(&AgentId::from("a".to_owned()), dir.path()).unwrap();

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

        let restored = restore_agent(&AgentId::from("a".to_owned()), dir.path()).unwrap();

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

        let restored = restore_agent(&AgentId::from("a".to_owned()), dir.path()).unwrap();

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
        let again = restore_agent(&AgentId::from("a".to_owned()), dir.path()).unwrap();
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

    #[test]
    fn missing_history_records_degrade_instead_of_failing() {
        let dir = TempDir::new().unwrap();
        let (store, convo) = legacy_fixture();
        // History records only the first turn; the second goes missing.
        write_legacy(&dir, &store, true, &convo[..1]);

        let restored = restore_agent(&AgentId::from("a".to_owned()), dir.path()).unwrap();

        // The first restore migrates: the full legacy window is in memory,
        // so nothing is degraded yet. The gap surfaces on the next restore,
        // which hydrates from the split files.
        assert!(restored.degraded.is_empty());

        let degraded = restore_agent(&AgentId::from("a".to_owned()), dir.path()).unwrap();

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
    #[test]
    fn pairing_damage_degrades_instead_of_repairing() {
        use just_llm_client::types::chat::{ChatToolCall, FunctionCall, ToolType};
        let call = |id: &str, name: &str| ChatToolCall {
            id: id.to_owned(),
            kind: ToolType::Function,
            function: FunctionCall {
                name: name.to_owned(),
                arguments: "{}".to_owned(),
            },
        };
        let dir = TempDir::new().unwrap();
        let mut store = ContextStore::new();
        let damaged = vec![
            ChatMessage::assistant_tool_calls(vec![call("c1", "read"), call("c2", "edit")]),
            ChatMessage::tool_result("ok", "c1"),
            ChatMessage::tool_result("ghost", "c9"),
        ];
        let (turn_id, _) = store.push_turn(damaged.clone());
        let history = HistoryWriter::new(dir.path().to_path_buf());
        history
            .append(Some(turn_id.0), &damaged, 8, RecordKind::Turn, None)
            .unwrap();
        persist_context(&store, dir.path()).unwrap();

        let restored = restore_agent(&AgentId::from("a".to_owned()), dir.path()).unwrap();

        let kinds: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
        assert!(kinds.contains(&DegradationKind::UnansweredToolCalls));
        assert!(kinds.contains(&DegradationKind::OrphanToolResults));
        // Report-only: the hydrated turn is byte-for-byte what history holds.
        let turns = restored.store.turns();
        assert_eq!(turns.front().unwrap().messages, damaged);
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

        let first = restore_agent(&id, tmp.path()).unwrap();
        println!(
            "first restore (migration): {} degradation(s)",
            first.degraded.len()
        );
        for d in &first.degraded {
            println!("  {:?}: {}", d.kind, d.detail);
        }

        let second = restore_agent(&id, tmp.path()).unwrap();
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
            last_restored_at: None,
            consecutive_restart_count: 0,
            created_by: None,
            role: "researcher".into(),
            description: "gathers sources".into(),
            permissions_class: crate::config::PermissionClass::Guest,
            delegation_mode: crate::config::DelegationMode::CarveOut,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: AgentMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.workspace_root, PathBuf::from("/app"));
        assert_eq!(back.role, "researcher");
        assert_eq!(back.description, "gathers sources");
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
            last_restored_at: None,
            consecutive_restart_count: 0,
            created_by: None,
            role: String::new(),
            description: String::new(),
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
            "last_restored_at": null,
            "consecutive_restart_count": 0,
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
    // These mutate the process-global KALLIP_DATA_DIR, so they are serialized
    // (serial_test) and each scopes a tempfile::TempDir via temp_env.
    fn with_data_dir<R>(f: impl FnOnce(&TempDir) -> R) -> R {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        temp_env::with_var("KALLIP_DATA_DIR", Some(path.as_str()), || f(&tmp))
    }

    // ----- workspace/data-dir overlap guard tests -----
    // The guard backs the data-dir integrity baseline: with no workspace↔data
    // overlap, landlock alone keeps the agent out of tagma bookkeeping.

    #[test]
    #[serial]
    fn overlap_detects_workspace_inside_data_root() {
        with_data_dir(|tmp| {
            // data root == tmp (env var used verbatim); workspace nested under it
            // → overlap. Exercises the `ws.starts_with(&data)` direction.
            let ws = tmp.path().join("agents/x");
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
        with_data_dir(|tmp| {
            // workspace == data root (== tmp) → overlap (degenerate case); equal
            // paths satisfy both `starts_with` directions.
            let ws = tmp.path().to_path_buf();
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
            // tmp's parent is the smallest such ancestor that exists on disk.
            let ws = tmp.path().parent().unwrap().to_path_buf();
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
        with_data_dir(|tmp| {
            // A sibling whose leaf name is a string prefix of the data root's leaf
            // (the data root is `tmp`, whose leaf is a random tempfile name) must
            // NOT be flagged: `Path::starts_with` is component-wise, not byte-wise.
            // Guards against a future regression to a byte-prefix comparison. The
            // candidate must exist on disk so `canonicalize` succeeds (the guard
            // fails closed — returning Err — on a non-existent workspace).
            let parent = tmp.path().parent().unwrap();
            let leaf = tmp.path().file_name().unwrap().to_str().unwrap();
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
        with_data_dir(|tmp| {
            let id = AgentId::from("archive-rt-1".to_owned());
            let dir = create_agent_dir(
                &id,
                Path::new("/app"),
                None,
                "",
                "",
                crate::config::PermissionClass::Normal,
                crate::config::DelegationMode::CarveOut,
            )
            .unwrap();

            // One history record (as the live writer would produce).
            HistoryWriter::new(dir.clone())
                .append(
                    Some(0),
                    &[ChatMessage::user("hello")],
                    16,
                    RecordKind::Turn,
                    None,
                )
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
            // Live and archived trees share one root (the data dir, used
            // verbatim from $KALLIP_DATA_DIR == tmp; so `rename` is atomic
            // on one filesystem, a cross-fs EXDEV falls back to copy + delete).
            assert!(tmp.path().join("agents").exists());
            assert!(tmp.path().join("archived").exists());
        })
    }

    #[test]
    #[serial]
    fn scan_agents_ignores_archived() {
        with_data_dir(|_| {
            let id = AgentId::from("scan-ignores-1".to_owned());
            create_agent_dir(
                &id,
                Path::new("/app"),
                None,
                "",
                "",
                crate::config::PermissionClass::Normal,
                crate::config::DelegationMode::CarveOut,
            )
            .unwrap();
            archive_agent_dir(&id).unwrap();

            let pending = scan_agents();
            assert!(
                pending.iter().all(|p| p.agent_id != id),
                "archived agent must not be eligible for restore"
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
                Path::new("/app"),
                None,
                "",
                "",
                crate::config::PermissionClass::Normal,
                crate::config::DelegationMode::CarveOut,
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
                Path::new("/app"),
                None,
                "",
                "",
                crate::config::PermissionClass::Normal,
                crate::config::DelegationMode::CarveOut,
            )
            .unwrap();
            // Pre-create the archived destination (an anomaly: UUIDs should not collide).
            std::fs::create_dir_all(archived_dir(&id).unwrap()).unwrap();

            let res = archive_agent_dir(&id);
            assert!(res.is_err(), "must bail when destination already exists");
            assert!(dir.exists(), "source must be left intact on bail");
        })
    }

    // `copy_dir_all` is the EXDEV fallback path inside `archive_agent_dir`; the
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
}
