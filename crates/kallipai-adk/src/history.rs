//! Append-only conversation history log.
//!
//! Each agent records every turn to a daily NDJSON file under its agent
//! directory. History files are append-only (O(1) per write) and survive
//! context compaction — evicted turns remain accessible in history.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::context::{Turn, TurnId, TurnKind};
use crate::profile::Profile;
use anyhow::Result;
use just_llm_client::types::generation::{Message, ReasoningEffort, Usage};
use kallipai_common::protocol::Modality;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub mod view;

// ---------------------------------------------------------------------------
// Record types
// ---------------------------------------------------------------------------

/// A sidecar reference to external media attached to a turn. Pure
/// metadata: the binary content lives outside the NDJSON log, this only
/// records what the restored context can claim modality-wise (the wake
/// modality gate reads it; see `ProfileSet::ensure_supports`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AttachmentRef {
    /// The modality of the referenced content.
    pub modality: Modality,
    /// The files-service record id behind the reference, when one exists
    /// (the `--id` ingest form). Purely provenance: sharing and restore
    /// never touch it. Absent for path/blob-form references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<uuid::Uuid>,
    /// MIME type of the referenced media (e.g. `image/png`).
    pub media_type: String,
    /// The reference anchor: the content address of the master copy in
    /// this tagma's `blobs/attachments` area (`sha256-<hex>`, the
    /// `kallipai-blob-store` id). `default` is contractual: hydration
    /// skips unparseable lines silently, so without it one blob_id-less
    /// legacy line would drop the whole turn (text included) from the
    /// restored window. The default value then fails the read-side
    /// parse, which classifies as Gone -- never Transient (a damaged
    /// line must not enter the retry loop) and never an error.
    #[serde(default)]
    pub blob_id: String,
    /// Optional human-readable caption carried alongside the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

/// One round's token accounting, normalized to the five counters the
/// round-history views read.
///
/// Providers report usage in their own shapes: the cache counters are
/// optional and reasoning tokens ride a nested details object. Flattening
/// happens once, at the write point, so a reader never has to know which
/// backend wrote a line. A counter the backend did not report is zero, not
/// absent: zero cache writes and an unreported cache-write counter are the
/// same operational fact.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnUsage {
    /// Prompt tokens billed for this send.
    pub prompt: u32,
    /// Completion tokens the model produced (thinking included where the
    /// provider counts it that way).
    pub completion: u32,
    /// Prompt tokens the provider served from its cache.
    pub cache_read: u32,
    /// Prompt tokens the provider wrote into its cache.
    pub cache_write: u32,
    /// Completion tokens spent thinking (0 when the provider does not say).
    pub reasoning: u32,
}

impl TurnUsage {
    /// Flatten one response's usage. Absent optional counters become zero.
    pub fn from_usage(usage: &Usage) -> Self {
        Self {
            prompt: usage.prompt_tokens,
            completion: usage.completion_tokens,
            cache_read: usage.cache_read_tokens.unwrap_or(0),
            cache_write: usage.cache_write_tokens.unwrap_or(0),
            reasoning: usage
                .completion_tokens_details
                .as_ref()
                .and_then(|d| d.reasoning_tokens)
                .unwrap_or(0),
        }
    }
}

/// What produced a turn: the profile that served it, the round it belongs to,
/// the response's token accounting and the round's clock.
///
/// Recorded on the turn record itself rather than reconstructed later: the
/// profile in flight, the usage a backend reported and the round's duration
/// are all gone by the time anybody reads the log. Absent on records written
/// before the field existed and on every record no model round produced
/// (prompts, injections, heartbeats, repair rewrites): a reader must treat a
/// missing attempt as no model turn, not as missing data. A compaction
/// summary is a model round too -- the summarizer call -- and carries one.
///
/// Every field added here from now on must be optional with a default:
/// the log is append-only, so a required field would make the lines an
/// older agent wrote unreadable to a newer one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TurnAttempt {
    /// Name of the resolved profile set the agent runs against.
    pub profile_set: String,
    /// The profile that served this round (after any failover advance).
    pub profile_id: String,
    /// The provider endpoint id that profile connects through.
    pub provider: String,
    /// The model id the request asked for.
    pub model: String,
    /// The profile's declared effort, as its provider names it: absent when the
    /// profile declares none. The name is what lands on disk, so an upstream
    /// rename of the enum cannot make an old line unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Zero-based index of the round within one run of the round loop: a
    /// heartbeat re-entry restarts it, so one wake can hold several records
    /// that read 0. A compaction summary reads 0 as a sentinel: it takes
    /// no round number; the zero marks the summary, and row order places it.
    pub round: usize,
    /// The response's usage; absent when the backend reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
    /// Wall clock of the round: from entering the round to the response being
    /// consumed -- context composition, token estimation, the in-round
    /// compaction call, any retry or failover wait and the post-stream budget
    /// check included. Tool execution is not. A compaction summary's record
    /// spans only its summarizer call, which starts after the window is built.
    pub elapsed_ms: u64,
}

