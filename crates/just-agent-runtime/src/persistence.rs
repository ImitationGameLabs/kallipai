//! Agent persistence: atomic JSON serialization to disk.
//!
//! Writes context and approval state to per-agent directories.
//! All writes use atomic rename (temp file + rename) to prevent corruption
//! on crash. On daemon restart, [`scan_agents`] scans for agents
//! that can be recovered.

use std::fs;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use just_agent_common::policy::ToolPolicy;
use serde::{Deserialize, Serialize};
use time::{Duration as TimeDuration, OffsetDateTime};

use crate::approval::ApprovalStore;
use crate::context::ContextStore;
use just_agent_common::AgentId;
use just_llm_client::types::chat::ChatMessage;

/// Resolve the shared `<data_dir>/just-agent` root.
///
/// `$JUST_AGENT_DATA_DIR` if set, else the platform data directory. Both
/// [`agents_base`] and [`archived_base`] route through this so the live and
/// archived trees always share one root on one filesystem — the invariant that
/// makes [`archive_agent_dir`]'s `rename` atomic (a divergent resolution would
/// raise `EXDEV` at runtime).
fn data_dir_root() -> Result<PathBuf> {
    let base = if let Ok(dir) = std::env::var("JUST_AGENT_DATA_DIR") {
        PathBuf::from(dir)
    } else {
        dirs::data_dir().context("could not determine platform data directory")?
    };
    Ok(base.join("just-agent"))
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
) -> Result<PathBuf> {
    let dir = agent_dir(agent_id)?;
    std::fs::create_dir_all(&dir)?;

    let meta = AgentMeta {
        workspace_root: workspace_root.to_path_buf(),
        last_restored_at: None,
        consecutive_restart_count: 0,
        created_by: created_by.cloned(),
    };
    atomic_write(
        &dir.join("meta.json"),
        &serde_json::to_string_pretty(&meta)?,
    )?;

    Ok(dir)
}

/// Move a lived agent's directory from `agents/` to `archived/` on remove.
///
/// The agent's data (history, `context.json` with cumulative usage, approvals,
/// policy, meta) is preserved verbatim — removal becomes archival, not
/// destruction. `archived/` is a sibling of `agents/`, so it is invisible to
/// [`scan_agents`] and the live registry.
///
/// Idempotent: a missing source is a no-op. Bails if the destination already
/// exists — agent ids are `Uuid::new_v4()`, so a pre-existing destination is an
/// anomaly (backup restore / tampering / bug), not a collision; surfacing it
/// loudly beats silently overwriting a prior archive. The same-filesystem
/// guarantee from `data_dir_root` makes the `rename` atomic.
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
    std::fs::rename(&src, &dst)?;
    Ok(())
}

/// Atomically write content to a file via temp file + rename.
pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp_path = parent.join(format!(".{file_name}.tmp"));
    std::fs::write(&temp_path, content)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

/// Serialize and write context store to context.json.
pub fn persist_context(json: &str, dir: &Path) -> Result<()> {
    atomic_write(&dir.join("context.json"), json)
}

/// Serialize and write approval store to approvals.json.
pub fn persist_approvals(json: &str, dir: &Path) -> Result<()> {
    atomic_write(&dir.join("approvals.json"), json)
}

/// Serialize and write tool policy to policy.toml.
pub fn persist_policy(dir: &Path, policy: &ToolPolicy) -> Result<()> {
    let toml_str = toml::to_string_pretty(policy).context("serializing policy.toml")?;
    atomic_write(&dir.join("policy.toml"), &toml_str)
}

