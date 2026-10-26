//! The read face against hand-written logs: paging, repair rows, damaged
//! lines, and the two text surfaces (a page's shape, one call's bytes).

use std::path::Path;

use super::*;
use crate::history::{TurnAttempt, TurnUsage};
use just_llm_client::types::generation::{Message, Reasoning};
use tempfile::TempDir;

/// A spill path in the shape the shell leaves in a stored banner.
const SPILL_PATH: &str = "/tmp/kallipai/1000/bash-exec/ab/0123456789abcdef.txt";

fn agent_dir() -> TempDir {
    TempDir::new().unwrap()
}

fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}

fn record(
    written_at: &str,
    turn_id: u64,
    messages: Vec<Message>,
    attempt: Option<TurnAttempt>,
) -> HistoryRecord {
    HistoryRecord {
        datetime: at(written_at),
        turn_id: Some(turn_id),
        messages,
        estimated_tokens: 0,
        kind: RecordKind::Turn,
        event: None,
        attachments: Vec::new(),
        attempt,
    }
}

/// Append records to one day file, the way the writer does.
fn append(dir: &Path, day: &str, records: &[HistoryRecord]) {
    let mut text = String::new();
    for record in records {
        text.push_str(&serde_json::to_string(record).unwrap());
        text.push('\n');
    }
    append_raw(dir, day, &text);
}

fn append_raw(dir: &Path, day: &str, lines: &str) {
    let history = dir.join("history");
    std::fs::create_dir_all(&history).unwrap();
    let path = history.join(format!("{day}.ndjson"));
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    text.push_str(lines);
    std::fs::write(path, text).unwrap();
}

fn attempt(model: &str) -> TurnAttempt {
    TurnAttempt {
        profile_set: "default".to_owned(),
        profile_id: "p1".to_owned(),
        provider: "openai".to_owned(),
        model: model.to_owned(),
        effort: Some("high".to_owned()),
        round: 0,
        usage: Some(TurnUsage {
            prompt: 10,
            completion: 5,
            cache_read: 2,
            cache_write: 1,
            reasoning: 3,
        }),
        elapsed_ms: 1200,
    }
}

fn query(day: Option<&str>, before: Option<String>, limit: Option<u32>) -> TurnsQuery {
    TurnsQuery {
        day: day.map(str::to_owned),
        before,
        limit,
    }
}

fn page(dir: &Path, before: Option<String>, limit: u32) -> TurnsPage {
    read_turns_page(dir, &query(None, before, Some(limit))).unwrap()
}

fn ids(page: &TurnsPage) -> Vec<u64> {
    page.rows.iter().map(|row| row.turn_id).collect()
}

/// One turn whose rows are spread over two day files: a repair row in the
/// newest file (`2026-10-20`), and the attempt block it folds in, carried by
/// the file `day` names. Every file between the two holds one filler turn,
/// each with an attempt.
///
/// `day` therefore names a file older than the repair row and no older than
/// the log's head, `2026-10-01`..=`2026-10-19`; it also sets how far behind
/// the newest file that attempt row sits, which is what the lookback tests
/// walk.
fn log_with_the_repair_row_on(day: &str) -> TempDir {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-20",
        &[record(
            "2026-10-20T10:00:00Z",
            5,
            vec![Message::user("repaired")],
            None,
        )],
    );
    for filler in 2..=19u32 {
        append(
            dir.path(),
            &format!("2026-10-{filler:02}"),
            &[record(
                &format!("2026-10-{filler:02}T09:00:00Z"),
                100 + u64::from(filler),
                vec![Message::user("filler")],
                Some(attempt("filler-model")),
            )],
        );
    }
    append(
        dir.path(),
        day,
        &[record(
            &format!("{day}T08:00:00Z"),
            5,
            vec![Message::user("damaged")],
            Some(attempt("old-model")),
        )],
    );
    dir
}

/// Five turns over two days, walked three pages at a time.
#[test]
fn page_pages_backwards_without_gaps_or_repeats() {
    let dir = agent_dir();
    let day_one = [
        record("2026-10-01T10:00:00Z", 1, vec![Message::user("one")], None),
        record("2026-10-01T10:01:00Z", 2, vec![Message::user("two")], None),
        record(
            "2026-10-01T10:02:00Z",
            3,
            vec![Message::user("three")],
            None,
        ),
    ];
    let day_two = [
        record("2026-10-02T10:00:00Z", 4, vec![Message::user("four")], None),
        record("2026-10-02T10:01:00Z", 5, vec![Message::user("five")], None),
    ];
    append(dir.path(), "2026-10-01", &day_one);
    append(dir.path(), "2026-10-02", &day_two);

    let first = page(dir.path(), None, 2);
    assert_eq!(ids(&first), vec![5, 4]);
    let cursor = first.next_cursor.clone().expect("a second page follows");

    let second = page(dir.path(), Some(cursor), 2);
    assert_eq!(ids(&second), vec![3, 2]);
    let cursor = second.next_cursor.clone().expect("a third page follows");

    let third = page(dir.path(), Some(cursor), 2);
    assert_eq!(ids(&third), vec![1]);
    assert!(third.next_cursor.is_none(), "the log is exhausted");
}

