//! Turn-history wire types: the paged read face over an agent's append-only
//! turn log.
//!
//! The log stays the single source of truth — these shapes are a projection
//! computed on demand, never a second store. `schema_version` rides every
//! response so a client refuses a face whose fields changed meaning instead
//! of misreading it.
//!
//! Offsets and ranges address the cleaned text: spill paths are reclaimed
//! before anything is measured, cut or emitted. A counter does not share that
//! rule, so each one says which text it measures: the sizes and line counts a
//! call row carries measure the record as the log holds it, spill paths
//! included; a message row's `chars` counts the message text the same way,
//! before any shortening; and a call body's `total_bytes` and `reclaimed`
//! measure the text after reclaiming.

use serde::{Deserialize, Serialize};

/// Version of the read face. Bumped when a field changes meaning or goes
/// away, so an older client stops instead of misreading a page.
pub const TURN_SCHEMA_VERSION: u32 = 1;

/// Rows per page when the request carries no `limit`.
pub const TURNS_PAGE_LIMIT_DEFAULT: u32 = 50;

/// Rows per page are clamped to this, whatever the request asks for.
pub const TURNS_PAGE_LIMIT_MAX: u32 = 200;

/// A page stops once its rows reach this serialized size. One turn carrying
/// a wall of tool output cannot blow up a response, and the row limit is not
/// a byte bound on its own.
pub const TURNS_PAGE_MAX_BYTES: usize = 128 * 1024;

/// Characters in a shortened text field: half from the head, half from the
/// tail, the middle elided. Tool arguments and message text can both be
/// enormous, and the identifying facts sit at the ends.
pub const TEXT_CUT_CHARS: usize = 400;

/// Bytes per call-body range read when the request carries no `limit`.
pub const CALL_BODY_LIMIT_DEFAULT: usize = 16 * 1024;

/// Bytes per call-body range read, clamped to this.
pub const CALL_BODY_MAX_BYTES: usize = 64 * 1024;

/// Stands in for a spill path: a tool stream parked in a temporary file. The
/// file is temporary and may already be gone, so the read face reports the
/// reclaim — it never hands out a path to follow.
pub const SPILL_RECLAIMED: &str = "[temporary stream reclaimed]";

/// Query for one page of turns.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnsQuery {
    /// The UTC day file to read, `YYYY-MM-DD`. Absent pages backwards from
    /// the newest day.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day: Option<String>,
    /// Opaque cursor taken from a previous page's `next_cursor`. Opaque by
    /// design: the ordering key is the server's to change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// Rows wanted, clamped to 1..=[`TURNS_PAGE_LIMIT_MAX`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Query for one range of a call's text.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CallBodyQuery {
    /// Byte offset into the cleaned text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Bytes wanted, clamped to 1..=[`CALL_BODY_MAX_BYTES`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// One page of turns, newest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnsPage {
    pub schema_version: u32,
    pub rows: Vec<TurnRow>,
    /// Unreadable log lines skipped while building this page, counted over
    /// the lines this page actually walked.
    pub bad_lines: u32,
    /// Cursor for the next (older) page; absent when the log is exhausted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// True when the byte cap, not the row limit, ended the page early.
    pub page_truncated: bool,
}

/// One turn as the read face shows it.
///
/// The model-round facts are nested and optional: a record no model round
/// produced (a prompt, a heartbeat) has no profile, no usage and no clock,
/// and a repair row shows the round folded in from an older row of the same
/// turn, when the walk reaches it. An absent block says that in one look.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRow {
    pub turn_id: u64,
    /// The record's own timestamp, RFC 3339 with the precision it was
    /// written at.
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<TurnProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsageView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// Tool calls the round's assistant messages carried, in the order the
    /// model sent them.
    pub calls: Vec<CallRow>,
    pub messages: Vec<MessageRow>,
}

/// Which profile served a round. The five names mirror the log's attempt
/// block one for one, so a reader never translates between two vocabularies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnProfile {
    pub profile_set: String,
    pub profile_id: String,
    pub provider: String,
    pub model: String,
    /// The profile's declared effort as its provider names it; absent when
    /// the profile declares none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// One round's token accounting. A counter the backend did not report reads
/// zero, and an absent block means no backend reported usage at all.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnUsageView {
    pub prompt: u64,
    pub completion: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

/// One tool call and what it returned.
///
/// The metadata here is what a list needs to triage: did it work, how big
/// was the output, which line failed. The text itself is a separate range
/// read — a page never carries tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRow {
    /// Position of the call within the round, from zero.
    pub call_index: u32,
    pub call_id: String,
    pub name: String,
    /// The arguments as the model sent them, shortened to
    /// [`TEXT_CUT_CHARS`].
    pub args: String,
    /// False when the call never produced a readable result.
    pub ok: bool,
    /// Present when the tool asked for approval; the call may then have no
    /// result in this round at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<bool>,
    /// Byte size of `result.stdout` in the record, spill paths included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_bytes: Option<u64>,
    /// Byte size of `result.stderr` in the record, the same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_bytes: Option<u64>,
    /// Lines in the merged stream, counted as `str::lines` counts them: a
    /// last line with no line end still counts as one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_lines: Option<u64>,
    /// Lines on stderr, counted the same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_lines: Option<u64>,
    /// The tool's exit status, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether the tool hit its own time bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
    /// Whether the tool's own output was cut before it was returned.
    pub truncated: bool,
    /// The first line of the error, shortened — the fact a list is scanned
    /// for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_error_line: Option<String>,
}

/// One message of a turn.
///
/// A page carries shape only (`chars`, flags) because a page of full text
/// would be unbounded; a turn detail carries the shortened text too.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    pub role: String,
    /// Characters in the message text as the record holds it: counted before
    /// any spill path is reclaimed and before any shortening.
    pub chars: usize,
    /// Whether the message carried reasoning content.
    pub has_reasoning: bool,
    /// Whether `text` was shortened to [`TEXT_CUT_CHARS`].
    pub truncated: bool,
    /// The shortened, cleaned text; absent on a page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// One turn, text included.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnDetail {
    pub schema_version: u32,
    pub turn: TurnRow,
}

/// A byte range of one call's text, as this face renders it.
///
/// The text is the tool-result record the log kept for that call, not the
/// bare stream the tool produced: `result.stdout` and its siblings sit
/// inside the record as the tool wrote them, and a reader that wants the
/// plain stream takes it out of the record itself.
///
/// Ranges walk the cleaned text: a spill path is reclaimed first, so every
/// offset addresses what a reader receives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallBody {
    pub schema_version: u32,
    pub turn_id: u64,
    pub call_index: u32,
    pub call_id: String,
    pub name: String,
    /// Byte offset this range starts at, in the cleaned text.
    pub offset: u64,
    /// Length of the whole cleaned text in bytes.
    pub total_bytes: u64,
    /// The range asked for, widened to the next character boundary when a
    /// character straddles the limit: at most three bytes over. An offset at or
    /// past the end of the text returns an empty range.
    pub text: String,
    /// True when this range stops before the end of the cleaned text.
    pub truncated: bool,
    /// Offset to ask for next; absent at the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
    /// Spill paths replaced in the whole cleaned text, not in this range:
    /// non-zero says the text pointed at a temporary file somewhere in it.
    pub reclaimed: u32,
}