/// Load tool policy from policy.toml.
/// Errors on missing file, read failure, or parse failure.
pub fn load_policy(dir: &Path) -> Result<ToolPolicy> {
    let path = dir.join("policy.toml");
    let content = fs::read_to_string(&path).context("reading policy.toml")?;
    toml::from_str(&content).context("parsing policy.toml")
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
///
/// Reads context.json and approvals.json, fixes incomplete turns, and
/// injects the restart message.
pub fn restore_agent(agent_id: &AgentId, dir: &Path) -> Result<RestorableAgent> {
    let mut store: ContextStore = match fs::read_to_string(dir.join("context.json")) {
        Ok(json) => serde_json::from_str(&json).context("parsing context.json")?,
        Err(_) => ContextStore::new(),
    };

    let approvals: ApprovalStore = match fs::read_to_string(dir.join("approvals.json")) {
        Ok(json) => serde_json::from_str(&json).context("parsing approvals.json")?,
        Err(_) => ApprovalStore::new(),
    };

    fix_incomplete_turn(&mut store);

    // Backfill the pinned-token cache for items deserialized from a pre-caching format (default
    // 0), before any budget computation or migration reads it.
    store.backfill_pinned_token_cache();

    // Migrate legacy summary field to pinned item.
    store.migrate_legacy_summary();

    // A restore may follow an agent-version upgrade that changed the system prompt or tool set,
    // invalidating the persisted `last_prompt_tokens` anchor. Force a full estimate on the first
    // post-restore round so the gate recomputes from the current config rather than trusting a
    // cross-version anchor. (migrate/pin/unpin above also set the flag; this is the canonical
    // restore statement and covers the clean-restore case.) See ContextStore::needs_full_estimate.
    store.mark_needs_full_estimate();

    let restart_msgs = vec![ChatMessage::user(RESTART_MESSAGE)];
    let (_, estimated_tokens) = store.push_turn(restart_msgs.clone());

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
    })
}

/// If the last turn ends with a ToolCalls message (no corresponding
/// ToolResult), the turn was interrupted by a crash. Remove it so
/// the provider does not receive an incomplete conversation.
fn fix_incomplete_turn(store: &mut ContextStore) {
    let count = store.turn_count();
    if count == 0 {
        return;
    }

    // A complete turn's last message should be either a plain assistant
    // response or a tool_result. If it ends with ToolCalls, the round
    // was interrupted before tool execution finished.
    let incomplete = store
        .turns()
        .back()
        .and_then(|t| t.messages.last())
        .is_some_and(|msg| msg.tool_calls().is_some());

    if incomplete {
        store.drain_turns(count - 1..count);
        tracing::info!("removed incomplete last turn from restored agent");
    }
}

const RESTART_MESSAGE: &str = concat!(
    "[system]\n",
    "Agent restored from a previous state. Shell sessions have been reset \u{2014}\n",
    "environment variables, working directory, and background processes are no\n",
    "longer available. Review the current state of the project and re-establish\n",
    "any necessary conditions before continuing."
);

#[cfg(test)]
mod tests {
    use super::*;

    use crate::history::{HistoryWriter, RecordKind};
    use serial_test::serial;
    use tempfile::TempDir;

    #[test]
    fn agent_meta_round_trips() {
        let meta = AgentMeta {
            workspace_root: PathBuf::from("/app"),
            last_restored_at: None,
            consecutive_restart_count: 0,
            created_by: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: AgentMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.workspace_root, PathBuf::from("/app"));
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
    }

    // ----- archive-on-remove tests -----
    // These mutate the process-global JUST_AGENT_DATA_DIR, so they are serialized
    // (serial_test) and each scopes a tempfile::TempDir via temp_env.
    fn with_data_dir<R>(f: impl FnOnce(&TempDir) -> R) -> R {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        temp_env::with_var("JUST_AGENT_DATA_DIR", Some(path.as_str()), || f(&tmp))
    }

    #[test]
    #[serial]
    fn archive_moves_dir_and_preserves_contents() {
        with_data_dir(|tmp| {
            let id = AgentId::from("archive-rt-1".to_owned());
            let dir = create_agent_dir(&id, Path::new("/app"), None).unwrap();

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
            // Live and archived trees share one root (the rename-atomicity invariant).
            assert!(tmp.path().join("just-agent/agents").exists());
            assert!(tmp.path().join("just-agent/archived").exists());
        })
    }

    #[test]
    #[serial]
    fn scan_agents_ignores_archived() {
        with_data_dir(|_| {
            let id = AgentId::from("scan-ignores-1".to_owned());
            create_agent_dir(&id, Path::new("/app"), None).unwrap();
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
            let dir = create_agent_dir(&id, Path::new("/app"), None).unwrap();
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
            let dir = create_agent_dir(&id, Path::new("/app"), None).unwrap();
            // Pre-create the archived destination (an anomaly: UUIDs should not collide).
            std::fs::create_dir_all(archived_dir(&id).unwrap()).unwrap();

            let res = archive_agent_dir(&id);
            assert!(res.is_err(), "must bail when destination already exists");
            assert!(dir.exists(), "source must be left intact on bail");
        })
    }
}
