//! Read face over the history log: a page of turns, one turn, one call's
//! text.
//!
//! Everything here computes from the append-only NDJSON and writes nothing.
//! The log is the source of truth; these functions are a projection of it,
//! and the merge rules below are the ones the log's own repair flow assumes
//! (a corrected row is appended under the same turn id, never rewritten in
//! place).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use just_llm_client::types::generation::{ContentPart, Message, ToolCall};
use kallipai_common::protocol::{
    CALL_BODY_LIMIT_DEFAULT, CALL_BODY_MAX_BYTES, CallBody, CallBodyQuery, CallRow, MessageRow,
    SPILL_RECLAIMED, TEXT_CUT_CHARS, TURN_SCHEMA_VERSION, TURNS_PAGE_LIMIT_DEFAULT,
    TURNS_PAGE_LIMIT_MAX, TURNS_PAGE_MAX_BYTES, TurnDetail, TurnProfile, TurnRow, TurnUsageView,
    TurnsPage, TurnsQuery,
};
use kallipai_common::toolresult::ToolResultEnvelope;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{HistoryRecord, RecordKind, history_files};

/// How many day files a scan walks while looking for an older row of the
/// same turn that still carries the attempt block.
///
/// Offline repair appends its corrected row to the newest day file, so the
/// row holding the model-round facts can sit in an older one. The bound is
/// what keeps a turn that never had an attempt from costing a walk of the
/// whole log; past it the row simply reads as having no model round.
const ATTEMPT_LOOKBACK_FILES: usize = 14;

/// Room the page envelope itself needs (schema version, counters, cursor),
/// held back from the byte cap before rows are measured.
const PAGE_ENVELOPE_BYTES: usize = 256;

/// Characters that end a path token inside a stored banner.
const PATH_STOP: &str = " \t\r\n\"'`,;()[]{}<>";

/// The spill tree's family root inside a temp dir — `kallipai-shell`'s
/// `<temp>/kallipai/<uid>/…` — the marker a stored banner is recognised by.
const SPILL_MARKER: &str = "/kallipai/";

/// Why a read failed.
///
/// The first two are the caller's input, the last two are what the log does
/// not have — the split callers map to status codes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ViewError {
    /// `day` is not shaped `YYYY-MM-DD`.
    #[error("day must be shaped YYYY-MM-DD")]
    BadDay(String),
    /// `before` is not a cursor this face issued.
    #[error("unreadable cursor")]
    BadCursor(String),
    /// No readable record carries this turn id.
    #[error("turn {0} is not in the history log")]
    NoSuchTurn(u64),
    /// The turn carries fewer tool calls than the index asked for.
    #[error("turn {0} has no call {1}")]
    NoSuchCall(u64, u32),
}

/// One page of turns, newest first.
///
/// A page windows the merged log, not raw lines: a turn whose messages a
/// repair row replaced appears once, at the repair row's position. The
/// cursor therefore names a position (timestamp, turn id), and re-walking
/// the log above it is what keeps a repaired turn from turning up a second
/// time on a later page.
pub fn read_turns_page(agent_dir: &Path, query: &TurnsQuery) -> Result<TurnsPage, ViewError> {
    if let Some(day) = query.day.as_deref() {
        validate_day(day)?;
    }
    let cursor = match query.before.as_deref() {
        Some(raw) => Some(parse_cursor(raw)?),
        None => None,
    };
    let limit = query
        .limit
        .unwrap_or(TURNS_PAGE_LIMIT_DEFAULT)
        .clamp(1, TURNS_PAGE_LIMIT_MAX) as usize;

    let collected = collect_turns(agent_dir, query.day.as_deref(), cursor, limit);
    let records = collected.rows;

    let mut rows: Vec<PageRow> = records
        .iter()
        .map(|record| PageRow {
            at: record.datetime,
            turn_id: record.turn_id.unwrap_or_default(),
            row: project_record(record, false),
        })
        .collect();
    let mut sizes: Vec<usize> = rows
        .iter()
        .map(|row| serde_json::to_string(&row.row).map_or(0, |text| text.len()))
        .collect();
    let mut bytes: usize = sizes.iter().sum();
    let mut page_truncated = false;
    // The newest row stays whatever it weighs: dropping it too would hand
    // back a page whose cursor cannot move.
    while rows.len() > 1 && bytes + PAGE_ENVELOPE_BYTES > TURNS_PAGE_MAX_BYTES {
        bytes -= sizes.pop().unwrap_or_default();
        rows.pop();
        page_truncated = true;
    }
    let next_cursor = match rows.last() {
        Some(last) if page_truncated || !collected.exhausted => {
            Some(format_cursor(last.at, last.turn_id))
        }
        _ => None,
    };
    Ok(TurnsPage {
        schema_version: TURN_SCHEMA_VERSION,
        rows: rows.into_iter().map(|row| row.row).collect(),
        bad_lines: collected.bad_lines,
        next_cursor,
        page_truncated,
    })
}