/// A repaired turn shows the repair's messages and the older row's attempt:
/// the two halves come from different records of the same turn.
#[test]
fn a_repair_row_takes_the_messages_and_the_older_row_keeps_the_attempt() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            7,
            vec![Message::user("damaged")],
            Some(attempt("old-model")),
        )],
    );
    append(
        dir.path(),
        "2026-10-02",
        &[record(
            "2026-10-02T10:00:00Z",
            7,
            vec![Message::user("repaired")],
            None,
        )],
    );

    let listed = page(dir.path(), None, 10);
    assert_eq!(ids(&listed), vec![7], "one turn, one row");
    assert_eq!(
        listed.rows[0]
            .profile
            .as_ref()
            .map(|profile| profile.model.as_str()),
        Some("old-model"),
        "the attempt comes from the older row"
    );
    assert!(
        listed.rows[0].messages[0].text.is_none(),
        "a page carries no text"
    );

    let detail = read_turn(dir.path(), 7).unwrap();
    assert_eq!(detail.turn.messages.len(), 1);
    assert_eq!(
        detail.turn.messages[0].text.as_deref(),
        Some("repaired"),
        "the messages come from the newer row"
    );
}

/// A page that fills before the older row of a repaired turn is reached still
/// folds that row's attempt in: the walk goes on past the page for the model
/// round alone, and an older turn standing in between is skipped, not taken.
#[test]
fn a_full_page_still_folds_in_the_older_attempt() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            9,
            vec![Message::user("damaged")],
            Some(attempt("old-model")),
        )],
    );
    append(
        dir.path(),
        "2026-10-02",
        &[
            record("2026-10-02T10:00:00Z", 6, vec![Message::user("six")], None),
            record(
                "2026-10-02T10:01:00Z",
                9,
                vec![Message::user("repaired")],
                None,
            ),
        ],
    );

    let listed = page(dir.path(), None, 1);
    assert_eq!(ids(&listed), vec![9], "one turn behind a full page");
    assert_eq!(
        listed.rows[0]
            .profile
            .as_ref()
            .map(|profile| profile.model.as_str()),
        Some("old-model"),
        "the attempt comes from the row below the page"
    );
    assert_eq!(
        listed.rows[0].usage.as_ref().map(|usage| usage.prompt),
        Some(10),
        "the usage rides the same block"
    );

    let detail = read_turn(dir.path(), 9).unwrap();
    assert_eq!(
        detail.turn.messages[0].text.as_deref(),
        Some("repaired"),
        "the messages stay the repair row's"
    );
}

/// A page that fills on the last row of a day file still offers the next page
/// while older files remain: the attempt lookback must not end the walk early.
#[test]
fn a_full_page_at_a_file_boundary_still_offers_the_next_page() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[
            record(
                "2026-10-01T10:00:00Z",
                1,
                vec![Message::user("one")],
                Some(attempt("m1")),
            ),
            record(
                "2026-10-01T10:01:00Z",
                2,
                vec![Message::user("two")],
                Some(attempt("m1")),
            ),
        ],
    );
    append(
        dir.path(),
        "2026-10-02",
        &[
            record(
                "2026-10-02T10:00:00Z",
                3,
                vec![Message::user("three")],
                Some(attempt("m1")),
            ),
            record(
                "2026-10-02T10:01:00Z",
                4,
                vec![Message::user("four")],
                Some(attempt("m1")),
            ),
        ],
    );

    let first = page(dir.path(), None, 2);
    assert_eq!(ids(&first), vec![4, 3]);
    let cursor = first
        .next_cursor
        .clone()
        .expect("older files remain, so a page follows");

    let second = page(dir.path(), Some(cursor), 2);
    assert_eq!(ids(&second), vec![2, 1], "the older day is reached");
    assert!(second.next_cursor.is_none(), "the log itself is exhausted");
}

/// A torn or corrupt line is skipped and counted over the lines the page
/// actually walked, not over the whole file.
#[test]
fn a_bad_line_is_skipped_and_counted_per_page() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            11,
            vec![Message::user("a")],
            None,
        )],
    );
    append_raw(dir.path(), "2026-10-01", "{\"torn\": \n");
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:02:00Z",
            12,
            vec![Message::user("b")],
            None,
        )],
    );

    let newest = page(dir.path(), None, 1);
    assert_eq!(ids(&newest), vec![12]);
    assert_eq!(newest.bad_lines, 0, "the page stopped above the bad line");

    let cursor = newest.next_cursor.clone().expect("a second page follows");
    let older = page(dir.path(), Some(cursor), 1);
    assert_eq!(ids(&older), vec![11]);
    assert_eq!(older.bad_lines, 1, "walking past it counts it once");
}

