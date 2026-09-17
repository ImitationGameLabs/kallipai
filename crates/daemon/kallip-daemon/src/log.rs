//! The log verb: tail an instance's log files as a read-only
//! diagnostic. It reads the record (for authorization) and the log
//! directory, and writes nothing — not the record area, not the data
//! directory, not the log tree. A stopped instance reads exactly like
//! a running one: the logs are output residue on disk.
//!
//! Log placement is owner-aware: the tree lives in the instance's
//! TARGET USER's state home (`<home>/.local/state/kallipai/tagmata/<slug>/logs`),
//! resolved from the record's `target_uid` through the passwd database
//! (`reconcile::instance_logs_dir`) — the daemon may run as root while
//! instances run as their own users, so the daemon's own state home
//! would name the wrong tree. The shape is mirrored by the tagma's
//! `logs_target` in kallip-tagma; the shapes move together by hand.
//! The files are
//! tracing-appender daily rolls (`instance.<date>.log`, seven
//! retained), so the file name sorts in date order and the merged
//! tail concatenates oldest → newest; cross-day crash context is the
//! point of merging rather than reading only the newest file.
//!
//! The response is one line of JSON, so the text payload carries a
//! byte budget measured *after* serialization: dense quotes and
//! control bytes — exactly what crash dumps are full of — inflate
//! several times in JSON escaping, and a raw-byte budget would burst
//! the wire line cap on the dirtiest logs, which are the ones being
//! diagnosed.
use std::cell::Cell;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use kallip_daemon_common::wire::{ErrorCode, LogCursor, valid_slug};

/// The payload budget in serialized bytes: the text field's
/// JSON-escaped length, not its raw length. Leaves envelope headroom
/// under `MAX_LINE_BYTES` (the 64 KiB wire line cap).
const LOG_BUDGET_BYTES: usize = 48 * 1024;
/// Raw-bytes cap for the in-memory line assembly. More than the kept
/// tail of any budget-conforming line can ever need, and one raw
/// byte more than the budget — so a line the reader had to cut
/// always exceeds the budget in the builder's escaped measure and
/// gets its truncation marker, instead of sliding in unmarked.
const RAW_LINE_KEEP: usize = LOG_BUDGET_BYTES + 1;
/// Sized on the same order as `MAX_LINE_BYTES`: a budget-conforming
/// line fits in one chunk, so taking a tail costs a handful of
/// reads instead of slurping the whole file into memory.
const READ_CHUNK: usize = 64 * 1024;

/// Request intent when the caller omits `--lines` (the daemon clamps
/// its copy too — kallipctl mirrors this defensively).
pub const DEFAULT_LINES: u32 = 20;
pub const MAX_LINES: u32 = 1000;

/// Prepended to the kept tail of a line that alone exceeds the
/// budget: the head is gone, and the reader should know it.
const TRUNCATION_MARKER: &str = "…truncated…";

#[derive(Debug)]
pub struct LogOutcome {
    pub text: String,
    pub next_cursor: Option<LogCursor>,
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("no instance named {0}")]
    NotFound(String),
    /// The record names a target uid the passwd database does not
    /// know, so the log directory cannot be placed. A hard error,
    /// not a fallback: the daemon's own state home would read
    /// another user's tree.
    #[error("uid {0} has no passwd entry; cannot place the log directory")]
    LogsHome(u32),
}

impl From<&LogError> for ErrorCode {
    fn from(error: &LogError) -> Self {
        match error {
            LogError::NotFound(_) => ErrorCode::NotFound,
            // Not a client mistake and not recoverable here: the
            // system's passwd database lacks the record's target uid.
            LogError::LogsHome(_) => ErrorCode::Internal,
        }
    }
}

