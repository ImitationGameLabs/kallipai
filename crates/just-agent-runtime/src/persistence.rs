//! Session persistence: atomic JSON serialization to disk.
//!
//! Writes context and approval state to per-agent session directories.
//! All writes use atomic rename (temp file + rename) to prevent corruption
//! on crash. On daemon restart, [`scan_sessions`] scans for sessions
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
/// Resolve the base sessions directory.
fn sessions_base() -> Result<PathBuf> {
    let base = if let Ok(dir) = std::env::var("JUST_AGENT_DATA_DIR") {
        PathBuf::from(dir)
    } else {
        dirs::data_dir().context("could not determine platform data directory")?
    };
    Ok(base.join("just-agent").join("sessions"))
}

/// Session directory for a given agent.
pub fn session_dir(agent_id: &AgentId) -> Result<PathBuf> {
    Ok(sessions_base()?.join(agent_id.as_ref()))
}

/// Create session directory and write initial meta.json.
pub fn create_session(
    agent_id: &AgentId,
    workspace_root: &Path,
    created_by: Option<&AgentId>,
) -> Result<PathBuf> {
    let dir = session_dir(agent_id)?;
    std::fs::create_dir_all(&dir)?;

    let mut meta = serde_json::json!({
        "workspace_root": workspace_root.to_string_lossy(),
    });
    if let Some(supervisor_id) = created_by {
        meta["created_by"] = serde_json::to_value(supervisor_id)?;
    }
    atomic_write(
        &dir.join("meta.json"),
        &serde_json::to_string_pretty(&meta)?,
    )?;

    Ok(dir)
}

/// Remove a session directory.
pub fn cleanup_session(agent_id: &AgentId) -> Result<()> {
    let dir = session_dir(agent_id)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// Atomically write content to a file via temp file + rename.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
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

/// Minimal metadata persisted per session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
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

/// Read a session's meta.json without side effects.
/// Used for validating supervisor chains at restore time.
pub fn read_meta(agent_id: &AgentId) -> Result<SessionMeta> {
    let path = session_dir(agent_id)?.join("meta.json");
    let json = fs::read_to_string(&path).context("reading meta.json")?;
    serde_json::from_str(&json).context("parsing meta.json")
}

/// Maximum consecutive rapid restarts before refusing restore.
const MAX_CONSECUTIVE_RESTARTS: u32 = 3;
/// Window in which restarts are considered consecutive.
const CONSECUTIVE_RESTART_WINDOW: TimeDuration = TimeDuration::seconds(60);

/// Lightweight handle produced by scanning the sessions directory.
pub struct PendingRestore {
    pub agent_id: AgentId,
    pub session_dir: PathBuf,
    pub meta: SessionMeta,
}

/// A session fully deserialized and ready to resume.
pub struct RestorableSession {
    pub agent_id: AgentId,
    pub session_dir: PathBuf,
    pub store: ContextStore,
    pub approvals: ApprovalStore,
}

/// Scan the sessions directory and return sessions eligible for restore.
///
/// Reads only `meta.json` per session (lightweight). Skips sessions that
/// fail crash-loop detection, logging warnings.
pub fn scan_sessions() -> Vec<PendingRestore> {
    let base = match sessions_base() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("cannot resolve sessions base: {e:#}");
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
                session_dir: path,
                meta,
            }),
            Err(e) => {
                tracing::warn!(id = %agent_id, "skipping session: {e:#}");
            }
        }
    }
    pending
}

/// Read, validate, and update meta.json for crash-loop detection.
fn check_meta(dir: &Path) -> Result<SessionMeta> {
    let meta_json = fs::read_to_string(dir.join("meta.json")).context("reading meta.json")?;
    let mut meta: SessionMeta = serde_json::from_str(&meta_json).context("parsing meta.json")?;

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
            "session exceeded {MAX_CONSECUTIVE_RESTARTS} consecutive restarts, \
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

/// Deserialize a single session from its directory.
///
/// Reads context.json and approvals.json, fixes incomplete turns, and
/// injects the restart message.
pub fn restore_session(agent_id: &AgentId, dir: &Path) -> Result<RestorableSession> {
    let mut store: ContextStore = match fs::read_to_string(dir.join("context.json")) {
        Ok(json) => serde_json::from_str(&json).context("parsing context.json")?,
        Err(_) => ContextStore::new(),
    };

    let approvals: ApprovalStore = match fs::read_to_string(dir.join("approvals.json")) {
        Ok(json) => serde_json::from_str(&json).context("parsing approvals.json")?,
        Err(_) => ApprovalStore::new(),
    };

    fix_incomplete_turn(&mut store);

    // Migrate legacy summary field to pinned item.
    store.migrate_legacy_summary();

    store.push_turn(vec![ChatMessage::user(RESTART_MESSAGE)]);

    Ok(RestorableSession {
        agent_id: agent_id.clone(),
        session_dir: dir.to_owned(),
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
        tracing::info!("removed incomplete last turn from restored session");
    }
}

const RESTART_MESSAGE: &str = concat!(
    "[system]\n",
    "Session restored from a previous state. Shell sessions have been reset \u{2014}\n",
    "environment variables, working directory, and background processes are no\n",
    "longer available. Review the current state of the project and re-establish\n",
    "any necessary conditions before continuing."
);