/// A row with the position it occupies in the log.
struct PageRow {
    at: OffsetDateTime,
    turn_id: u64,
    row: TurnRow,
}

/// One turn, its message text included.
pub fn read_turn(agent_dir: &Path, turn_id: u64) -> Result<TurnDetail, ViewError> {
    let record = find_turn(agent_dir, turn_id).ok_or(ViewError::NoSuchTurn(turn_id))?;
    Ok(TurnDetail {
        schema_version: TURN_SCHEMA_VERSION,
        turn: project_record(&record, true),
    })
}

/// A byte range of one call's text.
///
/// The text is what the log holds for that call's result, never a file the
/// log points at: a stream that was spilled reads as reclaimed, and nothing
/// here opens the spill path.
pub fn read_call_body(
    agent_dir: &Path,
    turn_id: u64,
    call_index: u32,
    query: &CallBodyQuery,
) -> Result<CallBody, ViewError> {
    let record = find_turn(agent_dir, turn_id).ok_or(ViewError::NoSuchTurn(turn_id))?;
    let calls = calls_with_results(&record.messages);
    let (call, stored) = calls
        .get(call_index as usize)
        .ok_or(ViewError::NoSuchCall(turn_id, call_index))?;
    let (text, reclaimed) = scrub_spills(stored.unwrap_or_default());

    let total_bytes = text.len() as u64;
    let limit = query
        .limit
        .unwrap_or(CALL_BODY_LIMIT_DEFAULT)
        .clamp(1, CALL_BODY_MAX_BYTES);
    let mut start = query.offset.unwrap_or(0).min(total_bytes) as usize;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (start + limit).min(text.len());
    // A window narrower than one character still returns that character:
    // the end moves to the next boundary, at most three bytes past
    // `limit`, so `next_offset` always advances.
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let truncated = end < text.len();
    Ok(CallBody {
        schema_version: TURN_SCHEMA_VERSION,
        turn_id,
        call_index,
        call_id: call.id.clone(),
        name: call.name.clone(),
        offset: start as u64,
        total_bytes,
        text: text.get(start..end).unwrap_or_default().to_owned(),
        truncated,
        next_offset: truncated.then_some(end as u64),
        reclaimed,
    })
}

/// What one pass over the log produced.
struct Collected {
    /// Newest first, one record per turn id.
    rows: Vec<HistoryRecord>,
    /// Unreadable lines met while collecting (`System` records are not
    /// lines of this face and are not counted).
    bad_lines: u32,
    /// Whether the walk ended because the log did, not because the page filled.
    exhausted: bool,
}