/// A record with no attempt block reads as having no model round: every
/// model-round field is absent rather than zero.
#[test]
fn a_turn_without_an_attempt_reads_as_no_model_round() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            3,
            vec![Message::user("plain")],
            None,
        )],
    );

    let listed = page(dir.path(), None, 10);
    let row = &listed.rows[0];
    assert!(row.profile.is_none());
    assert!(row.usage.is_none());
    assert!(row.elapsed_ms.is_none());
    assert!(row.calls.is_empty());

    let detail = read_turn(dir.path(), 3).unwrap();
    assert!(detail.turn.profile.is_none());
    assert_eq!(detail.turn.messages[0].chars, 5);
}

/// Calls come out in the order the model sent them, each carrying what its
/// result envelope reported.
#[test]
fn calls_pair_by_id_and_carry_the_envelope_facts() {
    let dir = agent_dir();
    let calls = vec![
        ToolCall {
            id: "c1".to_owned(),
            name: "bash_exec".to_owned(),
            arguments: "{\"command\":\"printf 'a\\nb\\nc'\"}".to_owned(),
        },
        ToolCall {
            id: "c2".to_owned(),
            name: "read_file".to_owned(),
            arguments: "{\"path\":\"/tmp/x\"}".to_owned(),
        },
    ];
    let shell = ToolResultEnvelope {
        ok: true,
        tool_name: "bash_exec".to_owned(),
        result: Some(serde_json::json!({
            "output": "a\nb\nc",
            "exit_code": 0,
            "timed_out": false,
            "truncated": false,
        })),
        error: None,
        pending_approval: None,
        rest: serde_json::Map::new(),
    };
    let other = ToolResultEnvelope {
        ok: true,
        tool_name: "read_file".to_owned(),
        result: Some(serde_json::json!({"lines": 3})),
        error: None,
        pending_approval: None,
        rest: serde_json::Map::new(),
    };
    let messages = vec![
        Message::user("go"),
        Message::assistant_tool_calls(None, calls, None),
        Message::tool(serde_json::to_string(&other).unwrap(), "c2"),
        Message::tool(serde_json::to_string(&shell).unwrap(), "c1"),
    ];
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            9,
            messages,
            Some(attempt("m")),
        )],
    );

    let detail = read_turn(dir.path(), 9).unwrap();
    let rows = &detail.turn.calls;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].call_index, rows[0].name.as_str()),
        (0, "bash_exec")
    );
    assert_eq!(
        (rows[1].call_index, rows[1].name.as_str()),
        (1, "read_file")
    );
    assert!(rows[0].ok && rows[1].ok);
    assert_eq!(rows[0].stdout_bytes, Some(5), "the merged stream counts");
    assert_eq!(rows[0].stdout_lines, Some(3));
    assert_eq!(rows[0].exit_code, Some(0));
    assert_eq!(rows[0].timed_out, Some(false));
    assert!(!rows[0].truncated);
    assert_eq!(rows[1].stdout_bytes, None, "no stream, no size");
    assert_eq!(rows[1].exit_code, None);
    let profile = detail.turn.profile.expect("the attempt is projected");
    assert_eq!(profile.effort.as_deref(), Some("high"));
    assert_eq!(detail.turn.usage.map(|usage| usage.prompt), Some(10));
    assert_eq!(detail.turn.elapsed_ms, Some(1200));
}

/// A banner naming a spill file is reported as reclaimed, everywhere: the
/// arguments, the error line, and the call's own text. The path itself never
/// leaves the log.
#[test]
fn a_spill_path_never_reaches_the_view() {
    let dir = agent_dir();
    let banner = format!(
        "[stdout was clipped (middle omitted); read the full output with: cat {SPILL_PATH}]\nhead\ntail"
    );
    let envelope = ToolResultEnvelope {
        ok: false,
        tool_name: "bash_exec".to_owned(),
        result: Some(serde_json::json!({
            "output": banner,
            "exit_code": 1,
            "timed_out": false,
            "truncated": true,
        })),
        error: Some(format!("spilled to {SPILL_PATH}")),
        pending_approval: None,
        rest: serde_json::Map::new(),
    };
    let calls = vec![ToolCall {
        id: "c1".to_owned(),
        name: "bash_exec".to_owned(),
        arguments: format!("{{\"command\":\"cat {SPILL_PATH}\"}}"),
    }];
    let messages = vec![
        Message::assistant_tool_calls(None, calls, None),
        Message::tool(serde_json::to_string(&envelope).unwrap(), "c1"),
    ];
    append(
        dir.path(),
        "2026-10-01",
        &[record("2026-10-01T10:00:00Z", 4, messages, None)],
    );

    let detail = read_turn(dir.path(), 4).unwrap();
    let rendered = serde_json::to_string(&detail).unwrap();
    assert!(
        !rendered.contains("/kallipai/"),
        "no spill path survives: {rendered}"
    );
    assert!(rendered.contains(SPILL_RECLAIMED));
    assert!(
        detail.turn.calls[0].args.contains(SPILL_RECLAIMED),
        "the argument blob is reclaimed too"
    );
    assert!(
        detail.turn.calls[0]
            .first_error_line
            .as_deref()
            .is_some_and(|line| line.contains(SPILL_RECLAIMED))
    );

    let body = read_call_body(dir.path(), 4, 0, &CallBodyQuery::default()).unwrap();
    assert_eq!(
        body.reclaimed, 2,
        "the banner and the error each name it once"
    );
    assert!(!body.text.contains("/kallipai/"));
    assert!(
        body.text.contains("was clipped"),
        "the clipping itself is still reported"
    );
}

