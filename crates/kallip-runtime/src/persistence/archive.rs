use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result};

use super::layout::agent_dir;
use super::layout::{archived_dir, inactive_dir};
use kallip_common::AgentId;

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

/// Move a lived agent's directory from `agents/active/` to `agents/archived/` on remove.
///
/// The agent's data (history, `context.json` with cumulative usage, approvals,
/// exec_policy, meta) is preserved verbatim — removal becomes archival, not
/// destruction. `agents/archived/` is a sibling of `agents/active/`, so it is invisible to
/// [`crate::persistence::scan_agents`] and the live registry.
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
    // Ensure the archived base exists (parent of `dst`).
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

#[cfg(test)]
mod tests;