/// Walk the log newest-first and take the newest `limit` turns positioned
/// older than `cursor`, folding in the attempt block of a turn whose newest
/// row lacks one.
///
/// One pass, one rule: the newest row of a turn supplies its messages and the
/// newest row carrying an attempt block supplies the model round. Once the page
/// is full the walk goes on only for that second half, and only for
/// [`ATTEMPT_LOOKBACK_FILES`] files from there: a turn that never had an
/// attempt must not cost a walk of the whole log.
///
/// The folding rule here and the one in `find_turn` are the same rule; a
/// change to one belongs in both.
fn collect_turns(
    agent_dir: &Path,
    day: Option<&str>,
    cursor: Option<(OffsetDateTime, u64)>,
    limit: usize,
) -> Collected {
    let mut collected = Collected {
        rows: Vec::new(),
        bad_lines: 0,
        exhausted: true,
    };
    // Turn ids whose newest record sits at or above the cursor: the reader
    // has already moved past them, so no older row of theirs may open a
    // second window. A repair row appended while a reader is paging lands
    // here too — its turn does not come back on a later page.
    let mut above: HashSet<u64> = HashSet::new();
    // Files walked since the page filled: the lookback budget for attempts.
    let mut files_past_page = 0usize;

    'files: for (index, path) in day_files(agent_dir, day).into_iter().enumerate().rev() {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        // A crash can cut a line mid-character; lossy decoding keeps the rest
        // of the day readable and turns the fragment into a bad line.
        let content = String::from_utf8_lossy(&bytes);
        for line in content.lines().rev() {
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<HistoryRecord>(line) else {
                // Damage inside the page window is the page's to report; past
                // it the walk reads only to fill an attempt block.
                if collected.rows.len() < limit {
                    collected.bad_lines += 1;
                }
                continue;
            };
            let Some((id, key)) = turn_key(&record) else {
                continue;
            };
            if let Some(taken) = collected
                .rows
                .iter()
                .position(|row| row.turn_id == Some(id))
            {
                // An older row of a turn already taken: all it can add is
                // the attempt block a repair row left out.
                if collected.rows[taken].attempt.is_none() {
                    collected.rows[taken].attempt = record.attempt;
                }
                continue;
            }
            if let Some(cursor) = cursor
                && key >= cursor
            {
                above.insert(id);
                continue;
            }
            if above.contains(&id) {
                continue;
            }
            if collected.rows.len() >= limit {
                // The page is full: older turns belong to a later page, so
                // this row is read only for an attempt block.
                collected.exhausted = false;
                continue;
            }
            collected.rows.push(record);
        }
        if collected.rows.len() >= limit {
            files_past_page += 1;
            let settled = collected.rows.iter().all(|row| row.attempt.is_some());
            if settled || files_past_page >= ATTEMPT_LOOKBACK_FILES {
                // Stopping short of the oldest file leaves turns the
                // page has not shown: the log's own end was not reached.
                if index > 0 {
                    collected.exhausted = false;
                }
                break 'files;
            }
        }
    }
    collected
}

/// The turn id and log position a record contributes, or `None` for a record
/// this face does not show (a system event carries no turn id).
fn turn_key(record: &HistoryRecord) -> Option<(u64, (OffsetDateTime, u64))> {
    if record.kind != RecordKind::Turn {
        return None;
    }
    let id = record.turn_id?;
    Some((id, (record.datetime, id)))
}

/// The day files a query walks, chronologically. A query pinned to one day
/// walks that file alone.
fn day_files(agent_dir: &Path, day: Option<&str>) -> Vec<PathBuf> {
    match day {
        Some(day) => vec![agent_dir.join("history").join(format!("{day}.ndjson"))],
        None => history_files(agent_dir),
    }
}

/// Whether the agent has a history log at all: the difference between an
/// empty page and a log that does not exist.
pub fn has_history(agent_dir: &Path) -> bool {
    super::has_history(agent_dir)
}

/// A day must be a real `YYYY-MM-DD` shape before it is spliced into a path.
fn validate_day(day: &str) -> Result<(), ViewError> {
    let bytes = day.as_bytes();
    let shaped = bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            _ => byte.is_ascii_digit(),
        });
    if shaped {
        Ok(())
    } else {
        Err(ViewError::BadDay(day.to_owned()))
    }
}

/// A cursor is `(unix nanoseconds, turn id)`: exact, and opaque to the
/// caller so the ordering key stays this module's to change.
fn parse_cursor(raw: &str) -> Result<(OffsetDateTime, u64), ViewError> {
    let bad = || ViewError::BadCursor(raw.to_owned());
    let (nanos, id) = raw.rsplit_once('|').ok_or_else(&bad)?;
    let nanos: i128 = nanos.parse().map_err(|_| bad())?;
    let at = OffsetDateTime::from_unix_timestamp_nanos(nanos).map_err(|_| bad())?;
    let id: u64 = id.parse().map_err(|_| bad())?;
    Ok((at, id))
}

fn format_cursor(at: OffsetDateTime, turn_id: u64) -> String {
    format!("{}|{turn_id}", at.unix_timestamp_nanos())
}