/// A page that would exceed its byte cap stops early and still hands back a
/// cursor, so the next page continues where it stopped.
#[test]
fn the_page_stops_at_its_byte_cap() {
    let dir = agent_dir();
    let long = "x".repeat(TEXT_CUT_CHARS);
    for index in 0..4u64 {
        let calls: Vec<ToolCall> = (0..100)
            .map(|call| ToolCall {
                id: format!("c{call}"),
                name: "bash_exec".to_owned(),
                arguments: long.clone(),
            })
            .collect();
        let messages = vec![Message::assistant_tool_calls(None, calls, None)];
        append(
            dir.path(),
            "2026-10-01",
            &[record(
                &format!("2026-10-01T10:0{index}:00Z"),
                index + 1,
                messages,
                None,
            )],
        );
    }

    let capped = page(dir.path(), None, 200);
    assert!(capped.page_truncated, "the byte cap ended the page");
    let encoded = serde_json::to_string(&capped).unwrap().len();
    assert!(
        encoded <= TURNS_PAGE_MAX_BYTES,
        "the page stays under the cap: {encoded}"
    );
    assert!(capped.rows.len() < 4, "rows were dropped from the tail");
    assert!(capped.next_cursor.is_some(), "the cursor still advances");

    let rest = page(dir.path(), capped.next_cursor.clone(), 200);
    let seen: Vec<u64> = capped
        .rows
        .iter()
        .chain(rest.rows.iter())
        .map(|row| row.turn_id)
        .collect();
    assert_eq!(seen, vec![4, 3, 2, 1], "no gap, no repeat across the cap");
}

/// The call body is a byte window over the text the log holds for that call.
#[test]
fn a_call_body_ranges_over_the_stored_text() {
    let dir = agent_dir();
    let output = (0..40)
        .map(|line| format!("line-{line:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    let envelope = ToolResultEnvelope {
        ok: true,
        tool_name: "bash_exec".to_owned(),
        result: Some(serde_json::json!({
            "output": output,
            "exit_code": 0,
            "timed_out": false,
            "truncated": false,
        })),
        error: None,
        pending_approval: None,
        rest: serde_json::Map::new(),
    };
    let stored = serde_json::to_string(&envelope).unwrap();
    let calls = vec![ToolCall {
        id: "c1".to_owned(),
        name: "bash_exec".to_owned(),
        arguments: "{}".to_owned(),
    }];
    let messages = vec![
        Message::assistant_tool_calls(None, calls, None),
        Message::tool(stored.clone(), "c1"),
    ];
    append(
        dir.path(),
        "2026-10-01",
        &[record("2026-10-01T10:00:00Z", 5, messages, None)],
    );

    let whole = read_call_body(dir.path(), 5, 0, &CallBodyQuery::default()).unwrap();
    assert_eq!(whole.text, stored);
    assert_eq!(whole.total_bytes, stored.len() as u64);
    assert_eq!(whole.offset, 0);
    assert_eq!(whole.call_id, "c1");
    assert!(!whole.truncated);
    assert!(whole.next_offset.is_none());
    assert_eq!(whole.reclaimed, 0);

    let ranged = read_call_body(
        dir.path(),
        5,
        0,
        &CallBodyQuery {
            offset: Some(10),
            limit: Some(7),
        },
    )
    .unwrap();
    assert_eq!(ranged.text, stored[10..17]);
    assert_eq!(ranged.offset, 10);
    assert!(ranged.truncated);
    assert_eq!(ranged.next_offset, Some(17));

    let past_the_end = read_call_body(
        dir.path(),
        5,
        0,
        &CallBodyQuery {
            offset: Some(whole.total_bytes),
            limit: Some(4),
        },
    )
    .unwrap();
    assert_eq!(
        past_the_end.text, "",
        "an offset at the end is an empty tail"
    );
    assert!(!past_the_end.truncated);
}

/// A call with no result row, or one whose text is not an envelope, reads as
/// not ok and carries no result facts.
#[test]
fn an_unanswered_call_reads_as_not_ok() {
    let dir = agent_dir();
    let calls = vec![
        ToolCall {
            id: "c1".to_owned(),
            name: "bash_exec".to_owned(),
            arguments: "{}".to_owned(),
        },
        ToolCall {
            id: "c2".to_owned(),
            name: "read_file".to_owned(),
            arguments: "{}".to_owned(),
        },
    ];
    let messages = vec![
        Message::assistant_tool_calls(None, calls, None),
        Message::tool("plain text, not an envelope", "c2"),
    ];
    append(
        dir.path(),
        "2026-10-01",
        &[record("2026-10-01T10:00:00Z", 6, messages, None)],
    );

    let detail = read_turn(dir.path(), 6).unwrap();
    assert!(detail.turn.calls.iter().all(|call| !call.ok));
    assert!(detail.turn.calls[0].stdout_bytes.is_none());
    assert!(detail.turn.calls[0].exit_code.is_none());

    let body = read_call_body(dir.path(), 6, 0, &CallBodyQuery::default()).unwrap();
    assert_eq!(body.total_bytes, 0);
    assert_eq!(body.text, "");
    assert_eq!(
        read_call_body(dir.path(), 6, 9, &CallBodyQuery::default()).unwrap_err(),
        ViewError::NoSuchCall(6, 9)
    );
}

/// A day is a path component, so a malformed one is refused before any
/// lookup.
#[test]
fn a_malformed_day_or_cursor_is_refused() {
    let dir = agent_dir();
    let bad_day = read_turns_page(dir.path(), &query(Some("2026-10-1"), None, None)).unwrap_err();
    assert_eq!(bad_day, ViewError::BadDay("2026-10-1".to_owned()));
    let traversal = read_turns_page(dir.path(), &query(Some("../../etc"), None, None)).unwrap_err();
    assert_eq!(traversal, ViewError::BadDay("../../etc".to_owned()));

    for cursor in ["nonsense", "abc|4", "1|abc"] {
        let error = read_turns_page(dir.path(), &query(None, Some(cursor.to_owned()), None));
        assert_eq!(
            error.unwrap_err(),
            ViewError::BadCursor(cursor.to_owned()),
            "cursor {cursor}"
        );
    }
}

/// A day with no file is an empty page, and a day query never crosses into
/// the neighbouring day.
#[test]
fn a_day_query_walks_that_day_only() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            1,
            vec![Message::user("a")],
            None,
        )],
    );
    append(
        dir.path(),
        "2026-10-02",
        &[record(
            "2026-10-02T10:00:00Z",
            2,
            vec![Message::user("b")],
            None,
        )],
    );

    let first_day =
        read_turns_page(dir.path(), &query(Some("2026-10-01"), None, Some(10))).unwrap();
    assert_eq!(ids(&first_day), vec![1]);
    assert!(
        first_day.next_cursor.is_none(),
        "the day is exhausted, not the log"
    );

    let no_file = read_turns_page(dir.path(), &query(Some("2020-01-01"), None, Some(10))).unwrap();
    assert!(no_file.rows.is_empty());
    assert_eq!(no_file.bad_lines, 0);
    assert!(no_file.next_cursor.is_none());
}

