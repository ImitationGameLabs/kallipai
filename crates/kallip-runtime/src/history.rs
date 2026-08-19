//! Append-only conversation history log.
//!
//! Each agent records every turn to a daily NDJSON file under its agent
//! directory. History files are append-only (O(1) per write) and survive
//! context compaction — evicted turns remain accessible in history.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::context::{Turn, TurnId, TurnKind};
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
// Reader (hydration)
// ---------------------------------------------------------------------------

/// Outcome of a hydration scan: what could not be read.
#[derive(Debug, Default)]
pub(crate) struct HydrationReport {
    /// Lines that failed to parse (torn tail, corruption). Skipped, not fatal.
    pub bad_lines: usize,
    /// Wanted turn IDs with no readable history record.
    pub missing_ids: Vec<u64>,
}

/// Per-day NDJSON files under `<agent_dir>/history/`, chronologically.
///
/// File names are UTC dates (`YYYY-MM-DD.ndjson`) generated by `today_path`,
/// so lexicographic order is chronological order — this is a format
/// invariant, tied to the UTC day boundary.
fn history_files(agent_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(agent_dir.join("history")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ndjson"))
        .collect();
    files.sort();
    files
}

/// Whether the agent has any history files at all — used by restore to
/// distinguish "nothing was ever persisted" from "the split documents
/// went missing but the history tail can rebuild the window".
pub(crate) fn has_history(agent_dir: &Path) -> bool {
    !history_files(agent_dir).is_empty()
}

/// Hydrate the conversation turns for `ids` from the history log.
///
/// Scans per-day files **newest-first**, and lines within a file newest-first
/// too — the live window's turns are always the most recent, so the wanted
/// IDs live in the tail and the scan stops after one or two files
/// (`wanted` empty → stop). Unparseable lines are skipped and counted: a
/// torn tail or corrupt line must not abort a restore. System records
/// never match (they carry no turn ID and are filtered by kind). Returns
/// the turns in ascending ID order, matching the manifest's
/// `conversation_turn_ids`.
///
/// A turn ID appearing in more than one record resolves to the newest:
/// the first match wins and `wanted` never re-admits the ID. Offline
/// repair relies on this — it appends corrected messages under the
/// same turn ID instead of rewriting the damaged record.
pub(crate) fn hydrate_turns(agent_dir: &Path, ids: &[u64]) -> (Vec<Turn>, HydrationReport) {
    let mut wanted: HashSet<u64> = ids.iter().copied().collect();
    let mut turns = Vec::new();
    let mut bad_lines = 0usize;

    'files: for path in history_files(agent_dir).into_iter().rev() {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines().rev() {
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<HistoryRecord>(line) {
                Ok(rec) => {
                    if rec.kind != RecordKind::Turn {
                        continue;
                    }
                    let Some(id) = rec.turn_id else {
                        continue;
                    };
                    if wanted.remove(&id) {
                        turns.push(Turn {
                            id: TurnId(id),
                            messages: rec.messages,
                            estimated_tokens: rec.estimated_tokens,
                            kind: TurnKind::Conversation,
                        });
                        if wanted.is_empty() {
                            break 'files;
                        }
                    }
                }
                Err(_) => bad_lines += 1,
            }
        }
    }

    let mut missing_ids: Vec<u64> = wanted.into_iter().collect();
    missing_ids.sort_unstable();
    turns.sort_by_key(|t| t.id.0);
    (
        turns,
        HydrationReport {
            bad_lines,
            missing_ids,
        },
    )
}

/// Highest turn ID ever recorded in the history log (`0` when none).
///
/// Used by the manifest-loss rebuild to keep `next_turn_id` ahead of every
/// historical ID: reusing an ID would silently collide manifest references
/// with different content.
pub(crate) fn max_turn_id(agent_dir: &Path) -> u64 {
    for path in history_files(agent_dir).into_iter().rev() {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines().rev() {
            let Ok(rec) = serde_json::from_str::<HistoryRecord>(line) else {
                continue;
            };
            if rec.kind == RecordKind::Turn
                && let Some(id) = rec.turn_id
            {
                return id;
            }
        }
    }
    0
}