/// Tail the instance's logs. `lines` is the request intent (clamped,
/// and further reduced whenever the budget says so); `file` picks one
/// file from the log directory instead of the merged tail; `cursor`
/// continues a previous pull (the follow contract). Authorization is
/// the stop verb's discipline verbatim: owner or root, and a foreign
/// peer gets the same NotFound as a missing slug so the verb cannot
/// probe slug existence.
pub fn tail(
    record_root: &Path,
    logs_dir: &Path,
    slug: &str,
    lines: Option<u32>,
    file: Option<&str>,
    cursor: Option<&LogCursor>,
    peer_uid: u32,
) -> Result<LogOutcome, LogError> {
    // Same grammar gate as stop/start: an invalid slug is not an
    // instance name, so it cannot exist — NotFound without a
    // record-area read (the slug must not become a path probe).
    if !valid_slug(slug) {
        return Err(LogError::NotFound(slug.to_string()));
    }
    let Some(record) = crate::records::read_record(record_root, slug) else {
        return Err(LogError::NotFound(slug.to_string()));
    };
    if !crate::spawn::authorized(peer_uid, record.target_uid) {
        tracing::warn!(
            slug = %slug,
            peer_uid,
            target_uid = record.target_uid,
            "log denied: foreign peer"
        );
        return Err(LogError::NotFound(slug.to_string()));
    }

    let max_lines = lines.unwrap_or(DEFAULT_LINES).min(MAX_LINES);
    let (text, next_cursor) = match file {
        Some(name) => match single_file(logs_dir, name) {
            None => (String::new(), None),
            Some(one) => {
                let files = vec![one];
                match cursor {
                    Some(cur) if cur.file == files[0].name => continue_from(&files, cur, max_lines),
                    _ => fresh_tail(&files, max_lines),
                }
            }
        },
        None => {
            let files = list_log_files(logs_dir);
            match cursor {
                Some(cur) => continue_from(&files, cur, max_lines),
                None => fresh_tail(&files, max_lines),
            }
        }
    };
    Ok(LogOutcome { text, next_cursor })
}

struct LogFile {
    name: String,
    path: PathBuf,
    len: u64,
}

/// The retained log files of an instance, sorted by name — which for
/// daily-rolled `instance.<date>.log` files is date order, oldest
/// first. A missing or unreadable directory is an empty list: the
/// verb answers an empty tail rather than an error, since the
/// instance plainly exists.
fn list_log_files(logs_dir: &Path) -> Vec<LogFile> {
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return Vec::new();
    };
    let mut files: Vec<LogFile> = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !(name.starts_with("instance") && name.ends_with(".log")) {
                return None;
            }
            let len = entry.metadata().ok()?.len();
            Some(LogFile {
                name,
                path: entry.path(),
                len,
            })
        })
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files
}

/// One explicitly selected file (`--file`). Anything that is not a
/// bare file name (separators, `..`) is refused rather than joined,
/// so the flag cannot point outside the log directory.
fn single_file(logs_dir: &Path, name: &str) -> Option<LogFile> {
    if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
        return None;
    }
    let path = logs_dir.join(name);
    let len = std::fs::metadata(&path).ok()?.len();
    Some(LogFile {
        name: name.to_string(),
        path,
        len,
    })
}

/// What the tail accumulator asks the reader to do after a line.
enum Step {
    /// Line kept; keep reading.
    Keep,
    /// Line not kept (budget or line cap reached); reading stops and
    /// the cursor stays at this line's start, so a follow poll
    /// re-fetches it whole.
    StopBefore,
    /// Line kept head-truncated (it alone exceeds the budget);
    /// reading stops with the cursor past it.
    StopAfter,
}

struct TailBuilder {
    max_lines: u32,
    used: usize,
    parts: Vec<String>, // newest first; reversed for the payload
}

impl TailBuilder {
    fn new(max_lines: u32) -> Self {
        TailBuilder {
            max_lines,
            used: 0,
            parts: Vec::new(),
        }
    }

    fn feed(&mut self, line: &str) -> Step {
        if self.parts.len() as u32 >= self.max_lines {
            return Step::StopBefore;
        }
        // The joining newline is part of the payload and escapes to
        // two bytes.
        let cost = escaped_len(line) + if self.parts.is_empty() { 0 } else { 2 };
        if self.used + cost <= LOG_BUDGET_BYTES {
            self.used += cost;
            self.parts.push(line.to_string());
            return Step::Keep;
        }
        // A line that alone exceeds the budget is presented as its
        // tail with the truncation marker — a diagnostic prefers a
        // marked half-line over an error. Only the first line can
        // take this path; later ones are dropped whole and re-fetched
        // (continuation) or simply absent (fresh tail).
        if self.parts.is_empty() && escaped_len(line) > LOG_BUDGET_BYTES {
            self.parts.push(truncate_head(line));
            return Step::StopAfter;
        }
        Step::StopBefore
    }