/// A stored tool-result envelope carrying one stream, as the log keeps it.
fn envelope_with_output(output: &str) -> String {
    let envelope = ToolResultEnvelope {
        ok: true,
        tool_name: "bash_exec".to_owned(),
        result: Some(serde_json::json!({
            "output": output,
            "exit_code": 0,
            "timed_out": false,
            "truncated": false,
        })),
        error: None,
        pending_approval: None,
        rest: serde_json::Map::new(),
    };
    serde_json::to_string(&envelope).unwrap()
}

/// One assistant call answered by `stored`: the two messages a result needs.
fn tool_messages(stored: &str) -> Vec<Message> {
    let calls = vec![ToolCall {
        id: "c1".to_owned(),
        name: "bash_exec".to_owned(),
        arguments: "{}".to_owned(),
    }];
    vec![
        Message::assistant_tool_calls(None, calls, None),
        Message::tool(stored.to_owned(), "c1"),
    ]
}

/// A turn whose two rows straddle a page boundary appears once: the newer row
/// is on the first page, and the older one, met again further down, does not
/// open a second window.
#[test]
fn a_turn_split_across_a_page_boundary_is_not_shown_twice() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            7,
            vec![Message::user("damaged")],
            Some(attempt("old-model")),
        )],
    );
    append(
        dir.path(),
        "2026-10-02",
        &[
            record(
                "2026-10-02T10:00:00Z",
                8,
                vec![Message::user("eight")],
                None,
            ),
            record(
                "2026-10-02T10:01:00Z",
                7,
                vec![Message::user("repaired")],
                None,
            ),
        ],
    );

    let first = page(dir.path(), None, 1);
    assert_eq!(ids(&first), vec![7], "the repair row is the newest");
    assert_eq!(
        first.rows[0]
            .profile
            .as_ref()
            .map(|profile| profile.model.as_str()),
        Some("old-model"),
        "the folded attempt still rides along"
    );
    let cursor = first.next_cursor.clone().expect("a second page follows");

    let second = page(dir.path(), Some(cursor), 1);
    assert_eq!(
        ids(&second),
        vec![8],
        "the older row of turn 7 is not a second window"
    );
    assert!(second.next_cursor.is_none(), "the log is exhausted");
}

/// The row limit is clamped at both ends: a zero still reads one row, and an
/// oversized request stops at the maximum.
#[test]
fn the_row_limit_is_clamped_at_both_ends() {
    let dir = agent_dir();
    let mut records = Vec::new();
    for index in 0..(TURNS_PAGE_LIMIT_MAX as usize + 3) {
        let at = format!("2026-10-01T{:02}:{:02}:00Z", 10 + index / 60, index % 60);
        records.push(record(
            &at,
            1000 + index as u64,
            vec![Message::user("x")],
            None,
        ));
    }
    append(dir.path(), "2026-10-01", &records);

    let zero = read_turns_page(dir.path(), &query(None, None, Some(0))).unwrap();
    assert_eq!(zero.rows.len(), 1, "a zero limit still reads one row");

    let huge = read_turns_page(dir.path(), &query(None, None, Some(u32::MAX))).unwrap();
    assert_eq!(
        huge.rows.len(),
        TURNS_PAGE_LIMIT_MAX as usize,
        "an oversized request stops at the maximum"
    );
}

