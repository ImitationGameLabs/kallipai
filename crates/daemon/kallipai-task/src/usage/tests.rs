//! The aggregation rules, one test per branch of the window model.

use super::*;

/// A snapshot whose agents carry the given (prompt, completion) readings.
fn snap(entries: &[(&str, u64, u64)]) -> UsageSnapshot {
    let agents = entries
        .iter()
        .map(|(agent_id, prompt_tokens, completion_tokens)| {
            (
                agent_id.to_string(),
                CapturedUsage {
                    usage: CumulativeUsage {
                        prompt_tokens: *prompt_tokens,
                        completion_tokens: *completion_tokens,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                    source: UsageSource::Live,
                },
            )
        })
        .collect();
    UsageSnapshot {
        agents,
        skipped: Vec::new(),
        captured_at: 0,
        capture_error: None,
    }
}

fn event(id: i64, name: &str, at: i64, snapshot: Option<&UsageSnapshot>) -> task_event::Model {
    task_event::Model {
        id,
        task_id: 1,
        kind: "transition".to_string(),
        name: name.to_string(),
        actor: Some("dev".to_string()),
        assignee: None,
        from_status: None,
        to_status: None,
        payload: transition_payload(snapshot).expect("payload"),
        created_at: at,
    }
}

fn totals(usage: &TaskUsageExport) -> Vec<(String, u64)> {
    usage
        .agents
        .iter()
        .map(|agent| (agent.agent_id.clone(), agent.consumed_tokens))
        .collect()
}

fn windows(usage: &TaskUsageExport) -> Vec<(String, String)> {
    usage
        .segments
        .iter()
        .map(|segment| (segment.from_event.clone(), segment.to_event.clone()))
        .collect()
}

#[test]
fn one_window_sums_every_counter_delta() {
    let start = snap(&[("a", 100, 10), ("b", 0, 0)]);
    let close = snap(&[("a", 300, 30), ("b", 50, 5)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "close", 20, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert_eq!(usage.total_tokens, 220 + 55);
    assert_eq!(totals(&usage), vec![("a".into(), 220), ("b".into(), 55)]);
    assert_eq!(
        windows(&usage),
        vec![("start".to_string(), "close".to_string())]
    );
    assert!(!usage.partial && !usage.open && usage.gaps.is_empty());
}

#[test]
fn pause_and_resume_book_one_window_each() {
    // Time spent paused is not the task's: the resume reading re-bases the
    // counters, so the second window counts only what came after it.
    let start = snap(&[("a", 100, 0)]);
    let pause = snap(&[("a", 150, 0)]);
    let resume = snap(&[("a", 900, 0)]);
    let close = snap(&[("a", 1_000, 0)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "pause", 20, Some(&pause)),
        event(3, "resume", 30, Some(&resume)),
        event(4, "close", 40, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert_eq!(usage.total_tokens, 50 + 100);
    assert_eq!(
        windows(&usage),
        vec![
            ("start".to_string(), "pause".to_string()),
            ("resume".to_string(), "close".to_string()),
        ]
    );
    assert!(
        usage
            .segments
            .iter()
            .all(|segment| segment.total_tokens > 0)
    );
}

#[test]
fn reopen_rebases_the_window() {
    let start = snap(&[("a", 0, 0)]);
    let close = snap(&[("a", 100, 0)]);
    let reopen = snap(&[("a", 400, 0)]);
    let close_again = snap(&[("a", 450, 0)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "close", 20, Some(&close)),
        event(3, "reopen", 30, Some(&reopen)),
        event(4, "close", 40, Some(&close_again)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert_eq!(usage.total_tokens, 100 + 50);
}

#[test]
fn an_open_window_is_reported_but_not_counted() {
    let start = snap(&[("a", 700, 0)]);
    let events = vec![event(1, "start", 1_760_000_000, Some(&start))];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert!(usage.open);
    assert_eq!(usage.total_tokens, 0);
    assert_eq!(usage.open_since.as_deref(), Some("2025-10-09T08:53:20Z"));
    assert!(usage.segments.is_empty(), "an open window is no segment");
    // An unclosed window is normal for a running task, not a data gap.
    assert!(!usage.partial && usage.gaps.is_empty());
}

#[test]
fn a_terminal_without_a_baseline_books_a_gap() {
    let close = snap(&[("a", 500, 0), ("b", 20, 0)]);
    let events = vec![event(2, "close", 20, Some(&close))];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert_eq!(usage.total_tokens, 0);
    assert!(usage.partial);
    assert_eq!(usage.gaps.len(), 2);
    assert_eq!(usage.gaps[0].segment, 1);
    assert!(
        usage
            .gaps
            .iter()
            .all(|gap| gap.reason == UsageGapReason::MissingBaseline)
    );
}

#[test]
fn an_agent_present_at_only_one_end_books_a_gap() {
    let start = snap(&[("a", 0, 0)]);
    let close = snap(&[("a", 10, 0), ("late", 90, 0)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "close", 20, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert!(usage.partial);
    assert_eq!(usage.total_tokens, 10);
    assert_eq!(usage.gaps.len(), 1);
    assert_eq!(usage.gaps[0].agent_id, "late");
    assert_eq!(usage.gaps[0].reason, UsageGapReason::MissingBaseline);

    let start = snap(&[("a", 0, 0), ("gone", 0, 0)]);
    let close = snap(&[("a", 10, 0)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "close", 20, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert!(usage.partial);
    assert_eq!(usage.gaps[0].agent_id, "gone");
    assert_eq!(usage.gaps[0].reason, UsageGapReason::MissingTerminal);
}

#[test]
fn gaps_name_the_window_they_belong_to() {
    let start = snap(&[("a", 0, 0)]);
    let pause = snap(&[("a", 10, 0)]);
    let resume = snap(&[("a", 10, 0)]);
    let close = snap(&[("a", 20, 0), ("late", 5, 0)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "pause", 20, Some(&pause)),
        event(3, "resume", 30, Some(&resume)),
        event(4, "close", 40, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert_eq!(usage.gaps.len(), 1);
    assert_eq!(usage.gaps[0].segment, 2);
    assert_eq!(usage.total_tokens, 10 + 10);
}

#[test]
fn counters_that_went_backwards_are_clamped_and_named() {
    let start = snap(&[("a", 100, 40)]);
    let close = snap(&[("a", 40, 90)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "close", 20, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    // prompt went backwards (clamped to 0), completion moved 40 -> 90.
    assert_eq!(usage.total_tokens, 50);
    assert_eq!(usage.non_monotonic, vec!["a".to_string()]);
    assert!(!usage.partial);
}

#[test]
fn unreadable_and_skipped_captures_are_booked_not_zeroed() {
    let mut start = event(1, "start", 10, None);
    start.payload = Some("{\"usage_snapshot\": 7}".to_string());
    let close = snap(&[("a", 100, 0)]);
    let events = vec![start, event(2, "close", 20, Some(&close))];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert!(usage.partial);
    assert_eq!(usage.total_tokens, 0);
    assert_eq!(usage.gaps.len(), 1);
    assert_eq!(usage.gaps[0].reason, UsageGapReason::MissingBaseline);

    // An agent the capture could not read is left out of the map at both
    // ends: book it rather than let it vanish.
    let mut start = snap(&[("a", 0, 0)]);
    start.skipped.push(SkippedAgent {
        agent_id: "b".to_string(),
        reason: "manifest unreadable".to_string(),
    });
    let mut close = snap(&[("a", 10, 0)]);
    close.skipped.push(SkippedAgent {
        agent_id: "b".to_string(),
        reason: "manifest unreadable".to_string(),
    });
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "close", 20, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert!(usage.partial);
    assert_eq!(usage.total_tokens, 10);
    assert_eq!(usage.gaps.len(), 1);
    assert_eq!(usage.gaps[0].agent_id, "b");
}

#[test]
fn a_failed_capture_marks_the_task_partial() {
    let mut start = snap(&[("a", 0, 0)]);
    start.capture_error = Some("agent scan failed".to_string());
    let close = snap(&[("a", 10, 0)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "close", 20, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert_eq!(usage.total_tokens, 10);
    assert!(usage.partial);
}

#[test]
fn a_task_that_predates_the_snapshots_reports_none() {
    let events = vec![event(1, "start", 10, None), event(2, "close", 20, None)];
    assert!(aggregate(&events).unwrap().is_none());
}

#[test]
fn every_counter_groups_into_its_own_pair() {
    let start = UsageSnapshot {
        agents: [(
            "a".to_string(),
            CapturedUsage {
                usage: CumulativeUsage {
                    prompt_tokens: 10,
                    completion_tokens: 2,
                    cache_read_tokens: 100,
                    cache_write_tokens: 30,
                },
                source: UsageSource::Disk,
            },
        )]
        .into_iter()
        .collect(),
        ..snap(&[])
    };
    let close = UsageSnapshot {
        agents: [(
            "a".to_string(),
            CapturedUsage {
                usage: CumulativeUsage {
                    prompt_tokens: 15,
                    completion_tokens: 5,
                    cache_read_tokens: 140,
                    cache_write_tokens: 70,
                },
                source: UsageSource::Disk,
            },
        )]
        .into_iter()
        .collect(),
        ..snap(&[])
    };
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "close", 20, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    let agent = &usage.agents[0];
    assert_eq!(agent.prompt_tokens, 5);
    assert_eq!(agent.completion_tokens, 3);
    assert_eq!(agent.cache_read_tokens, 40);
    assert_eq!(agent.cache_write_tokens, 40);
    assert_eq!(agent.consumed_tokens, 8);
    assert_eq!(agent.agent_role, None);
}

#[test]
fn an_out_of_order_trail_is_booked_not_panicked() {
    // A terminal whose baseline never appears in the trail: the window is
    // booked as a gap rather than read as if the terminal opened it.
    let close = snap(&[("a", 500, 0)]);
    let start = snap(&[("a", 600, 0)]);
    let events = vec![
        event(1, "close", 10, Some(&close)),
        event(2, "start", 20, Some(&start)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert!(usage.partial && usage.open);
    assert_eq!(usage.total_tokens, 0);
    assert_eq!(usage.gaps.len(), 1);
    assert_eq!(usage.gaps[0].segment, 1);
    assert_eq!(usage.gaps[0].reason, UsageGapReason::MissingBaseline);
}

#[test]
fn a_window_opened_over_an_open_one_books_the_abandoned_window() {
    // A second opening while one window is open is an inconsistent trail:
    // the abandoned window is booked as a gap, and so is every agent its
    // capture could not read; the gap names the window the trail actually
    // reached (the abandoned one), not the window the next terminal pairs
    // with.
    let mut start = snap(&[("a", 100, 0)]);
    start.skipped.push(SkippedAgent {
        agent_id: "b".into(),
        reason: "capture could not read the agent".into(),
    });
    let resume = snap(&[("a", 200, 0)]);
    let close = snap(&[("a", 300, 0)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "resume", 20, Some(&resume)),
        event(3, "close", 30, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert!(
        usage.partial && !usage.open,
        "an abandoned window makes the task partial"
    );
    assert_eq!(usage.gaps.len(), 2, "the unread agent is booked too");
    let booked: Vec<(&str, usize, UsageGapReason)> = usage
        .gaps
        .iter()
        .map(|gap| (gap.agent_id.as_str(), gap.segment, gap.reason))
        .collect();
    // Both gaps are MissingTerminal, the agent the capture could not read
    // included: a window abandoned by the next open never reaches a
    // terminal, so everything it carries is missing one. The paired path,
    // where a skip at open becomes MissingBaseline, does not apply here.
    // The choice is deliberate.
    assert_eq!(
        booked,
        vec![
            ("a", 1, UsageGapReason::MissingTerminal),
            ("b", 1, UsageGapReason::MissingTerminal),
        ],
        "the abandoned window books its agent and the one it could not read"
    );
    // The surviving window is the one the resume opened: its spend is the
    // resume -> close delta, and the abandoned window contributes nothing.
    assert_eq!(usage.segments.len(), 1);
    assert_eq!(usage.segments[0].from_event, "resume");
    assert_eq!(usage.segments[0].to_event, "close");
    assert_eq!(usage.total_tokens, 100);
}

#[test]
fn review_neither_opens_nor_closes_a_window() {
    // `review` moves the gate, not the work: the window it happens inside
    // stays one window, and its spend stays in that window's delta.
    let start = snap(&[("a", 100, 0)]);
    let close = snap(&[("a", 400, 0)]);
    let events = vec![
        event(1, "start", 10, Some(&start)),
        event(2, "review", 20, None),
        event(3, "close", 30, Some(&close)),
    ];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert_eq!(usage.total_tokens, 300);
    assert_eq!(usage.segments.len(), 1);
    assert_eq!(usage.segments[0].to_event, "close");
    assert!(usage.gaps.is_empty() && !usage.partial && !usage.open);
}

#[test]
fn an_unread_baseline_leaves_the_running_task_partial() {
    // The window is open and its baseline capture failed: the spend is
    // unmeasured, so the task must not read as a clean running task.
    let mut start = snap(&[("a", 10, 0)]);
    start.capture_error = Some("agent scan failed".to_string());
    let events = vec![event(1, "start", 10, Some(&start))];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert!(usage.open && usage.partial);
    assert_eq!(usage.total_tokens, 0);

    // The same shape when the capture ran but had to skip an agent.
    let mut start = snap(&[("a", 10, 0)]);
    start.skipped.push(SkippedAgent {
        agent_id: "b".to_string(),
        reason: "manifest unreadable".to_string(),
    });
    let events = vec![event(1, "start", 10, Some(&start))];
    let usage = aggregate(&events).unwrap().expect("usage");
    assert!(usage.open && usage.partial);
}

#[test]
fn payload_writers_leave_a_snapshotless_move_untouched() {
    // A move that captured nothing writes no payload, which keeps the trail
    // of a task that predates the feature byte-identical.
    assert!(transition_payload(None).unwrap().is_none());

    let mut payload = serde_json::json!({ "reason": "completed" });
    insert_snapshot(&mut payload, None).unwrap();
    assert_eq!(payload, serde_json::json!({ "reason": "completed" }));

    let captured = snap(&[("a", 1, 0)]);
    insert_snapshot(&mut payload, Some(&captured)).unwrap();
    assert_eq!(payload["reason"], "completed");
    let row = &payload[USAGE_SNAPSHOT_KEY]["agents"]["a"]["usage"];
    assert_eq!(row["prompt_tokens"], 1);
}