    fn text(&self) -> String {
        let mut lines = self.parts.clone();
        lines.reverse();
        lines.join("\n")
    }
}

/// Keep the tail of `line` such that marker + kept fits the budget.
fn truncate_head(line: &str) -> String {
    let marker_cost = escaped_len(TRUNCATION_MARKER);
    if LOG_BUDGET_BYTES <= marker_cost {
        return String::new();
    }
    let allowed = LOG_BUDGET_BYTES - marker_cost;
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0usize;
    for ch in line.chars().rev() {
        let cost = escaped_char_len(ch);
        if used + cost > allowed {
            break;
        }
        used += cost;
        kept.push(ch);
    }
    kept.reverse();
    let mut out = String::from(TRUNCATION_MARKER);
    out.extend(kept);
    out
}

/// The JSON-escaped byte length of `text` as serde_json emits it
/// inside a string: two bytes for the short escapes, six for the long
/// control forms, raw UTF-8 otherwise. The cross-check test pins this
/// against serde_json itself.
fn escaped_len(text: &str) -> usize {
    text.chars().map(escaped_char_len).sum()
}

fn escaped_char_len(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        ch if (ch as u32) < 0x20 => 6,
        ch => ch.len_utf8(),
    }
}

/// View of `raw` capped at `RAW_LINE_KEEP` from the end: a runaway
/// line is handed over as its tail, whose head is beyond any budget
/// that could keep it anyway.
fn tail_keep(raw: &[u8]) -> &[u8] {
    if raw.len() > RAW_LINE_KEEP {
        &raw[raw.len() - RAW_LINE_KEEP..]
    } else {
        raw
    }
}

/// Feed whole lines from the end of the file, newest first, until
/// `feed` says stop. Chunked backward reads: the files are append
/// only, so the tail is all a diagnostic needs, and a runaway day
/// must not be read whole to reach it.
fn for_each_line_newest_first(
    path: &Path,
    feed: &mut dyn FnMut(&str) -> bool,
) -> std::io::Result<()> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut pos = len;
    // The still-unterminated oldest line seen so far (its newer
    // part); capped at `RAW_LINE_KEEP` from the front.
    let mut pending: Vec<u8> = Vec::new();
    while pos > 0 {
        let start = pos.saturating_sub(READ_CHUNK as u64);
        let take = (pos - start) as usize;
        let mut buf = vec![0u8; take];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf)?;
        pos = start;
        // Older chunk bytes first, then the pending newer fragment:
        // every complete line splits off the end, newest first, and
        // whatever precedes the oldest newline stays pending.
        buf.extend_from_slice(&pending);
        pending = match buf.iter().rposition(|&b| b == b'\n') {
            Some(idx) => {
                // The bytes after the oldest newline in the chunk are
                // the newest line — including whatever fragment a
                // previous, newer chunk had already put aside (`buf`
                // carries it in front). Feed it before the interior
                // lines, newest first.
                let newest = &buf[idx + 1..];
                if !newest.is_empty() && !feed(&String::from_utf8_lossy(newest)) {
                    return Ok(());
                }
                let mut rest = &buf[..idx];
                while let Some(split) = rest.iter().rposition(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(&rest[split + 1..]).into_owned();
                    if !feed(&line) {
                        return Ok(());
                    }
                    rest = &rest[..split];
                }
                rest.to_vec()
            }
            None => buf,
        };
        if pending.len() > RAW_LINE_KEEP {
            let keep_from = pending.len() - RAW_LINE_KEEP;
            pending.drain(..keep_from);
        }
    }
    if !pending.is_empty() {
        // Beginning of the file: the first line has no leading
        // newline, so whatever is still pending is it.
        let line = String::from_utf8_lossy(&pending);
        feed(&line);
    }
    Ok(())
}

