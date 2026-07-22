//! Append-only conversation history log.
//!
//! Each agent records every turn to a daily NDJSON file under its agent
//! directory. History files are append-only (O(1) per write) and survive
//! context compaction — evicted turns remain accessible in history.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use just_llm_client::types::chat::ChatMessage;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Record types
// ---------------------------------------------------------------------------

/// A single record in the append-only history log.
///
/// Serialized as one NDJSON line. Turn records carry `turn_id`; system records
/// carry `event` instead.
#[derive(Serialize, Deserialize)]
pub struct HistoryRecord {
    /// ISO 8601 UTC timestamp when this record was written.
    #[serde(with = "time::serde::rfc3339")]
    pub datetime: OffsetDateTime,

    /// Turn ID from `ContextStore::push_turn()`. `None` for system records
    /// that are not tied to a specific turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,

    /// The turn's messages in full fidelity.
    pub messages: Vec<ChatMessage>,

    /// Cached token estimate for diagnostics.
    pub estimated_tokens: usize,

    /// Record category: `Turn` for LLM conversation, `System` for tagma events.
    #[serde(default)]
    pub kind: RecordKind,

    /// System event discriminator. Present only when `kind == System`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<SystemEvent>,
}

/// Distinguishes conversation turns from system events in the history log.
#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    /// A normal conversation turn (user, assistant, tool calls, tool results).
    #[default]
    Turn,
    /// A tagma/system event (agent restore, compaction, etc.).
    System,
}

/// Specific system event types. Extensible — new variants can be added without
/// changing the `RecordKind` enum.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SystemEvent {
    /// Agent restored from a previous state on tagma restart.
    AgentRestore,
    /// Context compaction summarized and evicted turns.
    CompactionSummary,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Append-only conversation history writer for a single agent.
///
/// Owns the agent directory path. All write operations go to
/// `<agent_dir>/history/YYYY-MM-DD.ndjson`. Not `Sync` — each agent runs as
/// a single sequential task, so no concurrent access occurs.
pub struct HistoryWriter {
    agent_dir: PathBuf,
}

impl HistoryWriter {
    /// Create a new writer targeting the given agent directory.
    pub fn new(agent_dir: PathBuf) -> Self {
        Self { agent_dir }
    }