/// The newest record for one turn, with the newest attempt block any of its
/// rows carried folded in.
///
/// The walk stops once both halves are in hand, and anyway after
/// [`ATTEMPT_LOOKBACK_FILES`] files past the hit — a turn that never had an
/// attempt must not cost a whole-log walk.
///
/// The same folding rule `collect_turns` applies; a change to one belongs in
/// both.
fn find_turn(agent_dir: &Path, turn_id: u64) -> Option<HistoryRecord> {
    let mut found: Option<HistoryRecord> = None;
    let mut files_past_hit = 0usize;
    for path in history_files(agent_dir).into_iter().rev() {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let content = String::from_utf8_lossy(&bytes);
        for line in content.lines().rev() {
            let Ok(record) = serde_json::from_str::<HistoryRecord>(line) else {
                continue;
            };
            if record.kind != RecordKind::Turn || record.turn_id != Some(turn_id) {
                continue;
            }
            match found.as_mut() {
                None => found = Some(record),
                Some(newest) => {
                    if newest.attempt.is_none() {
                        newest.attempt = record.attempt;
                    }
                }
            }
            if found
                .as_ref()
                .is_some_and(|record| record.attempt.is_some())
            {
                return found;
            }
        }
        if found.is_some() {
            files_past_hit += 1;
            if files_past_hit >= ATTEMPT_LOOKBACK_FILES {
                break;
            }
        }
    }
    found
}

/// One record as the read face shows it.
///
/// The model-round facts are read off the attempt block and nothing else: a
/// record without one shows no model round at all, which is the honest
/// reading of a log that never recorded the round.
fn project_record(record: &HistoryRecord, with_text: bool) -> TurnRow {
    let attempt = record.attempt.as_ref();
    TurnRow {
        turn_id: record.turn_id.unwrap_or_default(),
        at: rfc3339(record.datetime),
        profile: attempt.map(|attempt| TurnProfile {
            profile_set: attempt.profile_set.clone(),
            profile_id: attempt.profile_id.clone(),
            provider: attempt.provider.clone(),
            model: attempt.model.clone(),
            effort: attempt.effort.clone(),
        }),
        usage: attempt
            .and_then(|attempt| attempt.usage)
            .map(|usage| TurnUsageView {
                prompt: usage.prompt.into(),
                completion: usage.completion.into(),
                cache_read: usage.cache_read.into(),
                cache_write: usage.cache_write.into(),
                reasoning: usage.reasoning.into(),
            }),
        elapsed_ms: attempt.map(|attempt| attempt.elapsed_ms),
        calls: project_calls(&record.messages),
        messages: project_messages(&record.messages, with_text),
    }
}

/// The tool calls of a record's messages, in the order the model sent them,
/// each paired with the result text stored for it (if any).
fn calls_with_results(messages: &[Message]) -> Vec<(&ToolCall, Option<&str>)> {
    let mut results: HashMap<&str, &str> = HashMap::new();
    for message in messages {
        if let Some(id) = message.tool_call_id() {
            // A repeated id keeps the last result: the log appends, so the
            // last answer is the newest one for that call.
            results.insert(id, message.content().unwrap_or_default());
        }
    }
    let mut calls = Vec::new();
    for message in messages {
        for call in message.tool_calls() {
            calls.push((call, results.get(call.id.as_str()).copied()));
        }
    }
    calls
}

fn project_calls(messages: &[Message]) -> Vec<CallRow> {
    calls_with_results(messages)
        .into_iter()
        .enumerate()
        .map(|(index, (call, result))| project_call(index as u32, call, result))
        .collect()
}

/// One call's row: what the model sent, plus the facts the result envelope
/// carries.
///
/// A call with no result row, or one whose text is not an envelope (a tool
/// answering in another shape), reads as not ok: this face never reports a
/// success it cannot see in the log.
fn project_call(call_index: u32, call: &ToolCall, result: Option<&str>) -> CallRow {
    let mut row = CallRow {
        call_index,
        call_id: call.id.clone(),
        name: call.name.clone(),
        args: shorten(&scrub_spills(&call.arguments).0),
        ok: false,
        pending_approval: None,
        stdout_bytes: None,
        stderr_bytes: None,
        stdout_lines: None,
        stderr_lines: None,
        exit_code: None,
        timed_out: None,
        truncated: false,
        first_error_line: None,
    };
    let Some(envelope) =
        result.and_then(|text| serde_json::from_str::<ToolResultEnvelope>(text).ok())
    else {
        return row;
    };
    row.ok = envelope.ok;
    row.pending_approval = envelope.pending_approval;
    row.first_error_line = envelope
        .error
        .as_deref()
        .and_then(|error| error.lines().next())
        .map(|line| shorten(&scrub_spills(line).0));
    if let Some(result) = envelope.result.as_ref() {
        // bash_exec's merged stream fills the stdout column: it is the one
        // stream the model saw.
        // Sizes and line counts measure the record's own text, before any
        // spill path is reclaimed or the text is shortened.
        let stdout = stream(result, "stdout").or_else(|| stream(result, "output"));
        let stderr = stream(result, "stderr");
        if let Some(text) = stdout {
            row.stdout_bytes = Some(text.len() as u64);
            row.stdout_lines = Some(count_lines(text));
        }
        if let Some(text) = stderr {
            row.stderr_bytes = Some(text.len() as u64);
            row.stderr_lines = Some(count_lines(text));
        }
        row.exit_code = result
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok());
        row.timed_out = result.get("timed_out").and_then(Value::as_bool);
        row.truncated = result
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or_default();
    }
    row
}