/// What a forward line feed tells the reader.
enum Forward {
    Continue,
    StopAt(u64),
}

/// Feed whole lines of `path` from byte `start` forward, in order,
/// until the feed stops or the file ends. Returns the byte offset
/// where reading should resume: the start of the line the feed
/// refused, or the end of file. An unterminated stub at the end of
/// file is fed and the offset resumes after it — if the writer
/// completes the line later, the next poll slices its tail from
/// there rather than replaying the stub.
fn for_each_line_forward(
    path: &Path,
    start: u64,
    feed: &mut dyn FnMut(&str, u64, usize) -> Forward,
) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if start >= len {
        return Ok(len);
    }
    file.seek(SeekFrom::Start(start))?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = [0u8; READ_CHUNK];
    let mut consumed = start;
    let mut pending: Vec<u8> = Vec::new(); // the current line's bytes so far
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            if pending.is_empty() {
                return Ok(consumed);
            }
            let line_start = consumed - pending.len() as u64;
            let line = String::from_utf8_lossy(tail_keep(&pending)).into_owned();
            return match feed(&line, line_start, pending.len()) {
                Forward::Continue => Ok(consumed),
                Forward::StopAt(pos) => Ok(pos),
            };
        }
        let mut rest = &buf[..n];
        while let Some(idx) = rest.iter().position(|&b| b == b'\n') {
            let (head, tail) = rest.split_at(idx);
            pending.extend_from_slice(head);
            let line_start = consumed - pending.len() as u64;
            let line = String::from_utf8_lossy(tail_keep(&pending)).into_owned();
            match feed(&line, line_start, pending.len()) {
                Forward::Continue => {}
                Forward::StopAt(pos) => return Ok(pos),
            }
            consumed += pending.len() as u64 + 1; // the line plus its newline
            pending.clear();
            rest = &tail[1..];
        }
        pending.extend_from_slice(rest);
        consumed += n as u64;
        if pending.len() > RAW_LINE_KEEP {
            let keep_from = pending.len() - RAW_LINE_KEEP;
            pending.drain(..keep_from);
        }
    }
}

/// The merged tail: all retained files concatenated oldest → newest,
/// accumulated from the end within the budget. The cursor lands on
/// the newest file's end — the tail always reaches the last byte,
/// whatever the old side dropped.
fn fresh_tail(files: &[LogFile], max_lines: u32) -> (String, Option<LogCursor>) {
    let Some(newest) = files.last() else {
        return (String::new(), None);
    };
    let mut builder = TailBuilder::new(max_lines);
    let stopped = Cell::new(false);
    for file in files.iter().rev() {
        stopped.set(false);
        let mut feed = |line: &str| match builder.feed(line) {
            Step::Keep => true,
            Step::StopBefore | Step::StopAfter => {
                stopped.set(true);
                false
            }
        };
        if let Err(error) = for_each_line_newest_first(&file.path, &mut feed) {
            // One unreadable file must not blank the rest of the
            // tail: keep what was read, move on to the older files.
            tracing::debug!(file = %file.name, %error, "log tail read failed");
            continue;
        }
        if stopped.get() {
            break;
        }
    }
    let cursor = LogCursor {
        file: newest.name.clone(),
        byte: newest.len,
    };
    (builder.text(), Some(cursor))
}