/// Newest turns whose cached estimates fit `budget` tokens, ascending.
///
/// The manifest-loss rebuild window: scans day files (and lines within
/// them) newest-first, accumulating `estimated_tokens` until a turn would
/// cross the budget, which excludes it — so the kept window never exceeds
/// the budget and a budget of ~0 degenerates to an empty window (pins and
/// fresh turns still boot). Unparseable lines are skipped: this path only
/// runs on an already-wrecked directory, where the tail rebuild itself is
/// the recorded degradation.
///
/// A turn ID already collected is skipped: repair appends the corrected
/// messages under the same ID, and the older duplicate must neither
/// double-count the budget nor enter the window twice.
pub(crate) fn tail_turns_within_budget(agent_dir: &Path, budget: usize) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut used = 0usize;
    let mut seen: HashSet<u64> = HashSet::new();
    'files: for path in history_files(agent_dir).into_iter().rev() {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines().rev() {
            let Ok(rec) = serde_json::from_str::<HistoryRecord>(line) else {
                continue;
            };
            if rec.kind != RecordKind::Turn {
                continue;
            }
            let Some(id) = rec.turn_id else {
                continue;
            };
            // Older duplicate of a repaired turn: the newest record
            // already won above — count it once, not twice.
            if !seen.insert(id) {
                continue;
            }
            if used + rec.estimated_tokens > budget {
                break 'files;
            }
            used += rec.estimated_tokens;
            turns.push(Turn {
                id: TurnId(id),
                messages: rec.messages,
                estimated_tokens: rec.estimated_tokens,
                kind: TurnKind::Conversation,
            });
        }
    }
    turns.reverse();
    turns
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

    // --- reader (hydration) ---

    /// Write records into an explicitly-named daily file, so tests can lay
    /// out multiple days without depending on the wall clock.
    fn write_day(dir: &std::path::Path, date: &str, lines: &[String]) {
        let history_dir = dir.join("history");
        std::fs::create_dir_all(&history_dir).unwrap();
        std::fs::write(
            history_dir.join(format!("{date}.ndjson")),
            lines.join("\n") + "\n",
        )
        .unwrap();
    }

    fn turn_line(id: Option<u64>, text: &str) -> String {
        serde_json::to_string(&HistoryRecord {
            datetime: OffsetDateTime::now_utc(),
            turn_id: id,
            messages: vec![ChatMessage::user(text)],
            estimated_tokens: 8,
            kind: RecordKind::Turn,
            event: None,
        })
        .unwrap()
    }

    fn system_line() -> String {
        serde_json::to_string(&HistoryRecord {
            datetime: OffsetDateTime::now_utc(),
            turn_id: None,
            messages: vec![ChatMessage::user("restore notice")],
            estimated_tokens: 4,
            kind: RecordKind::System,
            event: Some(SystemEvent::AgentRestore),
        })
        .unwrap()
    }

    fn turn_line_sized(id: Option<u64>, text: &str, tokens: usize) -> String {
        serde_json::to_string(&HistoryRecord {
            datetime: OffsetDateTime::now_utc(),
            turn_id: id,
            messages: vec![ChatMessage::user(text)],
            estimated_tokens: tokens,
            kind: RecordKind::Turn,
            event: None,
        })
        .unwrap()
    }

    #[test]
    fn hydrate_spans_daily_files_skips_system_and_sorts_ascending() {
        let dir = tmp_agent_dir();
        write_day(
            dir.path(),
            "2026-08-18",
            &[turn_line(Some(1), "old"), turn_line(Some(2), "kept-old")],
        );
        write_day(
            dir.path(),
            "2026-08-19",
            &[
                system_line(),
                turn_line(Some(5), "kept-new"),
                turn_line(Some(4), "kept-mid"),
            ],
        );

        let (turns, report) = hydrate_turns(dir.path(), &[2, 4, 5]);
        assert!(report.missing_ids.is_empty());
        assert_eq!(report.bad_lines, 0);
        let ids: Vec<u64> = turns.iter().map(|t| t.id.0).collect();
        assert_eq!(ids, vec![2, 4, 5], "ascending regardless of scan order");
        assert_eq!(turns[0].messages[0].content(), Some("kept-old"));
        assert!(turns.iter().all(|t| !t.is_pinned()));
    }

    #[test]
    fn hydrate_counts_bad_lines_and_reports_missing_ids() {
        let dir = tmp_agent_dir();
        write_day(
            dir.path(),
            "2026-08-19",
            &[
                turn_line(Some(7), "good"),
                "{not json".to_string(),
                turn_line(Some(8), "after-bad"),
            ],
        );
        // A corrupt line mid-file is skipped; parsing continues past it.

        let (turns, report) = hydrate_turns(dir.path(), &[7, 8, 9]);
        assert_eq!(
            report.bad_lines, 1,
            "the malformed line is skipped, counted"
        );
        assert_eq!(report.missing_ids, vec![9]);
        assert_eq!(turns.len(), 2, "records around the bad line still hydrate");
        assert_eq!(turns[1].id.0, 8);
    }
    /// The same turn ID in two day files resolves to the newest record —
    /// the shape offline repair produces when it appends corrected
    /// messages under the damaged turn's ID.
    #[test]
    fn hydrate_same_turn_id_resolves_to_newest_record() {
        let dir = tmp_agent_dir();
        write_day(dir.path(), "2026-08-18", &[turn_line(Some(3), "damaged")]);
        write_day(dir.path(), "2026-08-19", &[turn_line(Some(3), "repaired")]);

        let (turns, report) = hydrate_turns(dir.path(), &[3]);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].messages[0].content(), Some("repaired"));
        assert!(report.missing_ids.is_empty());
        assert_eq!(report.bad_lines, 0);
    }

    #[test]
    fn max_turn_id_finds_newest_turn_record() {
        let dir = tmp_agent_dir();
        write_day(dir.path(), "2026-08-18", &[turn_line(Some(3), "a")]);
        write_day(
            dir.path(),
            "2026-08-19",
            &[system_line(), turn_line(Some(12), "newest")],
        );
        assert_eq!(max_turn_id(dir.path()), 12);

        // System-only tail keeps scanning back to the older turn record.
        let dir2 = tmp_agent_dir();
        write_day(dir2.path(), "2026-08-18", &[turn_line(Some(4), "old")]);
        write_day(dir2.path(), "2026-08-19", &[system_line()]);
        assert_eq!(max_turn_id(dir2.path()), 4);

        // No history at all.
        let dir3 = tmp_agent_dir();
        assert_eq!(max_turn_id(dir3.path()), 0);
    }

    /// The tail window keeps the newest turns that fit the budget, spans
    /// day files, skips system records, and excludes a turn that would
    /// cross the line.
    #[test]
    fn tail_window_fits_budget_newest_first() {
        let dir = tmp_agent_dir();
        write_day(
            dir.path(),
            "2026-08-18",
            &[
                turn_line_sized(Some(1), "old", 8),
                turn_line_sized(Some(2), "mid", 8),
            ],
        );
        write_day(
            dir.path(),
            "2026-08-19",
            &[
                turn_line_sized(Some(3), "new-a", 8),
                turn_line_sized(Some(4), "new-b", 30),
            ],
        );
        // The 30-token newest turn fits only a generous budget.
        let ids = |ts: Vec<Turn>| ts.iter().map(|t| t.id.0).collect::<Vec<_>>();
        assert_eq!(
            ids(tail_turns_within_budget(dir.path(), 20)),
            Vec::<u64>::new()
        );
        assert_eq!(
            ids(tail_turns_within_budget(dir.path(), 37)),
            vec![4],
            "30-token newest fits, the next 8-token turn would cross"
        );
        assert_eq!(
            ids(tail_turns_within_budget(dir.path(), 54)),
            vec![1, 2, 3, 4],
            "spans days, ascending"
        );
    }

    /// Budget ~0 keeps the old c8 degenerate: an empty conversation window
    /// rather than an over-budget one.
    #[test]
    fn tail_window_zero_budget_is_empty() {
        let dir = tmp_agent_dir();
        write_day(
            dir.path(),
            "2026-08-19",
            &[turn_line(Some(9), "whatever"), system_line()],
        );
        assert!(tail_turns_within_budget(dir.path(), 0).is_empty());
        assert!(tail_turns_within_budget(dir.path(), 8).len() == 1);
    }

    /// A repaired turn's older duplicate counts once: no double budget,
    /// no duplicate window entry. Same-day shape — repair appends to the
    /// file the damaged record lives in.
    #[test]
    fn tail_rebuild_counts_a_repaired_turn_once() {
        let dir = tmp_agent_dir();
        write_day(
            dir.path(),
            "2026-08-19",
            &[
                turn_line_sized(Some(2), "before", 8),
                turn_line_sized(Some(3), "damaged", 8),
                turn_line_sized(Some(3), "repaired", 8),
            ],
        );

        // Budget 16 fits both turns only if the duplicate is skipped.
        let turns = tail_turns_within_budget(dir.path(), 16);
        let ids: Vec<u64> = turns.iter().map(|t| t.id.0).collect();
        assert_eq!(ids, vec![2, 3]);
        assert_eq!(turns[1].messages[0].content(), Some("repaired"));
    }
}