/// One stream of a structured tool result, as the record holds it.
fn stream<'a>(result: &'a Value, field: &str) -> Option<&'a str> {
    result.get(field).and_then(Value::as_str)
}

/// Lines as the record holds them: `str::lines` splits on line ends, so a
/// last line without one still counts as a line.
fn count_lines(text: &str) -> u64 {
    text.lines().count() as u64
}

fn project_messages(messages: &[Message], with_text: bool) -> Vec<MessageRow> {
    messages
        .iter()
        .map(|message| {
            let raw = message_text(message);
            let chars = raw.chars().count();
            let (text, truncated) = if with_text {
                let (cut, truncated) = cut_text(&scrub_spills(&raw).0);
                (Some(cut), truncated)
            } else {
                (None, false)
            };
            MessageRow {
                role: message.role().to_owned(),
                chars,
                has_reasoning: message.reasoning().is_some(),
                truncated,
                text,
            }
        })
        .collect()
}

/// A message's own text as the record holds it: its content, or the text
/// parts of a multimodal body (an image part carries no text).
fn message_text(message: &Message) -> String {
    if let Some(text) = message.content() {
        return text.to_owned();
    }
    let mut joined = String::new();
    for part in message.content_parts().unwrap_or_default() {
        if let ContentPart::Text { text } = part {
            if !joined.is_empty() {
                joined.push('\n');
            }
            joined.push_str(text);
        }
    }
    joined
}

/// Shorten a text to [`TEXT_CUT_CHARS`] characters, keeping both ends: the
/// identifying facts of an argument blob or a tool result sit at the edges.
fn cut_text(text: &str) -> (String, bool) {
    let total = text.chars().count();
    if total <= TEXT_CUT_CHARS {
        return (text.to_owned(), false);
    }
    let half = TEXT_CUT_CHARS / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(total - half).collect();
    (format!("{head} … {tail}"), true)
}

fn shorten(text: &str) -> String {
    cut_text(text).0
}

/// Replace every path into the spill tree with [`SPILL_RECLAIMED`],
/// counting the replacements.
///
/// A spilled stream or message left a banner naming a temporary file under
/// `<temp>/kallipai/<uid>/…` (`kallipai-shell` owns that layout). The file
/// is temporary and the read face must never send a reader after it, so the
/// path is replaced where the text is rendered — and nowhere else.
fn scrub_spills(text: &str) -> (String, u32) {
    let mut out = String::with_capacity(text.len());
    let mut at = 0usize;
    let mut reclaimed = 0u32;
    while let Some((start, end)) = next_spill_span(text, at) {
        out.push_str(&text[at..start]);
        out.push_str(SPILL_RECLAIMED);
        reclaimed += 1;
        at = end;
    }
    out.push_str(&text[at..]);
    (out, reclaimed)
}

/// The next path token wrapping a `/kallipai/<uid>/` marker at or after
/// `from`, as a byte span covering the whole token.
fn next_spill_span(text: &str, from: usize) -> Option<(usize, usize)> {
    let mut search = from;
    while let Some(found) = text[search..].find(SPILL_MARKER) {
        let head = search + found;
        let after_marker = head + SPILL_MARKER.len();
        let rest = &text[after_marker..];
        let uid = rest.bytes().take_while(u8::is_ascii_digit).count();
        if uid > 0 && rest.as_bytes().get(uid) == Some(&b'/') {
            // `PATH_STOP` holds single-byte characters, so one byte back
            // lands past the separator.
            let start = text[..head]
                .rfind(|c: char| PATH_STOP.contains(c))
                .map_or(0, |index| index + 1);
            let end = rest
                .find(|c: char| PATH_STOP.contains(c))
                .map_or(text.len(), |index| after_marker + index);
            return Some((start, end));
        }
        search = after_marker;
    }
    None
}

/// A log timestamp in the form the log writes it.
fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

#[cfg(test)]
mod tests;