/// A day query and a cursor compose: the walk stays inside the day and starts
/// below the cursor rather than at the newest row.
#[test]
fn a_day_query_takes_the_cursor_too() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[
            record("2026-10-01T10:00:00Z", 1, vec![Message::user("a")], None),
            record("2026-10-01T10:01:00Z", 2, vec![Message::user("b")], None),
            record("2026-10-01T10:02:00Z", 3, vec![Message::user("c")], None),
        ],
    );

    let first = read_turns_page(dir.path(), &query(Some("2026-10-01"), None, Some(1))).unwrap();
    assert_eq!(ids(&first), vec![3]);
    let cursor = first.next_cursor.clone().expect("a second row follows");

    let second = read_turns_page(
        dir.path(),
        &query(Some("2026-10-01"), Some(cursor), Some(5)),
    )
    .unwrap();
    assert_eq!(
        ids(&second),
        vec![2, 1],
        "the cursor pins where the walk starts"
    );
    assert!(second.next_cursor.is_none(), "the day is exhausted");
}

/// A range read always advances, even when the limit is narrower than one
/// character: it widens to the next boundary instead of returning nothing.
#[test]
fn a_range_read_advances_past_characters_wider_than_its_limit() {
    let dir = agent_dir();
    // Two, three and four bytes, so every widening is exercised.
    let output = "aé€𝄞z";
    let stored = envelope_with_output(output);
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            8,
            tool_messages(&stored),
            None,
        )],
    );

    let mut offset = Some(0u64);
    let mut rebuilt = String::new();
    let mut steps = 0usize;
    while let Some(at) = offset {
        let body = read_call_body(
            dir.path(),
            8,
            0,
            &CallBodyQuery {
                offset: Some(at),
                limit: Some(1),
            },
        )
        .unwrap();
        assert_eq!(body.offset, at, "the range starts where it was asked to");
        assert!(
            !body.text.is_empty(),
            "a range short of the end is never empty"
        );
        rebuilt.push_str(&body.text);
        offset = body.next_offset;
        steps += 1;
        assert!(steps <= stored.len() + 1, "the walk terminates");
    }
    assert_eq!(rebuilt, stored, "the ranges put the text back together");
}

/// A crash can cut the last line mid-character: the bytes that are left still
/// read, and the fragment counts as one bad line instead of hiding the day.
#[test]
fn a_cut_character_does_not_hide_the_rest_of_the_day() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            12,
            vec![Message::user("a")],
            None,
        )],
    );
    let torn = serde_json::to_string(&record(
        "2026-10-01T10:01:00Z",
        13,
        vec![Message::user("é")],
        None,
    ))
    .unwrap();
    let mut bytes = torn.clone().into_bytes();
    let at = torn
        .find('é')
        .expect("the character is written as it stands");
    bytes.truncate(at + 1);
    let path = dir.path().join("history").join("2026-10-01.ndjson");
    let mut content = std::fs::read(&path).unwrap();
    content.extend_from_slice(&bytes);
    content.push(b'\n');
    std::fs::write(&path, content).unwrap();

    let listed = page(dir.path(), None, 10);
    assert_eq!(ids(&listed), vec![12], "the readable row still reads");
    assert_eq!(listed.bad_lines, 1, "the cut line counts as one bad line");
    assert!(
        read_turn(dir.path(), 12).is_ok(),
        "the detail read is unharmed"
    );
}

/// Counters measure the text the record holds, not the text this face shows:
/// a spilled stream still reports what the tool produced.
#[test]
fn the_stream_counters_measure_the_record_not_the_rendering() {
    let dir = agent_dir();
    let banner = format!("[stdout was clipped; read the full output with: cat {SPILL_PATH}]\nhead");
    let stored = envelope_with_output(&banner);
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            9,
            tool_messages(&stored),
            None,
        )],
    );

    let detail = read_turn(dir.path(), 9).unwrap();
    let call = &detail.turn.calls[0];
    assert_eq!(
        call.stdout_bytes,
        Some(banner.len() as u64),
        "the size is the record's own"
    );
    assert_eq!(call.stdout_lines, Some(2), "so is the line count");

    let rendered = serde_json::to_string(&detail).unwrap();
    assert!(
        !rendered.contains("/kallipai/"),
        "the path itself never renders: {rendered}"
    );
    let tool = detail
        .turn
        .messages
        .iter()
        .find(|message| message.role == "tool")
        .expect("the result row is a message");
    assert_eq!(
        tool.chars,
        stored.chars().count(),
        "chars counts the record's text, not the reclaimed one"
    );
    assert!(
        tool.text
            .as_deref()
            .is_some_and(|text| text.contains(SPILL_RECLAIMED)),
        "the rendering still reclaims the path"
    );
}