/// Continue a follow pull from `cursor`: the cursor file from its
/// offset, then every newer file whole, budget-bounded, cutting at
/// line boundaries so the next pull re-fetches an unfitted line
/// whole. A cursor whose file has rotated off (or a name that never
/// existed) resets to a fresh tail — the client sees the cursor file
/// name change and re-baselines with a warning (the follow
/// contract's rotation shape). A clamped offset (the file shrank)
/// comes back regressed, which the client reads the same way.
fn continue_from(
    files: &[LogFile],
    cursor: &LogCursor,
    max_lines: u32,
) -> (String, Option<LogCursor>) {
    let Some(idx) = files.iter().position(|f| f.name == cursor.file) else {
        return fresh_tail(files, max_lines);
    };
    let mut builder = TailBuilder::new(max_lines);
    let stopped = Cell::new(false);
    let mut feed = |line: &str, line_start: u64, raw_len: usize| match builder.feed(line) {
        Step::Keep => Forward::Continue,
        Step::StopBefore => {
            stopped.set(true);
            Forward::StopAt(line_start)
        }
        Step::StopAfter => {
            stopped.set(true);
            Forward::StopAt(line_start + raw_len as u64 + 1)
        }
    };
    let mut resume: Option<LogCursor> = None;
    for (i, file) in files.iter().enumerate().skip(idx) {
        let start = if i == idx {
            cursor.byte.min(file.len)
        } else {
            0
        };
        stopped.set(false);
        match for_each_line_forward(&file.path, start, &mut feed) {
            Ok(offset) => {
                if stopped.get() {
                    resume = Some(LogCursor {
                        file: file.name.clone(),
                        byte: offset,
                    });
                    break;
                }
            }
            Err(error) => {
                tracing::debug!(file = %file.name, %error, "log read failed");
            }
        }
    }
    let next = resume.unwrap_or_else(|| {
        let newest = files.last().expect("cursor file matched, list non-empty");
        LogCursor {
            file: newest.name.clone(),
            byte: newest.len,
        }
    });
    (builder.text(), Some(next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_daemon_common::wire::MAX_LINE_BYTES;
    use std::fs;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, text).expect("write");
    }

    fn put_record(root: &Path, slug: &str, owner: u32) {
        let record = crate::records::InstanceRecord {
            instance_id: format!("inst-{slug}"),
            owner_uid: owner,
            target_uid: owner,
            target_username: None,
            workspace: None,
            env: Vec::new(),
            identity: None,
            data_dir: root.join("data"),
        };
        crate::records::create_record(root, slug, &record).expect("record");
    }

    fn roll(root: &Path, name: &str, text: &str) {
        write(&root.join("logs").join(name), text);
    }

    fn peek(out: &LogOutcome) -> (&str, Option<(&str, u64)>) {
        (
            out.text.as_str(),
            out.next_cursor.as_ref().map(|c| (c.file.as_str(), c.byte)),
        )
    }

    #[test]
    fn authorization_is_owner_or_root_and_foreign_reads_not_found() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            None,
            None,
            None,
            1000,
        )
        .expect("owner reads");
        assert_eq!(out.text, "");
        tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            None,
            None,
            None,
            0,
        )
        .expect("root reads");
        let error = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            None,
            None,
            None,
            2000,
        )
        .unwrap_err();
        assert!(matches!(error, LogError::NotFound(_)));
    }

    #[test]
    fn invalid_slug_is_not_found_without_probing() {
        let root = tempdir();
        let error = tail(
            root.path(),
            &root.path().join("logs"),
            "../escape",
            None,
            None,
            None,
            1000,
        )
        .unwrap_err();
        assert!(matches!(error, LogError::NotFound(_)));
    }

    #[test]
    fn missing_log_dir_is_an_empty_tail_without_a_cursor() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            None,
            None,
            None,
            1000,
        )
        .expect("tail");
        assert_eq!(peek(&out), ("", None));
    }

    #[test]
    fn merged_tail_concatenates_oldest_to_newest() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        roll(root.path(), "instance.2026-09-08.log", "d8-a\nd8-b\n");
        roll(root.path(), "instance.2026-09-09.log", "d9-a\nd9-b\n");
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            None,
            None,
            1000,
        )
        .expect("tail");
        assert_eq!(
            peek(&out),
            (
                "d8-a\nd8-b\nd9-a\nd9-b",
                Some(("instance.2026-09-09.log", 10))
            )
        );
    }

    #[test]
    fn lines_intent_takes_the_last_lines_across_the_merge() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        roll(root.path(), "instance.2026-09-08.log", "d8-a\nd8-b\n");
        roll(root.path(), "instance.2026-09-09.log", "d9-a\nd9-b\n");
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(2),
            None,
            None,
            1000,
        )
        .expect("tail");
        assert_eq!(
            peek(&out),
            ("d9-a\nd9-b", Some(("instance.2026-09-09.log", 10)))
        );
    }

    #[test]
    fn lines_defaults_to_twenty() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        let body: String = (0..25).map(|i| format!("line-{i:02}\n")).collect();
        roll(root.path(), "instance.2026-09-09.log", &body);
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            None,
            None,
            None,
            1000,
        )
        .expect("tail");
        assert_eq!(out.text.lines().count(), 20);
        assert!(out.text.starts_with("line-05"));
        assert!(out.text.ends_with("line-24"));
    }

    #[test]
    fn lines_clamps_at_the_cap() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        let body: String = (0..1001).map(|i| format!("line-{i:04}\n")).collect();
        roll(root.path(), "instance.2026-09-09.log", &body);
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(5000),
            None,
            None,
            1000,
        )
        .expect("tail");
        assert_eq!(out.text.lines().count(), 1000);
    }

    #[test]
    fn budget_trims_the_oldest_lines() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        let line = format!("{}\n", "x".repeat(1000));
        let mut body = String::new();
        for i in 0..60 {
            body.push_str(&format!("mark-{i:03}-"));
            body.push_str(&line);
        }
        roll(root.path(), "instance.2026-09-09.log", &body);
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(1000),
            None,
            None,
            1000,
        )
        .expect("tail");
        assert!(escaped_len(&out.text) <= LOG_BUDGET_BYTES);
        assert!(out.text.starts_with("mark-"), "kept lines stay whole");
        assert!(out.text.contains("mark-059-"), "the newest line survives");
        assert!(!out.text.contains("mark-000-"), "the oldest lines drop");
        // 48 lines of 1011 escaped bytes (first 1009) is the largest
        // prefix that fits 48 KiB; the 49th would push it over.
        assert_eq!(out.text.lines().count(), 48);
    }

    #[test]
    fn single_line_over_budget_is_head_truncated() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        let giant = format!("HEAD-MARK-{}TAIL-END\n", "y".repeat(60 * 1024));
        roll(root.path(), "instance.2026-09-09.log", &giant);
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            None,
            None,
            1000,
        )
        .expect("tail");
        assert!(out.text.starts_with(TRUNCATION_MARKER));
        assert!(escaped_len(&out.text) <= LOG_BUDGET_BYTES);
        assert!(
            out.text.ends_with("TAIL-END"),
            "the tail of the line survives"
        );
    }

    #[test]
    fn budget_is_accounted_after_serialization() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        // Dense quotes: ~30 KiB raw per line, ~61 KiB escaped. A raw
        // accounting would keep all three and burst the wire cap.
        let mut body = String::new();
        for _ in 0..3 {
            body.push('"');
            body.push_str(&"\"".repeat(30 * 1024));
            body.push('\n');
        }
        roll(root.path(), "instance.2026-09-09.log", &body);
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(1000),
            None,
            None,
            1000,
        )
        .expect("tail");
        assert!(escaped_len(&out.text) <= LOG_BUDGET_BYTES);
        let payload = kallip_daemon_common::wire::OkPayload::Log {
            text: out.text.clone(),
            next_cursor: out.next_cursor.clone(),
        };
        let line =
            kallip_daemon_common::wire::encode_response(&kallip_daemon_common::wire::ok(payload))
                .expect("encode");
        assert!(line.len() < MAX_LINE_BYTES);
    }

    #[test]
    fn escaped_len_matches_serde_json() {
        let samples = [
            "plain",
            "quote\"inside",
            "back\\slash",
            "ctrl\u{1}\u{7f}\u{8}\u{c}",
            "newline\nand\ttab",
            "unicode-é—…",
            "",
        ];
        for text in samples {
            assert_eq!(
                escaped_len(text),
                serde_json::to_string(text).unwrap().len() - 2,
                "escape measure drifted from serde_json for {text:?}"
            );
        }
    }

    #[test]
    fn cursor_pulls_only_lines_after_it() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        roll(root.path(), "instance.2026-09-09.log", "l1\nl2\n");
        let first = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            None,
            None,
            1000,
        )
        .expect("fresh tail");
        assert_eq!(
            peek(&first),
            ("l1\nl2", Some(("instance.2026-09-09.log", 6)))
        );
        // The writer appends; the follow poll must see exactly the new
        // line and nothing replayed.
        write(
            &root.path().join("logs").join("instance.2026-09-09.log"),
            "l1\nl2\nl3\n",
        );
        let cursor = first.next_cursor.expect("cursor");
        let next = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            None,
            Some(&cursor),
            1000,
        )
        .expect("continuation");
        assert_eq!(peek(&next), ("l3", Some(("instance.2026-09-09.log", 9))));
    }

    #[test]
    fn cursor_beyond_the_file_length_comes_back_clamped() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        roll(root.path(), "instance.2026-09-09.log", "l1\nl2\n");
        // byte 100 > len 6: the daemon clamps the resume point to the
        // real length; the regressed offset is the client's signal to
        // reset (the follow contract's shrink shape).
        let cursor = LogCursor {
            file: "instance.2026-09-09.log".into(),
            byte: 100,
        };
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            None,
            Some(&cursor),
            1000,
        )
        .expect("clamped continuation");
        assert_eq!(peek(&out), ("", Some(("instance.2026-09-09.log", 6))));
    }

    #[test]
    fn rotated_off_cursor_file_resets_to_a_fresh_tail() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        roll(root.path(), "instance.2026-09-08.log", "d8\n");
        roll(root.path(), "instance.2026-09-09.log", "d9\n");
        // The retention policy dropped yesterday's file the cursor
        // points at: same path as any reset — fresh tail, no error;
        // the client spots the file name change.
        let cursor = LogCursor {
            file: "instance.2026-09-07.log".into(),
            byte: 3,
        };
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            None,
            Some(&cursor),
            1000,
        )
        .expect("reset continuation");
        assert_eq!(peek(&out), ("d8\nd9", Some(("instance.2026-09-09.log", 3))));
    }

    #[test]
    fn single_file_selection_skips_the_merge() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        roll(root.path(), "instance.2026-09-08.log", "d8-a\nd8-b\n");
        roll(root.path(), "instance.2026-09-09.log", "d9-a\n");
        let out = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            Some("instance.2026-09-08.log"),
            None,
            1000,
        )
        .expect("single file");
        assert_eq!(
            peek(&out),
            ("d8-a\nd8-b", Some(("instance.2026-09-08.log", 10)))
        );
        // A name that is not there: empty, nothing to follow.
        let missing = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            Some("instance.2026-09-01.log"),
            None,
            1000,
        )
        .expect("missing file");
        assert_eq!(peek(&missing), ("", None));
        // A name that is not a bare file name: refused as empty —
        // the flag must not become a path escape.
        let escape = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            Some("../secrets"),
            None,
            1000,
        )
        .expect("escape refused");
        assert_eq!(peek(&escape), ("", None));
    }

    #[test]
    fn lines_intent_and_cursor_coexist_on_the_first_screen() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        roll(root.path(), "instance.2026-09-09.log", "a\nb\nc\nd\n");
        // First screen: last two lines, cursor still at the end so
        // the follow picks up only what comes next.
        let first = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(2),
            None,
            None,
            1000,
        )
        .expect("first screen");
        assert_eq!(peek(&first), ("c\nd", Some(("instance.2026-09-09.log", 8))));
        write(
            &root.path().join("logs").join("instance.2026-09-09.log"),
            "a\nb\nc\nd\ne\n",
        );
        let cursor = first.next_cursor.expect("cursor");
        let next = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(2),
            None,
            Some(&cursor),
            1000,
        )
        .expect("continuation");
        assert_eq!(peek(&next), ("e", Some(("instance.2026-09-09.log", 10))));
    }

    #[test]
    fn unterminated_last_line_is_served_and_resumes_after_it() {
        let root = tempdir();
        put_record(root.path(), "team-a", 1000);
        roll(root.path(), "instance.2026-09-09.log", "whole\npartia");
        let first = tail(
            root.path(),
            &root.path().join("logs"),
            "team-a",
            Some(100),
            None,
            None,
            1000,
        )
        .expect("fresh tail");
        assert_eq!(first.text, "whole\npartia");
        assert_eq!(
            peek(&first),
            ("whole\npartia", Some(("instance.2026-09-09.log", 12)))
        );
    }
}