/// The provider-facing name of a declared effort level ("high", "xhigh"),
/// taken from the type's own wire spelling. The record stores the name, so an
/// upstream rename of the enum cannot make an old line unreadable.
pub fn effort_name(effort: &ReasoningEffort) -> Option<String> {
    serde_json::to_value(effort)
        .ok()?
        .as_str()
        .map(ToOwned::to_owned)
}

impl TurnAttempt {
    /// Build one round's attempt block from the profile that served it.
    ///
    /// The profile in flight, the response's usage and the round's clock are
    /// all gone by the time anybody reads the log, so the write point is the
    /// only place that can record them — and the only place that knows which
    /// of the profile's fields belong on a turn.
    pub fn from_profile(
        set_name: &str,
        profile: &Profile,
        round: usize,
        usage: Option<TurnUsage>,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            profile_set: set_name.to_owned(),
            profile_id: profile.id.clone(),
            provider: profile.endpoint.clone(),
            model: profile.model.clone(),
            effort: profile.effort.as_ref().and_then(effort_name),
            round,
            usage,
            elapsed_ms,
        }
    }
}

/// A single record in the append-only history log.
///
/// Serialized as one NDJSON line. Turn records carry `turn_id`; system records
/// carry `event` instead.
#[derive(Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    /// ISO 8601 UTC timestamp when this record was written.
    #[serde(with = "time::serde::rfc3339")]
    pub datetime: OffsetDateTime,

    /// Turn ID from `ContextStore::push_turn()`. `None` for system records
    /// that are not tied to a specific turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,

    /// The turn's messages in full fidelity.
    #[serde(with = "crate::persisted_message::message_vec")]
    pub messages: Vec<Message>,

    /// Cached token estimate for diagnostics.
    pub estimated_tokens: usize,

    /// Record category: `Turn` for LLM conversation, `System` for tagma events.
    #[serde(default)]
    pub kind: RecordKind,

    /// System event discriminator. Present only when `kind == System`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<SystemEvent>,

    /// Sidecar attachment references. Absent in records written before
    /// this field existed (deserializes to empty — old logs need zero
    /// migration) and omitted when empty on write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,

    /// What produced this turn: the profile that served it, the round it
    /// belongs to, the response token accounting and the round wall clock.
    /// `default` is contractual: history lines written before this field
    /// existed must keep hydrating, and the read side classifies an
    /// unparseable line as damaged, which a missing optional key must not
    /// trigger. Omitted on write when absent so the log stays compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<TurnAttempt>,
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
    /// A compaction attempt failed and the stretch ended.
    ///
    /// Carries what the operator acts on: how many sends the stretch spent,
    /// against which cap, the class the shared table gave the failure, and
    /// the rendered detail.
    CompactionFailure {
        /// Sends charged to the failing stretch.
        attempt: u32,
        /// The policy's own cap for that stretch.
        max_attempts: u32,
        /// The failure class's own label.
        class: String,
        /// The rendered backend error.
        detail: String,
    },
    /// Legacy log event; kept so older history lines still parse.
    ReferenceInvalidated {
        /// The turn whose sidecar record carries the invalidated reference.
        turn_id: u64,
        /// The files-service record id of the invalidated media.
        record_id: uuid::Uuid,
        /// Why the reference was invalidated — carried so the recovery
        /// truth (what failed, and that it was not silent) survives
        /// restarts with the event.
        reason: String,
    },
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
    #[allow(clippy::too_many_arguments)] // the record's own shape: one argument per field it writes
    pub fn append(
        &self,
        turn_id: Option<u64>,
        messages: &[Message],
        estimated_tokens: usize,
        kind: RecordKind,
        event: Option<SystemEvent>,
        attachments: &[AttachmentRef],
        attempt: Option<&TurnAttempt>,
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
            attachments: attachments.to_vec(),
            attempt: attempt.cloned(),
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
    /// The window's attachment sidecar references, keyed by turn id:
    /// carried out of the read pass (the history records were in hand)
    /// so the restore's re-assembly never rescans the log.
    pub sidecar: BTreeMap<u64, Vec<AttachmentRef>>,
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
    let mut sidecar: BTreeMap<u64, Vec<AttachmentRef>> = BTreeMap::new();

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
                        if !rec.attachments.is_empty() {
                            sidecar.insert(id, rec.attachments);
                        }
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
            sidecar,
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
pub(crate) fn tail_turns_within_budget(
    agent_dir: &Path,
    budget: usize,
) -> (Vec<Turn>, BTreeMap<u64, Vec<AttachmentRef>>) {
    let mut turns = Vec::new();
    let mut used = 0usize;
    let mut seen: HashSet<u64> = HashSet::new();
    let mut sidecar: BTreeMap<u64, Vec<AttachmentRef>> = BTreeMap::new();
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
            if !rec.attachments.is_empty() {
                sidecar.insert(id, rec.attachments);
            }
        }
    }
    turns.reverse();
    (turns, sidecar)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{assistant_msg, tool_result_msg, user_msg};
    use just_llm_client::types::generation::CompletionTokensDetails;

    fn tmp_agent_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    #[test]
    fn turn_record_roundtrip() {
        let dir = tmp_agent_dir();
        let writer = HistoryWriter::new(dir.path().to_owned());

        let msgs = vec![user_msg("hello, world")];
        writer
            .append(Some(0), &msgs, 16, RecordKind::Turn, None, &[], None)
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

        let msgs = vec![user_msg("summary of turns 1-5")];
        writer
            .append(
                None,
                &msgs,
                200,
                RecordKind::System,
                Some(SystemEvent::CompactionSummary),
                &[],
                None,
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
                &[user_msg("a")],
                16,
                RecordKind::Turn,
                None,
                &[],
                None,
            )
            .unwrap();
        writer
            .append(
                Some(1),
                &[assistant_msg("b")],
                16,
                RecordKind::Turn,
                None,
                &[],
                None,
            )
            .unwrap();
        writer
            .append(
                None,
                &[user_msg("restored")],
                32,
                RecordKind::System,
                Some(SystemEvent::AgentRestore),
                &[],
                None,
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
                &[user_msg("x")],
                16,
                RecordKind::Turn,
                None,
                &[],
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
        let msgs = vec![tool_result_msg("line1\nline2\nline3", "call_1")];
        writer
            .append(Some(0), &msgs, 32, RecordKind::Turn, None, &[], None)
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
            messages: vec![user_msg(text)],
            estimated_tokens: 8,
            kind: RecordKind::Turn,
            event: None,
            attachments: Vec::new(),
            attempt: None,
        })
        .unwrap()
    }

    fn system_line() -> String {
        serde_json::to_string(&HistoryRecord {
            datetime: OffsetDateTime::now_utc(),
            turn_id: None,
            messages: vec![user_msg("restore notice")],
            estimated_tokens: 4,
            kind: RecordKind::System,
            event: Some(SystemEvent::AgentRestore),
            attachments: Vec::new(),
            attempt: None,
        })
        .unwrap()
    }

    fn turn_line_sized(id: Option<u64>, text: &str, tokens: usize) -> String {
        serde_json::to_string(&HistoryRecord {
            datetime: OffsetDateTime::now_utc(),
            turn_id: id,
            messages: vec![user_msg(text)],
            estimated_tokens: tokens,
            kind: RecordKind::Turn,
            event: None,
            attachments: Vec::new(),
            attempt: None,
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
            ids(tail_turns_within_budget(dir.path(), 20).0),
            Vec::<u64>::new()
        );
        assert_eq!(
            ids(tail_turns_within_budget(dir.path(), 37).0),
            vec![4],
            "30-token newest fits, the next 8-token turn would cross"
        );
        assert_eq!(
            ids(tail_turns_within_budget(dir.path(), 54).0),
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
        assert!(tail_turns_within_budget(dir.path(), 0).0.is_empty());
        assert!(tail_turns_within_budget(dir.path(), 8).0.len() == 1);
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
        let (turns, _sidecar) = tail_turns_within_budget(dir.path(), 16);
        let ids: Vec<u64> = turns.iter().map(|t| t.id.0).collect();
        assert_eq!(ids, vec![2, 3]);
        assert_eq!(turns[1].messages[0].content(), Some("repaired"));
    }

    fn attachment(modality: Modality, record_id: u64) -> AttachmentRef {
        AttachmentRef {
            modality,
            record_id: Some(uuid::Uuid::from_bytes([
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                record_id as u8,
            ])),
            media_type: "image/png".to_owned(),
            blob_id: format!("sha256-{record_id:064x}"),
            caption: Some("a chart".to_owned()),
        }
    }

    #[test]
    fn attachment_sidecar_roundtrip_and_old_record_reads_empty() {
        let rec = HistoryRecord {
            datetime: OffsetDateTime::now_utc(),
            turn_id: Some(7),
            messages: vec![user_msg("see chart")],
            estimated_tokens: 8,
            kind: RecordKind::Turn,
            event: None,
            attachments: vec![attachment(Modality::Image, 7)],
            attempt: None,
        };
        let line = serde_json::to_string(&rec).unwrap();
        let back: HistoryRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.attachments, rec.attachments);
        // Caption round-trips both ways: None is omitted on write and
        // deserializes back to None; the Some arm is covered above.
        let mut uncaptioned = rec.clone();
        uncaptioned.attachments[0].caption = None;
        let line = serde_json::to_string(&uncaptioned).unwrap();
        assert!(!line.contains("caption"), "got: {line}");
        let back: HistoryRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.attachments[0].caption, None);

        // Empty sidecar is omitted on write.
        let mut bare = rec.clone();
        bare.attachments = Vec::new();
        let line = serde_json::to_string(&bare).unwrap();
        assert!(!line.contains("attachments"), "got: {line}");

        // Records written before the field existed deserialize to empty —
        // old logs need zero migration.
        let stamp = rec
            .datetime
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let old = format!(
            r#"{{"datetime":"{stamp}","turn_id":1,"messages":[],"estimated_tokens":8,"kind":"turn"}}"#,
        );
        let parsed: HistoryRecord = serde_json::from_str(&old).unwrap();
        assert!(parsed.attachments.is_empty());
    }

    /// A reference-invalidation event for `turn_id` about record `seed`.
    fn invalidated_event(turn_id: u64, seed: u64) -> SystemEvent {
        SystemEvent::ReferenceInvalidated {
            turn_id,
            record_id: attachment(Modality::Image, seed).record_id.unwrap(),
            reason: "files record no longer exists".to_owned(),
        }
    }

    #[test]
    fn reference_invalidated_roundtrip() {
        let rec = HistoryRecord {
            datetime: OffsetDateTime::now_utc(),
            turn_id: None,
            messages: vec![],
            estimated_tokens: 0,
            kind: RecordKind::System,
            event: Some(invalidated_event(7, 3)),
            attachments: vec![],
            attempt: None,
        };
        let line = serde_json::to_string(&rec).unwrap();
        assert!(line.contains("reference_invalidated"), "got: {line}");
        let back: HistoryRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.event, rec.event);

        // Records written before the variant existed still read: the old
        // shape has no `event` key at all.
        let stamp = rec
            .datetime
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let old = format!(
            r#"{{"datetime":"{stamp}","turn_id":null,"messages":[],"estimated_tokens":0,"kind":"system"}}"#,
        );
        let parsed: HistoryRecord = serde_json::from_str(&old).unwrap();
        assert!(parsed.event.is_none());
    }

    #[test]
    fn hydrate_sidecar_newest_wins() {
        let dir = tmp_agent_dir();
        let writer = HistoryWriter::new(dir.path().to_owned());

        // Turn 7 recorded twice: the newest record's refs and messages win,
        // mirroring the same-id resolution the turns and the sidecar share.
        writer
            .append(
                Some(7),
                &[user_msg("older")],
                8,
                RecordKind::Turn,
                None,
                &[attachment(Modality::Image, 1)],
                None,
            )
            .unwrap();
        writer
            .append(
                Some(7),
                &[user_msg("newest")],
                8,
                RecordKind::Turn,
                None,
                &[attachment(Modality::Image, 2)],
                None,
            )
            .unwrap();
        writer
            .append(
                Some(9),
                &[user_msg("kept")],
                8,
                RecordKind::Turn,
                None,
                &[attachment(Modality::Image, 5)],
                None,
            )
            .unwrap();

        let (turns, report) = hydrate_turns(dir.path(), &[7, 9]);
        assert_eq!(report.sidecar[&7], vec![attachment(Modality::Image, 2)]);
        assert_eq!(report.sidecar[&9], vec![attachment(Modality::Image, 5)]);
        let seven = turns.iter().find(|t| t.id.0 == 7).unwrap();
        assert_eq!(seven.messages[0].content(), Some("newest"));
    }

    /// A line written before the attempt field existed still hydrates: the key
    /// is absent rather than null, and absence must read as no model turn.
    #[test]
    fn an_old_line_without_attempt_reads_as_no_model_turn() {
        let json = r#"{"datetime":"2026-06-08T12:00:00Z","turn_id":5,"messages":[{"role":"user","content":"x"}],"estimated_tokens":16}"#;
        let record: HistoryRecord = serde_json::from_str(json).unwrap();
        assert!(record.attempt.is_none());
    }

    /// What a round records survives the round trip: a reader of the log sees
    /// the profile, the round, the flattened usage and the clock as written.
    #[test]
    fn attempt_roundtrips_profile_usage_and_clock() {
        let dir = tmp_agent_dir();
        let writer = HistoryWriter::new(dir.path().to_owned());
        let attempt = TurnAttempt {
            profile_set: "default".to_owned(),
            profile_id: "a".to_owned(),
            provider: "p1".to_owned(),
            model: "a-model".to_owned(),
            effort: Some("high".to_owned()),
            round: 2,
            usage: Some(TurnUsage {
                prompt: 120,
                completion: 30,
                cache_read: 80,
                cache_write: 40,
                reasoning: 7,
            }),
            elapsed_ms: 1234,
        };
        writer
            .append(
                Some(0),
                &[user_msg("hello")],
                16,
                RecordKind::Turn,
                None,
                &[],
                Some(&attempt),
            )
            .unwrap();

        let line = std::fs::read_to_string(today_path(&dir.path().join("history"))).unwrap();
        let record: HistoryRecord = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(record.attempt, Some(attempt));
        assert!(
            line.contains("\"high\""),
            "the effort level travels: {line}"
        );
    }

    /// Every optional counter is read on its own: a backend that reports the
    /// cache and reasoning counters keeps them in their own columns, and one
    /// that reports none lands where a reported zero lands.
    #[test]
    fn from_usage_flattens_each_counter_by_its_own_name() {
        let bare = Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            total_tokens: 14,
            cache_read_tokens: None,
            cache_write_tokens: None,
            completion_tokens_details: None,
        };
        assert_eq!(
            TurnUsage::from_usage(&bare),
            TurnUsage {
                prompt: 10,
                completion: 4,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0
            }
        );

        let full = Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            total_tokens: 14,
            cache_read_tokens: Some(7),
            cache_write_tokens: Some(3),
            completion_tokens_details: Some(CompletionTokensDetails {
                reasoning_tokens: Some(2),
            }),
        };
        assert_eq!(
            TurnUsage::from_usage(&full),
            TurnUsage {
                prompt: 10,
                completion: 4,
                cache_read: 7,
                cache_write: 3,
                reasoning: 2
            }
        );

        let zeros = Usage {
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            completion_tokens_details: Some(CompletionTokensDetails {
                reasoning_tokens: Some(0),
            }),
            ..full.clone()
        };
        assert_eq!(
            TurnUsage::from_usage(&zeros),
            TurnUsage {
                prompt: 10,
                completion: 4,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0
            }
        );
    }

    /// A record no model round produced omits the key entirely: the log stays
    /// compact, and an absent attempt stays distinct from a null one.
    #[test]
    fn a_record_without_an_attempt_omits_the_key() {
        let dir = tmp_agent_dir();
        let writer = HistoryWriter::new(dir.path().to_owned());
        writer
            .append(
                Some(0),
                &[user_msg("x")],
                16,
                RecordKind::Turn,
                None,
                &[],
                None,
            )
            .unwrap();
        let line = std::fs::read_to_string(today_path(&dir.path().join("history"))).unwrap();
        assert!(!line.contains("attempt"), "the key is omitted: {line}");
    }
}