    /// Append a record to today's NDJSON history file.
    ///
    /// Opens the file for append on each call (no cached handle). This is
    /// intentional — it avoids file-handle lifetime issues across daily
    /// rotation boundaries and is consistent with the `persist()` pattern
    /// used elsewhere.
    pub fn append(
        &self,
        turn_id: Option<u64>,
        messages: &[ChatMessage],
        estimated_tokens: usize,
        kind: RecordKind,
        event: Option<SystemEvent>,
    ) -> Result<()> {
        let history_dir = ensure_history_dir(&self.agent_dir)?;
        let path = today_path(&history_dir);

        let record = HistoryRecord {
            datetime: OffsetDateTime::now_utc(),
            turn_id,
            messages: messages.to_vec(),
            estimated_tokens,
            kind,
            event,
        };

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;

        // Best-effort durability. Non-blocking on failure.
        let _ = file.sync_data();

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Ensure the `history/` subdirectory exists under the agent directory.
/// Lazy — only created on first write. `create_dir_all` is idempotent.
fn ensure_history_dir(agent_dir: &std::path::Path) -> Result<PathBuf> {
    let history_dir = agent_dir.join("history");
    std::fs::create_dir_all(&history_dir)?;
    Ok(history_dir)
}

/// Resolve today's history file path.
fn today_path(history_dir: &std::path::Path) -> PathBuf {
    let date_str = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Iso8601::DATE)
        .expect("ISO 8601 date formatting is infallible");
    history_dir.join(format!("{date_str}.ndjson"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_agent_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    #[test]
    fn turn_record_roundtrip() {
        let dir = tmp_agent_dir();
        let writer = HistoryWriter::new(dir.path().to_owned());

        let msgs = vec![ChatMessage::user("hello, world")];
        writer
            .append(Some(0), &msgs, 16, RecordKind::Turn, None)
            .unwrap();

        // Verify history/ directory was created.
        assert!(dir.path().join("history").exists());

        // Read back and parse the NDJSON line.
        let ndjson_path = today_path(&dir.path().join("history"));
        let content = std::fs::read_to_string(&ndjson_path).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 1);

        let record: HistoryRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(record.turn_id, Some(0));
        assert_eq!(record.kind, RecordKind::Turn);
        assert!(record.event.is_none());
        assert_eq!(record.estimated_tokens, 16);
        assert_eq!(record.messages.len(), 1);
        assert_eq!(record.messages[0].content(), Some("hello, world"));
    }

    #[test]
    fn system_record_roundtrip() {
        let dir = tmp_agent_dir();
        let writer = HistoryWriter::new(dir.path().to_owned());

        let msgs = vec![ChatMessage::assistant("summary of turns 1-5")];
        writer
            .append(
                None,
                &msgs,
                200,
                RecordKind::System,
                Some(SystemEvent::CompactionSummary),
            )
            .unwrap();

        let ndjson_path = today_path(&dir.path().join("history"));
        let content = std::fs::read_to_string(&ndjson_path).unwrap();
        let record: HistoryRecord = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(record.turn_id, None);
        assert_eq!(record.kind, RecordKind::System);
        assert_eq!(record.event, Some(SystemEvent::CompactionSummary));
    }

    #[test]
    fn multiple_records_append() {
        let dir = tmp_agent_dir();
        let writer = HistoryWriter::new(dir.path().to_owned());

        writer
            .append(
                Some(0),
                &[ChatMessage::user("a")],
                16,
                RecordKind::Turn,
                None,
            )
            .unwrap();
        writer
            .append(
                Some(1),
                &[ChatMessage::assistant("b")],
                16,
                RecordKind::Turn,
                None,
            )
            .unwrap();
        writer
            .append(
                None,
                &[ChatMessage::user("restored")],
                32,
                RecordKind::System,
                Some(SystemEvent::AgentRestore),
            )
            .unwrap();

        let ndjson_path = today_path(&dir.path().join("history"));
        let content = std::fs::read_to_string(&ndjson_path).unwrap();
        let records: Vec<HistoryRecord> = content
            .trim()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(records.len(), 3);
        assert_eq!(records[0].turn_id, Some(0));
        assert_eq!(records[1].turn_id, Some(1));
        assert_eq!(records[2].kind, RecordKind::System);
    }

    #[test]
    fn record_kind_default_is_turn() {
        // Verify that missing "kind" field deserializes as Turn.
        let json = r#"{"datetime":"2026-06-08T12:00:00Z","turn_id":5,"messages":[{"role":"user","content":"x"}],"estimated_tokens":16}"#;
        let record: HistoryRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.kind, RecordKind::Turn);
    }

    #[test]
    fn lazy_directory_creation() {
        let dir = tmp_agent_dir();
        // No history/ dir before any writes.
        assert!(!dir.path().join("history").exists());

        let writer = HistoryWriter::new(dir.path().to_owned());
        writer
            .append(
                Some(0),
                &[ChatMessage::user("x")],
                16,
                RecordKind::Turn,
                None,
            )
            .unwrap();

        // Now it exists.
        assert!(dir.path().join("history").exists());
    }

    #[test]
    fn multi_line_tool_result_preserves_line_boundary() {
        let dir = tmp_agent_dir();
        let writer = HistoryWriter::new(dir.path().to_owned());

        // Tool result with embedded newlines — must NOT break NDJSON line boundary.
        let msgs = vec![ChatMessage::tool_result("line1\nline2\nline3", "call_1")];
        writer
            .append(Some(0), &msgs, 32, RecordKind::Turn, None)
            .unwrap();

        let ndjson_path = today_path(&dir.path().join("history"));
        let content = std::fs::read_to_string(&ndjson_path).unwrap();
        // Exactly one line (one trailing newline).
        assert_eq!(content.trim().lines().count(), 1);

        // Roundtrip preserves newlines inside content.
        let record: HistoryRecord = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(record.messages[0].content(), Some("line1\nline2\nline3"));
    }
}
