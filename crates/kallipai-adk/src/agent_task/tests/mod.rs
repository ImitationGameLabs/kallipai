//! The tests for `agent_task`, one file per question its surface answers.

use super::context::AgentContext;
use super::rounds::run_and_report;
use super::wake::WakeDisposition;
use crate::event::AgentEvent;
use crate::lifecycle::LifecycleState;
use crate::retry::AttemptOrigin;
use kallipai_common::protocol::ParkedReason;

mod context;
mod fuses;
mod message_guard;
mod parks;
mod rounds;
mod task_loop;

/// Mount a short SSE stream under the loopback mock's failing default, so a
/// wake's round really sends and finishes: the drain tests need a round that
/// ends awake, not one that parks on the placeholder key's 401.
async fn mount_streaming_ok(server: &wiremock::MockServer) {
    let stream = "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n";
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(stream.as_bytes().to_vec(), "text/event-stream"),
        )
        .with_priority(1)
        .mount(server)
        .await;
}

/// Drive one gate-refused wake from `start` and assert the whole refusal:
/// no round runs, the park carries the gate's verdict, exactly one error
/// event reaches the channel, and the model is never asked.
async fn assert_gate_refusal_parks_from(from: &str, start: LifecycleState) {
    let (mut ctx, backend) = crate::test_support::ctx_with_unservable_image().await;
    *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()) = start;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    assert_eq!(
        run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
        WakeDisposition::Held,
        "{from}: a blocked wake parks instead of starting a round"
    );
    // The wake boundary is where the count lands. A refusal is a failed
    // wake, so the park above and this count tell one story.
    let snapshot = ctx.metrics.snapshot().expect("local tallies");
    assert_eq!(
        snapshot
            .wakes_total
            .iter()
            .find(|(outcome, _)| *outcome == "failed")
            .map(|(_, count)| *count),
        Some(1),
        "{from}: a refused wake is counted once, as failed"
    );
    assert!(
        snapshot
            .wakes_total
            .iter()
            .all(|(outcome, count)| *outcome == "failed" || *count == 0),
        "{from}: the same wake is not counted under another outcome"
    );
    let parked = ctx
        .lifecycle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match parked {
        LifecycleState::Parked {
            reason: ParkedReason::FatalError { message },
            ..
        } => assert!(
            message.contains("cannot serve the restored context"),
            "{from}: the park carries the gate's verdict: {message}"
        ),
        other => panic!("{from}: expected a FatalError park, got {other:?}"),
    }
    let events: Vec<AgentEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    match events.as_slice() {
        [AgentEvent::Error(text)] => assert!(
            text.contains("cannot serve the restored context"),
            "{from}: the wake surfaces the refusal to the caller: {text}"
        ),
        other => panic!("{from}: expected exactly one Error event, got {other:?}"),
    }
    assert!(
        backend.take_requests().is_empty(),
        "{from}: the blocked wake never reaches the model"
    );
}

/// An over-budget context whose summarizer answers `400` — the class table's
/// terminal class — so the pre-loop wrapper reaches a stop.
async fn stop_pin_ctx() -> (AgentContext, wiremock::MockServer) {
    let (source, server) = crate::test_support::mock_source(&["ep1"], 400).await;
    let mut ctx = crate::test_support::ctx_from_source(
        vec![crate::test_support::profile("p1", "ep1", 500_000)],
        source,
        crate::retry::RetryPolicy::default(),
    )
    .await;
    crate::test_support::set_context_window(&mut ctx, crate::test_support::FAILURE_PIN_WINDOW)
        .await;
    crate::test_support::seed_over_budget_store(&mut ctx).await;
    // One threshold leaves no progressive-warning level above the trigger, so
    // the gate reaches the compaction branch on its first call.
    ctx.config.context_thresholds = vec![80];
    (ctx, server)
}

/// How many records the history log holds: one line per appended record.
fn history_lines(dir: &std::path::Path) -> usize {
    let mut lines = 0;
    for entry in std::fs::read_dir(dir.join("history")).expect("the history dir exists") {
        let path = entry.expect("a readable entry").path();
        let text = std::fs::read_to_string(path).expect("a readable history file");
        lines += text.lines().filter(|line| !line.is_empty()).count();
    }
    lines
}

/// Every record the history log holds, in file order.
fn history_records(dir: &std::path::Path) -> Vec<crate::history::HistoryRecord> {
    let mut records = Vec::new();
    for entry in std::fs::read_dir(dir.join("history")).expect("the history dir exists") {
        let path = entry.expect("a readable entry").path();
        let text = std::fs::read_to_string(path).expect("a readable history file");
        for line in text.lines().filter(|line| !line.is_empty()) {
            records.push(serde_json::from_str(line).expect("a parsable history line"));
        }
    }
    records
}