/// The lookback window's two edges. Counting the day file that filled the
/// page as one, the walk reads that file and thirteen older ones: an attempt
/// on the last of those still arrives, one file further back does not.
#[test]
fn an_attempt_window_ends_thirteen_files_behind_the_filling_file() {
    let inside = log_with_the_repair_row_on("2026-10-07");
    let listed = page(inside.path(), None, 1);
    assert_eq!(ids(&listed), vec![5]);
    assert_eq!(
        listed.rows[0]
            .profile
            .as_ref()
            .map(|profile| profile.model.as_str()),
        Some("old-model"),
        "the last file the window reaches still supplies the model round"
    );

    let outside = log_with_the_repair_row_on("2026-10-06");
    let listed = page(outside.path(), None, 1);
    assert_eq!(ids(&listed), vec![5]);
    assert!(
        listed.rows[0].profile.is_none(),
        "one file further back is past the window"
    );
}

/// A page that never fills walks to the head of the log, so no lookback bound
/// applies: an attempt nineteen files back still arrives.
#[test]
fn an_unfilled_page_reads_to_the_head_of_the_log() {
    let dir = log_with_the_repair_row_on("2026-10-01");
    let listed = page(dir.path(), None, 25);
    assert_eq!(listed.rows.len(), 19, "the log holds nineteen turns");
    assert_eq!(
        listed.rows[0]
            .profile
            .as_ref()
            .map(|profile| profile.model.as_str()),
        Some("old-model"),
        "an unfilled page carries no lookback bound"
    );
}

/// A repair row appended while a reader is paging sits above the cursor, so no
/// later page shows that turn: the cost the docs state, pinned here so a
/// change to the cursor rule has to face it.
#[test]
fn a_repair_row_written_between_pages_stays_out_of_later_pages() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[
            // Appended oldest first, the way the writer leaves them: within one
            // day file the last line is the newest record.
            record(
                "2026-10-01T09:59:00Z",
                3,
                vec![Message::user("three, earlier")],
                None,
            ),
            record("2026-10-01T10:00:00Z", 1, vec![Message::user("one")], None),
            record("2026-10-01T10:01:00Z", 2, vec![Message::user("two")], None),
            record(
                "2026-10-01T10:02:00Z",
                3,
                vec![Message::user("three")],
                None,
            ),
        ],
    );

    let first = page(dir.path(), None, 1);
    assert_eq!(ids(&first), vec![3]);
    let cursor = first.next_cursor.clone().expect("a second page follows");

    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:03:00Z",
            3,
            vec![Message::user("three, repaired")],
            None,
        )],
    );

    let second = page(dir.path(), Some(cursor), 1);
    assert_eq!(ids(&second), vec![2], "the turn above the cursor stays out");
    // The turn's own older row still sits below the cursor: the reader that
    // already passed it must not be handed the turn a second time.
    let third = page(dir.path(), second.next_cursor.clone(), 1);
    assert_eq!(ids(&third), vec![1]);
    assert!(
        third.next_cursor.is_none(),
        "nothing follows the older turn"
    );
}

/// A walk that switches its `day` between pages keeps no memory of the turns it
/// already passed, and its cursor is a position rather than a resume point
/// inside one file: a turn comes back a second time, and the files between the
/// two pages are stepped over. Both costs the docs state, pinned here.
#[test]
fn a_day_switch_between_pages_repeats_a_turn_and_steps_over_files() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-15",
        &[
            record(
                "2026-10-15T09:00:00Z",
                3,
                vec![Message::user("three, older")],
                None,
            ),
            record(
                "2026-10-15T10:00:00Z",
                7,
                vec![Message::user("seven")],
                None,
            ),
        ],
    );
    append(
        dir.path(),
        "2026-10-16",
        &[record(
            "2026-10-16T09:00:00Z",
            6,
            vec![Message::user("six")],
            None,
        )],
    );
    append(
        dir.path(),
        "2026-10-17",
        &[
            record(
                "2026-10-17T09:00:00Z",
                3,
                vec![Message::user("three, repaired")],
                None,
            ),
            record("2026-10-17T10:00:00Z", 5, vec![Message::user("five")], None),
        ],
    );

    let first = page(dir.path(), None, 2);
    assert_eq!(ids(&first), vec![5, 3]);
    let cursor = first.next_cursor.clone().expect("a second page follows");

    let switched = read_turns_page(
        dir.path(),
        &query(Some("2026-10-15"), Some(cursor), Some(2)),
    )
    .unwrap();
    // The turn read on the first page comes back: its newest row sits in a file
    // this walk never opens, so nothing marks it as already passed. Turn 6 sits
    // below the cursor in 10-16 and is stepped over entirely.
    assert_eq!(ids(&switched), vec![7, 3], "the pinned day repeats a turn");
}

/// `page_truncated` marks the byte cap and nothing else: a page stopped by its
/// row limit and a page that ran out of log both report false.
#[test]
fn a_page_stopped_by_its_row_limit_or_by_the_log_end_is_not_truncated() {
    let dir = agent_dir();
    append(
        dir.path(),
        "2026-10-01",
        &[
            record("2026-10-01T10:00:00Z", 1, vec![Message::user("one")], None),
            record("2026-10-01T10:01:00Z", 2, vec![Message::user("two")], None),
            record(
                "2026-10-01T10:02:00Z",
                3,
                vec![Message::user("three")],
                None,
            ),
        ],
    );

    let row_limited = page(dir.path(), None, 2);
    assert_eq!(ids(&row_limited), vec![3, 2]);
    assert!(!row_limited.page_truncated, "the row limit is not the cap");
    assert!(row_limited.next_cursor.is_some(), "turns remain below it");

    let whole_log = page(dir.path(), None, 10);
    assert_eq!(ids(&whole_log), vec![3, 2, 1]);
    assert!(!whole_log.page_truncated, "the log ended inside the page");
    assert!(whole_log.next_cursor.is_none(), "nothing follows");
}

/// The reasoning text is a detail-face fact: a page reports the flag, one
/// turn hands back the shortened text, and a record holding only the
/// redacted marker reports the flag with no text at all.
#[test]
fn the_detail_face_carries_the_shortened_reasoning_text() {
    let dir = agent_dir();
    let long = "r".repeat(TEXT_CUT_CHARS + 20);
    let held = Message::assistant_tool_calls(
        Some("done".to_owned()),
        Vec::new(),
        Some(Reasoning {
            text: Some(long),
            ..Reasoning::default()
        }),
    );
    let marker_only = Message::assistant_tool_calls(
        None,
        Vec::new(),
        Some(Reasoning {
            redacted: Some("cipher".to_owned()),
            ..Reasoning::default()
        }),
    );
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            1,
            vec![held, marker_only],
            None,
        )],
    );

    let detail = read_turn(dir.path(), 1).unwrap();
    let rows = &detail.turn.messages;
    let text = rows[0].reasoning.as_deref().unwrap();
    assert!(rows[0].reasoning_truncated, "the long text was shortened");
    assert!(text.contains(" … "), "both ends survive: {text}");
    assert_eq!(text.chars().count(), TEXT_CUT_CHARS + 3);
    assert!(rows[1].has_reasoning, "the marker alone is still reasoning");
    assert!(rows[1].reasoning.is_none(), "no text was stored");
    assert!(!rows[1].reasoning_truncated, "nothing was shortened");

    let listed = read_turns_page(dir.path(), &query(Some("2026-10-01"), None, Some(10))).unwrap();
    assert_eq!(ids(&listed), vec![1]);
    for row in &listed.rows[0].messages {
        assert!(row.has_reasoning, "the flag survives the page");
        assert!(row.reasoning.is_none(), "a page carries the flag alone");
        assert!(!row.reasoning_truncated, "a page reports no shortening");
        let encoded = serde_json::to_string(row).unwrap();
        assert!(
            !encoded.contains("reasoning_truncated"),
            "the flag leaves no trace on a page: {encoded}"
        );
        assert!(
            !encoded.contains("\"reasoning\":"),
            "no reasoning text on the wire: {encoded}"
        );
    }
}

/// A reasoning block whose text is empty or all blanks reports the flag
/// with no text: a fold that opens on nothing is a rendering fault, not a
/// fact, and whitespace is nothing to show.
#[test]
fn an_empty_reasoning_text_reads_as_no_text() {
    let dir = agent_dir();
    let blank = Message::assistant_tool_calls(
        None,
        Vec::new(),
        Some(Reasoning {
            text: Some(String::new()),
            ..Reasoning::default()
        }),
    );
    let spaces = Message::assistant_tool_calls(
        None,
        Vec::new(),
        Some(Reasoning {
            text: Some(" \n\t ".to_owned()),
            ..Reasoning::default()
        }),
    );
    let padded = Message::assistant_tool_calls(
        None,
        Vec::new(),
        Some(Reasoning {
            text: Some(" because ".to_owned()),
            ..Reasoning::default()
        }),
    );
    let inner = Message::assistant_tool_calls(
        None,
        Vec::new(),
        Some(Reasoning {
            text: Some("first\n\nsecond".to_owned()),
            ..Reasoning::default()
        }),
    );
    append(
        dir.path(),
        "2026-10-01",
        &[record(
            "2026-10-01T10:00:00Z",
            1,
            vec![blank, spaces, padded, inner],
            None,
        )],
    );

    let detail = read_turn(dir.path(), 1).unwrap();
    let rows = &detail.turn.messages;
    assert_eq!(rows.len(), 4, "all four shapes were read");
    for row in &rows[..2] {
        assert!(row.has_reasoning, "the record kept a reasoning block");
        assert!(row.reasoning.is_none(), "a blank text is no text");
        assert!(!row.reasoning_truncated, "nothing was shortened");
    }
    assert_eq!(
        rows[2].reasoning.as_deref(),
        Some(" because "),
        "padding around a text is still a text"
    );
    assert_eq!(
        rows[3].reasoning.as_deref(),
        Some("first\n\nsecond"),
        "a blank line inside a text is still a text"
    );
}
