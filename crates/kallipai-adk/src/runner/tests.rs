use super::*;
use crate::context::{AgenticContext as _, KeepCaps};
use crate::test_support::{TurnMessage, tool_calls_msg, tool_result_msg, user_msg};
use std::collections::HashMap;
use std::sync::Arc;

use just_llm_client::{
    LlmBackend,
    types::generation::{Message, Reasoning, ToolCall},
};
use kallipai_common::protocol::FailoverChainExhaustion;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

use crate::acquisition::advance_failover;
use crate::agent_task::{RoundToken, WakeDisposition, run_and_report};
use crate::failover::{FailoverOutcome, ProfileReset};
use crate::policy::ToolCallOutcome;
use crate::profile::{BackendSource, ProfileRegistry, ProfileSet};
use crate::retry::AttemptOrigin;
use crate::runner::BreakUntil;
use crate::test_support::{
    FAILURE_PIN_WINDOW, GatedTool, MapSource, ctx_from_source, ctx_from_source_with_tools,
    make_ctx, profile, seed_over_budget_store, set_context_window,
};
use crate::tool_execution::{run_tool_bounded, synthesize_unanswered_results};
use crate::tools::DEFAULT_BREAK_TIMEOUT_SECS;

fn no_cancel() -> CancellationToken {
    CancellationToken::new()
}

/// Drain `rx` until `WaitCancelled`, failing if the park form ever arrives as a
/// round interrupt or the fuse never fires. `what` names the park under test, so
/// a failure says which one.
async fn expect_wait_cancelled(rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>, what: &str) {
    let mut seen = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !seen {
        if let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {
            // The park form of the interrupt: no round was in flight, so the
            // event must say the wait ended and never claim a cut-short turn.
            assert!(
                !matches!(event, AgentEvent::Interrupted),
                "{what}: a cancelled park must not report a round interrupt: {event:?}"
            );
            seen = matches!(event, AgentEvent::WaitCancelled);
        }
    }
    assert!(
        seen,
        "{what}: the cancel must settle the agent with WaitCancelled"
    );
}

// --- synthesize_unanswered_results unit tests ---

fn result_ids(msgs: &[TurnMessage]) -> Vec<&str> {
    msgs.iter().filter_map(|m| m.tool_call_id()).collect()
}

#[test]
fn synthesize_answers_break_and_trailing_calls() {
    // [break, bash_exec]: the loop returns at break, so neither ran — break gets
    // its success ack, bash_exec gets an honest not-executed result.
    let mut msgs = vec![tool_calls_msg(&[("c1", "break"), ("c2", "bash_exec")])];
    synthesize_unanswered_results(
        &mut msgs,
        BreakUntil::Wait {
            timeout_secs: DEFAULT_BREAK_TIMEOUT_SECS,
        },
    );
    assert_eq!(result_ids(&msgs), vec!["c1", "c2"]);
    assert!(
        msgs[1].content().unwrap().contains("parked"),
        "break result should be its success ack: {}",
        msgs[1].content().unwrap()
    );
    assert!(
        msgs[2].content().unwrap().contains("not executed"),
        "trailing call should be not-executed: {}",
        msgs[2].content().unwrap()
    );
}

#[test]
fn synthesize_preserves_existing_results() {
    // [bash_exec, break]: bash_exec already ran and has a result; only break is filled.
    let mut msgs = vec![
        tool_calls_msg(&[("c1", "bash_exec"), ("c2", "break")]),
        tool_result_msg("done", "c1"),
    ];
    synthesize_unanswered_results(
        &mut msgs,
        BreakUntil::Wait {
            timeout_secs: DEFAULT_BREAK_TIMEOUT_SECS,
        },
    );
    assert_eq!(result_ids(&msgs), vec!["c1", "c2"]);
    assert_eq!(msgs[1].content().unwrap(), "done");
    assert!(msgs[2].content().unwrap().contains("parked"));
}

#[test]
fn synthesize_is_noop_on_complete_turn_and_idempotent() {
    let mut msgs = vec![
        tool_calls_msg(&[("c1", "bash_exec")]),
        tool_result_msg("done", "c1"),
    ];
    synthesize_unanswered_results(
        &mut msgs,
        BreakUntil::Wait {
            timeout_secs: DEFAULT_BREAK_TIMEOUT_SECS,
        },
    );
    assert_eq!(msgs.len(), 2, "a fully-answered turn is untouched");
    // Running again over the completed turn must not duplicate anything.
    let before = msgs.clone();
    synthesize_unanswered_results(
        &mut msgs,
        BreakUntil::Wait {
            timeout_secs: DEFAULT_BREAK_TIMEOUT_SECS,
        },
    );
    assert_eq!(result_ids(&msgs), result_ids(&before));
    assert_eq!(msgs.len(), before.len());
}

#[test]
fn synthesize_keeps_assistant_content_and_reasoning_intact() {
    let mut msgs = vec![Message::assistant_tool_calls(
        Some("parking now".into()),
        vec![ToolCall {
            id: "c1".into(),
            name: "break".into(),
            arguments: "{}".into(),
        }],
        Some(Reasoning {
            text: Some("done thinking".into()),
            ..Reasoning::default()
        }),
    )];
    synthesize_unanswered_results(
        &mut msgs,
        BreakUntil::Wait {
            timeout_secs: DEFAULT_BREAK_TIMEOUT_SECS,
        },
    );
    assert_eq!(msgs[0].content(), Some("parking now"));
    assert_eq!(
        msgs[0].reasoning().and_then(|r| r.text.as_deref()),
        Some("done thinking")
    );
    assert_eq!(msgs[1].tool_call_id(), Some("c1"));
}

// --- advance_failover unit tests (fast, no wiremock; summarize_and_evict no-ops on empty store) ---

#[tokio::test]
async fn advance_lands_on_next_buildable() {
    let profiles = vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)];
    let mut ctx = make_ctx(profiles, &["ep1", "ep2"]).await;
    let before_window = ctx.config.context_window_tokens;

    let outcome = advance_failover(
        &mut ctx,
        vec![],
        anyhow::anyhow!("trigger"),
        &no_cancel(),
        0,
    )
    .await;

    match outcome {
        FailoverOutcome::Advanced { from, to, messages } => {
            assert_eq!(from, "p1");
            assert_eq!(to, "p2");
            assert_eq!(ctx.failover.profile_idx(), 1);
            assert_eq!(ctx.client.model(), "p2-model");
            assert!(
                messages.is_empty(),
                "no compaction → prior (empty) messages returned"
            );
        }
        other => panic!("expected Advanced, got {other:?}"),
    }
    // p2 carries the same window as the config default → unchanged after advance.
    assert_eq!(ctx.config.context_window_tokens, before_window);
}

#[tokio::test]
async fn advance_chain_exhausted_single_profile() {
    let mut ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
    let outcome = advance_failover(
        &mut ctx,
        vec![],
        anyhow::anyhow!("trigger"),
        &no_cancel(),
        0,
    )
    .await;
    match outcome {
        FailoverOutcome::ChainExhausted {
            reason: FailoverChainExhaustion::NoFailoverConfigured,
            ..
        } => {}
        other => panic!("expected ChainExhausted(NoFailoverConfigured), got {other:?}"),
    }
    assert_eq!(
        ctx.failover.profile_idx(),
        0,
        "index must not advance on exhaustion"
    );
}

#[tokio::test]
async fn advance_cancelled_when_round_cancelled() {
    let mut ctx = make_ctx(
        vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)],
        &["ep1", "ep2"],
    )
    .await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcome = advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &cancel, 0).await;
    assert!(matches!(outcome, FailoverOutcome::Cancelled));
    assert_eq!(
        ctx.failover.profile_idx(),
        0,
        "index must not advance on cancel"
    );
}

#[tokio::test]
async fn advance_skips_unbuildable_lands_on_next() {
    // [p1(ep1), p2(ep2-missing), p3(ep3)] → skip p2, land on p3.
    let profiles = vec![
        profile("p1", "ep1", 500_000),
        profile("p2", "ep2", 500_000),
        profile("p3", "ep3", 500_000),
    ];
    let mut ctx = make_ctx(profiles, &["ep1", "ep3"]).await;
    let outcome = advance_failover(
        &mut ctx,
        vec![],
        anyhow::anyhow!("trigger"),
        &no_cancel(),
        0,
    )
    .await;
    match outcome {
        FailoverOutcome::Advanced { from, to, .. } => {
            assert_eq!(from, "p1");
            assert_eq!(to, "p3", "p2 is unbuildable and must be skipped");
            assert_eq!(ctx.failover.profile_idx(), 2);
            assert_eq!(ctx.client.model(), "p3-model");
        }
        other => panic!("expected Advanced, got {other:?}"),
    }
}

#[tokio::test]
async fn advance_all_unbuildable_chain_exhausted() {
    // [p1(ep1), p2(ep2-missing)] → p2 unbuildable, no further candidate → ChainExhausted,
    // index unchanged, client still p1's (never left on an unbuilt profile).
    let mut ctx = make_ctx(
        vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)],
        &["ep1"],
    )
    .await;
    let outcome = advance_failover(
        &mut ctx,
        vec![],
        anyhow::anyhow!("trigger"),
        &no_cancel(),
        0,
    )
    .await;
    assert!(
        matches!(
            outcome,
            FailoverOutcome::ChainExhausted {
                reason: FailoverChainExhaustion::AllCandidatesUnbuildable,
                ..
            }
        ),
        "expected ChainExhausted(AllCandidatesUnbuildable), got {outcome:?}"
    );
    assert_eq!(ctx.failover.profile_idx(), 0, "index must not advance");
    assert_eq!(
        ctx.client.model(),
        "p1-model",
        "client must remain the active profile's"
    );
}

#[tokio::test]
async fn advance_reapplies_smaller_window() {
    // p2 declares a smaller but valid window → config window tracks it after advance.
    let profiles = vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 100_000)];
    let mut ctx = make_ctx(profiles, &["ep1", "ep2"]).await;
    let outcome = advance_failover(
        &mut ctx,
        vec![],
        anyhow::anyhow!("trigger"),
        &no_cancel(),
        0,
    )
    .await;
    assert!(matches!(outcome, FailoverOutcome::Advanced { .. }));
    assert_eq!(ctx.config.context_window_tokens, 100_000);
    assert_eq!(ctx.failover.profile_idx(), 1);
}

#[tokio::test]
async fn advance_skips_window_that_violates_invariant() {
    // [p1(500k), p2(10k infeasible), p3(500k)] all buildable → p2's window violates the budget
    // shape (summary_max > pinned at 10k) so it is skipped pre-advance; failover lands on p3.
    let profiles = vec![
        profile("p1", "ep1", 500_000),
        profile("p2", "ep2", 10_000),
        profile("p3", "ep3", 500_000),
    ];
    let mut ctx = make_ctx(profiles, &["ep1", "ep2", "ep3"]).await;

    let outcome = advance_failover(
        &mut ctx,
        vec![],
        anyhow::anyhow!("trigger"),
        &no_cancel(),
        0,
    )
    .await;

    match outcome {
        FailoverOutcome::Advanced { from, to, messages } => {
            assert_eq!(from, "p1");
            assert_eq!(to, "p3", "infeasible p2 is skipped, lands on p3");
            assert_eq!(ctx.failover.profile_idx(), 2);
            assert_eq!(ctx.client.model(), "p3-model");
            assert!(messages.is_empty());
        }
        other => panic!("expected Advanced (p2 skipped → p3), got {other:?}"),
    }
    assert_ne!(
        ctx.config.context_window_tokens, 10_000,
        "p2's infeasible window must never be adopted"
    );
}

#[tokio::test]
async fn advance_all_infeasible_chain_exhausted() {
    // [p1(500k), p2(10k infeasible)] → p2 builds but its window violates the budget shape, is
    // skipped pre-advance, and no candidate remains → ChainExhausted(AllCandidatesInfeasible).
    // Index unchanged, client stays p1's (never swapped to an infeasible-window profile).
    let mut ctx = make_ctx(
        vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 10_000)],
        &["ep1", "ep2"],
    )
    .await;
    let before = ctx.config.context_window_tokens;
    let outcome = advance_failover(
        &mut ctx,
        vec![],
        anyhow::anyhow!("trigger"),
        &no_cancel(),
        0,
    )
    .await;
    assert!(
        matches!(
            outcome,
            FailoverOutcome::ChainExhausted {
                reason: FailoverChainExhaustion::AllCandidatesInfeasible,
                ..
            }
        ),
        "expected ChainExhausted(AllCandidatesInfeasible), got {outcome:?}"
    );
    assert_eq!(ctx.failover.profile_idx(), 0, "index must not advance");
    assert_eq!(
        ctx.client.model(),
        "p1-model",
        "client stays on the active profile (never swapped to p2)"
    );
    assert_eq!(
        ctx.config.context_window_tokens, before,
        "infeasible window never adopted"
    );
}

// --- run_agent_rounds integration tests (wiremock, one MockServer per profile) ---

/// Fast retry policy so the wiremock suite stays snappy.
fn fast_policy() -> crate::retry::RetryPolicy {
    crate::retry::RetryPolicy {
        max_retries: 2,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
        retry_timeout: Duration::from_secs(10),
    }
}

/// A real OpenAI-compatible backend pointed at `uri` (a wiremock server).
fn wiremock_backend(uri: &str) -> Arc<dyn LlmBackend> {
    just_llm_client::provider::OpenAiCompatBackend::new(
        reqwest::Client::builder().use_rustls_tls(),
        "test-key",
        Some(uri),
    )
    .expect("openai-compat backend constructs without network")
}

async fn mount_status(server: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

/// Mount a 200 streaming response carrying `content` (no tool calls → `BareAssistant`).
async fn mount_ok_stream(server: &MockServer, content: &str) {
    let body = format!(
        "data: {{\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\ndata: [DONE]\n"
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// The marker every summarizer request body carries: `ContextSummarizer::prompt` rides
/// the request as a user message, and a round's own requests never carry it.
const SUMMARY_MARKER: &str = "compaction-probe-marker-3f19";

/// Point `ctx`'s summarizer at [`SUMMARY_MARKER`].
fn mark_summarizer(ctx: &mut AgentContext) {
    ctx.summarizer.prompt = SUMMARY_MARKER.to_owned();
}

/// Mount the summarizer's answer: `status` for any request whose body carries the
/// marker. The summary mock is the narrower one (body marker) and outranks the
/// catch-all `mount_ok_stream` (priority 1 against its default 5), so mount order
/// never decides which mock answers.
async fn mount_summary_status(server: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(SUMMARY_MARKER))
        .respond_with(ResponseTemplate::new(status))
        .with_priority(1)
        .mount(server)
        .await;
}

/// What the server saw, split by kind: `(summary, round)` counts.
async fn request_counts(server: &MockServer) -> (usize, usize) {
    let requests = server.received_requests().await.unwrap_or_default();
    let summary = requests
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains(SUMMARY_MARKER))
        .count();
    (summary, requests.len() - summary)
}

/// A round context over `window`: `context_thresholds = vec![80]` drops the progressive
/// warning level, so a single round reaches compaction instead of re-composing. The
/// summarizer answers `400` while the round's own requests stream normally.
async fn round_ctx(server: &MockServer, window: usize) -> AgentContext {
    round_ctx_with_policy(server, window, fast_policy()).await
}

/// The same shape over `policy`: a compaction failure's verdict reads the retry
/// policy, so a pin about the backoff it arms needs its own.
async fn round_ctx_with_policy(
    server: &MockServer,
    window: usize,
    policy: crate::retry::RetryPolicy,
) -> AgentContext {
    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let mut ctx = ctx_from_source(
        vec![profile("p1", "ep1", 500_000)],
        wiremock_source(map),
        policy,
    )
    .await;
    mark_summarizer(&mut ctx);
    ctx.config.context_thresholds = vec![80];
    set_context_window(&mut ctx, window).await;
    ctx
}

/// The over-budget shape: the seeded pair exceeds [`FAILURE_PIN_WINDOW`], so the round
/// reaches compaction and the summarizer call fails there.
async fn failing_round_ctx(server: &MockServer) -> AgentContext {
    let mut ctx = round_ctx(server, FAILURE_PIN_WINDOW).await;
    seed_over_budget_store(&mut ctx).await;
    ctx
}

/// Mount a 200 streaming response that emits a single `break` tool call (no
/// content). Drives the round loop's `break` short-circuit path.
async fn mount_break_stream(server: &MockServer) {
    let body = "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"break\",\"arguments\":\"{}\"}}]}}]}\n\ndata: [DONE]\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body.to_owned().into_bytes(), "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// Mount a 200 streaming response that emits a single `break` tool call with
/// the given raw `arguments` JSON (no content). Drives the `break(wait)` /
/// `break(idle)` park-target paths.
async fn mount_break_args_stream(server: &MockServer, args: &str) {
    // `arguments` is a JSON string inside the SSE JSON — its quotes must be
    // escaped, or the tool call fails to parse and no `break` happens.
    let escaped = args.replace('"', "\\\"");
    // Same body as `mount_break_stream` with the arguments swapped in via
    // replace (not format!) so the JSON braces need no doubling.
    let body = "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"break\",\"arguments\":\"@ARGS@\"}}]}}]}\n\ndata: [DONE]\n"
        .replace("@ARGS@", &escaped);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// Build a 200 SSE body carrying one tool call for `name` with the raw
/// `arguments` JSON. The quotes inside `arguments` are escaped for the SSE
/// envelope, or the call fails to parse.
fn tool_call_sse_body(name: &str, args: &str) -> String {
    let escaped = args.replace('"', "\\\"");
    "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"@NAME@\",\"arguments\":\"@ARGS@\"}}]}}]}\n\ndata: [DONE]\n"
        .replace("@NAME@", name)
        .replace("@ARGS@", &escaped)
}

/// A `MapSource` mapping endpoint id → wiremock backend.
fn wiremock_source(map: HashMap<String, Arc<dyn LlmBackend>>) -> Arc<dyn BackendSource> {
    Arc::new(MapSource(map))
}

/// Drive one `run_agent_rounds`: seed a user turn, mint a round token, run, collect events.
async fn run_rounds(ctx: &mut AgentContext) -> (Result<RoundOutcome>, Vec<AgentEvent>) {
    ctx.record_turn(vec![user_msg("respond with the single word: done")])
        .await;
    let round = RoundToken::new(&ctx.cancel);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let outcome = run_agent_rounds(ctx, &tx, &mut prompt_rx, round.handle()).await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (outcome, events)
}

fn failover_hops(events: &[AgentEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Failover { from, to, .. } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect()
}

/// The `(attempt, max_attempts)` carried by each `StreamReset` event, in order.
fn stream_resets(events: &[AgentEvent]) -> Vec<(u32, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StreamReset {
                attempt,
                max_attempts,
                ..
            } => Some((*attempt, *max_attempts)),
            _ => None,
        })
        .collect()
}

/// Serialize one SSE `data:` line as an HTTP/1.1 chunked-transfer chunk.
fn write_chunk<W: std::io::Write>(w: &mut W, data: &[u8]) {
    let _ = write!(w, "{:x}\r\n", data.len());
    let _ = w.write_all(data);
    let _ = w.write_all(b"\r\n");
}

/// Build an SSE chunk carrying a single content delta.
fn sse_delta(content: &str) -> Vec<u8> {
    format!(
            "data: {{\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n"
        )
        .into_bytes()
}

/// A raw `std::net` server that, on the first request, streams one content delta then closes
/// the socket **without** the terminating zero-length chunk — so hyper/reqwest surfaces a body
/// decode error mid-stream (the exact transport drop this feature recovers from). On the second
/// request it returns a complete, properly-terminated stream carrying `retry_content`.
///
/// wiremock cannot reproduce this: it only emits complete bodies (a clean EOF reads as `None`,
/// not `Err`), so a real truncated-chunk connection is required.
fn dropping_then_ok_server(retry_content: &str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let retry = retry_content.to_owned();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        // More than the nominal 2 connects: under a loaded runner the
        // transport can burn a spare connect on a pooled-connection reuse
        // race, and refusing it flips the outcome to ChainExhausted. The
        // first connect drops mid-stream; every later one gets the
        // complete retry stream. (The thread parks in accept afterwards —
        // fine for a test process.)
        let mut attempt = 0u32;
        while let Ok((mut socket, _)) = listener.accept() {
            let first = attempt == 0;
            attempt += 1;
            // Best-effort drain of the request (the small JSON body); HTTP is full-duplex so
            // writing the response does not depend on fully reading the request.
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let _ = socket.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
                );
            if first {
                write_chunk(&mut socket, &sse_delta("PARTIAL"));
                // Drop mid-stream: close without the terminating zero-length chunk.
                let _ = socket.shutdown(std::net::Shutdown::Both);
            } else {
                write_chunk(&mut socket, &sse_delta(&retry));
                write_chunk(&mut socket, b"data: [DONE]\n\n");
                let _ = socket.write_all(b"0\r\n\r\n");
                // Graceful close: half-close the write side, then drain until
                // the peer closes. Dropping with unread data sends RST and can
                // swallow the response under load (observed as a phantom
                // second stream reset).
                let _ = socket.shutdown(std::net::Shutdown::Write);
                let _ = socket.set_read_timeout(Some(std::time::Duration::from_millis(200)));
                let _ = socket.read_to_end(&mut Vec::new());
            }
        }
    });
    format!("http://{addr}")
}

/// A raw server that drops mid-stream on every request (for budget-exhaustion tests).
fn always_dropping_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        while let Ok((mut socket, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let _ = socket.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
                );
            write_chunk(&mut socket, &sse_delta("PARTIAL"));
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn failover_primary_down_backup_succeeds() {
    let primary = MockServer::start().await;
    let backup = MockServer::start().await;
    mount_status(&primary, 500).await; // exhausts retries → Failover
    mount_ok_stream(&backup, "done").await; // 200 → BareAssistant

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&primary.uri()));
    map.insert("ep2".into(), wiremock_backend(&backup.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (outcome, events) = run_rounds(&mut ctx).await;

    let content = match outcome {
        Ok(RoundOutcome::BareAssistant { content, .. }) => content,
        _ => panic!("expected BareAssistant"),
    };
    assert_eq!(content, "done");
    assert_eq!(
        ctx.failover.profile_idx(),
        1,
        "should have failed over to backup"
    );
    assert_eq!(
        failover_hops(&events),
        vec![("p1".to_string(), "p2".to_string())],
        "exactly one failover p1→p2"
    );
}

/// A pending profile reset (what the tagma's bind/apply writes into the
/// shared cell) is drained at the top of the next `run_and_report`: the
/// switch takes effect on that very request, and the failover chain is
/// rebuilt from the new set's head — not resumed at the old index.
#[tokio::test]
async fn switch_takes_effect_next_request_and_resets_failover_chain() {
    let server = MockServer::start().await;
    mount_break_stream(&server).await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p0", "ep1", 500_000), profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map.clone()), fast_policy()).await;
    // Advance so the active index is 1 — the switch must reset to the head.
    ctx.failover.advance_to(1);
    assert_eq!(ctx.failover.profile_idx(), 1);

    // The pending reset the tagma would write: a one-profile "fresh" set.
    let fresh = ProfileSet {
        name: "fresh".into(),
        description: None,
        profiles: vec![profile("q0", "ep1", 500_000)],
    };
    let sets = std::collections::BTreeMap::from([("fresh".to_string(), fresh.clone())]);
    let registry = Arc::new(ProfileRegistry::new(sets, wiremock_source(map)).unwrap());
    // Stale values: the reset must re-install both from the fresh set, not
    // leave the store on what it held before the switch.
    {
        let mut store = ctx.store.lock().await;
        store.set_pinned_budget(1);
        store.set_eviction_caps(KeepCaps::new(0, 0));
    }
    *ctx.pending_profile_reset.lock().unwrap() = Some(ProfileReset {
        set: fresh,
        registry,
    });

    ctx.record_turn(vec![user_msg("switch time")]).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let disposition = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;
    assert_ne!(disposition, WakeDisposition::Terminated);
    let _ = rx.try_recv();

    // The chain was rebuilt from the fresh set's head.
    assert_eq!(ctx.failover.profile_idx(), 0, "reset must land on the head");
    let snapshot = ctx.failover.profile_snapshot();
    assert_eq!(snapshot.set_name, "fresh");
    assert_eq!(snapshot.profile_id, "q0");
    // The cell is drained — a second wake must not re-apply.
    assert!(ctx.pending_profile_reset.lock().unwrap().is_none());
    {
        let store = ctx.store.lock().await;
        assert_eq!(
            store.eviction_caps(),
            ctx.config.eviction_caps(),
            "the reset re-installs the caps for the fresh window"
        );
        assert_eq!(
            store.pinned_budget(),
            ctx.config.pinned_budget(),
            "and the pinned layer's capacity with them"
        );
    }
}

/// A pending profile reset swaps the endpoint, so a stop the pre-loop pass
/// parked for the old endpoint no longer answers for the context the new one
/// would send: the round starts the ledger over and asks the new endpoint's
/// summarizer, instead of ending on the verdict of the endpoint just left.
#[tokio::test]
async fn a_profile_reset_drops_the_stop_the_old_endpoint_parked() {
    let (source, server) = crate::test_support::mock_source(&["ep1"], 400).await;
    let profiles = vec![profile("p0", "ep1", 500_000)];
    let mut ctx = ctx_from_source(
        profiles,
        source.clone(),
        crate::retry::RetryPolicy::default(),
    )
    .await;
    set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
    seed_over_budget_store(&mut ctx).await;
    // One threshold leaves no progressive-warning level above the trigger, so
    // the gate reaches the compaction branch on its first call.
    ctx.config.context_thresholds = vec![80];

    // The pre-loop pass meets the endpoint failure and parks its stop.
    crate::context::compact_pre_loop(&mut ctx).await;
    assert_eq!(ctx.compaction.attempts(), 1, "the wrapper charged the send");

    // The pending reset the tagma's bind writes: a one-profile "fresh" set. Its
    // window matches the one the context is over budget for, so the round still
    // reaches compaction and the only thing that moved is the endpoint that owns
    // the verdict.
    let fresh = ProfileSet {
        name: "fresh".into(),
        description: None,
        profiles: vec![profile("q0", "ep1", FAILURE_PIN_WINDOW)],
    };
    let sets = std::collections::BTreeMap::from([("fresh".to_string(), fresh.clone())]);
    let registry = Arc::new(ProfileRegistry::new(sets, source).unwrap());
    *ctx.pending_profile_reset.lock().unwrap() = Some(ProfileReset {
        set: fresh,
        registry,
    });

    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    assert_ne!(
        run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::External).await,
        WakeDisposition::Terminated,
        "the round's own failure parks it"
    );

    assert_eq!(
        ctx.failover.profile_snapshot().profile_id,
        "q0",
        "the reset landed on the fresh set"
    );
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        2,
        "the reset drops the parked stop: the round asks the summarizer once"
    );
    let parked = ctx
        .lifecycle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(
        matches!(
            parked,
            crate::lifecycle::LifecycleState::Parked {
                reason: kallipai_common::protocol::ParkedReason::FatalError { .. },
                ..
            }
        ),
        "the round ends parked on its own compaction verdict"
    );
}

#[tokio::test]
async fn break_tool_call_yields_break_outcome() {
    // A `break` tool call short-circuits the round: the outcome is `Break`
    // (not BareAssistant, not a continued tool round). This pins the
    // contract — break terminates the round via the hoisted
    // name-check in execute_tool_calls.
    let server = MockServer::start().await;
    mount_break_stream(&server).await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (outcome, _events) = run_rounds(&mut ctx).await;
    assert!(
        matches!(outcome, Ok(RoundOutcome::Break(_))),
        "expected RoundOutcome::Break, got {outcome:?}"
    );
}

/// `break()` with no args is `break(wait)` with the default fuse: the turn
/// parks as *Waiting* — a Waiting terminal event, the lifecycle state
/// carrying the armed deadline, and an actually-armed timer.
#[tokio::test]
async fn break_wait_parks_waiting_with_armed_timer() {
    let server = MockServer::start().await;
    mount_break_stream(&server).await; // break with `{}` → default wait

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let disposition = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;
    assert_ne!(disposition, WakeDisposition::Terminated);

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::Waiting { timeout_secs: 600 })),
        "expected a Waiting(600s) terminal event, got {events:?}"
    );
    assert!(
        !events.iter().any(|ev| matches!(ev, AgentEvent::Idle)),
        "break(wait) must not emit Idle: {events:?}"
    );
    match &*ctx.lifecycle.lock().unwrap() {
        crate::lifecycle::LifecycleState::Waiting { .. } => {}
        other => panic!("expected lifecycle Waiting, got {other:?}"),
    }
    assert!(
        ctx.wait_until.lock().unwrap().is_some(),
        "the wait timer must be armed"
    );
    assert_eq!(ctx.wait_armed_secs, 600);
}

/// `break(until:"idle")` parks as Idle for good: the Idle terminal event,
/// lifecycle Idle, and no armed timer.
#[tokio::test]
async fn break_idle_parks_idle_without_timer() {
    let server = MockServer::start().await;
    mount_break_args_stream(&server, "{\"until\":\"idle\"}").await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let disposition = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;
    assert_ne!(disposition, WakeDisposition::Terminated);

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(
        events.iter().any(|ev| matches!(ev, AgentEvent::Idle)),
        "expected Idle terminal event, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::Waiting { .. })),
        "break(idle) must not emit Waiting: {events:?}"
    );
    assert_eq!(
        *ctx.lifecycle.lock().unwrap(),
        crate::lifecycle::LifecycleState::Idle
    );
    assert!(ctx.wait_until.lock().unwrap().is_none());
}

/// Full-loop pin: `break(wait)` with a 1s fuse parks Waiting; when the fuse
/// elapses, the outer loop injects the `[system] wait timer elapsed` turn
/// and a real round runs on it (the model re-answers — here by parking
/// again). The timer never auto-runs work: the injected turn is the only
/// thing between elapse and the model.
#[tokio::test]
async fn wait_timer_elapse_injects_system_turn_and_reruns() {
    let server = MockServer::start().await;
    mount_break_args_stream(&server, "{\"timeout_secs\":1}").await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;
    let store = ctx.store.clone();

    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));

    // Drive one round; it ends in break(wait, 1s).
    ptx.send("go".into()).await.unwrap();

    // Poll until the elapsed-turn lands in the store (real 1s fuse).
    let injected = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let guard = store.lock().await;
            let found = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
                m.content()
                    .map(|t| t.contains("wait timer elapsed (armed 1s)"))
                    .unwrap_or(false)
            });
            drop(guard);
            if found {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(injected.is_ok(), "the elapsed [system] turn never landed");

    // A real round ran on the injected turn: the model answered (the mock
    // saw a second request), and the answer parks Waiting again.
    let requests = server.received_requests().await.unwrap().len();
    assert!(
        requests >= 2,
        "no round ran after the injection ({requests} requests)"
    );
}

/// Full-loop pin of the interrupt-on-a-park form: the cancel drops the armed
/// fuse and settles the agent with `WaitCancelled` without running a round, and
/// the old timer, when it reaches its deadline, must not inject its turn
/// either.
#[tokio::test]
async fn interrupt_cancels_the_armed_wait_and_the_fuse_stays_dead() {
    let server = MockServer::start().await;
    mount_break_args_stream(&server, "{\"timeout_secs\":1}").await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;
    let store = ctx.store.clone();
    let wait_until = ctx.wait_until.clone();
    let park_epoch = ctx.park_epoch.clone();
    let park_cancel = ctx.park_cancel.clone();
    let notify = ctx.notify.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));
    ptx.send("go".into()).await.unwrap();

    // The round ends in break(wait, 1s): the park is armed and mirrored.
    let armed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(Some(AgentEvent::Waiting { .. })) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            {
                return;
            }
        }
    })
    .await;
    assert!(armed.is_ok(), "the round must park on the wait fuse");

    // The same producer the interrupt route calls: stamp the generation it
    // reads, then wake the loop.
    crate::agent_task::request_park_cancel(&park_epoch, &park_cancel, &notify);

    expect_wait_cancelled(&mut rx, "break(wait) fuse").await;
    assert!(
        wait_until.lock().unwrap().is_none(),
        "a cancelled park must leave no fuse armed"
    );

    // Sleep well past the old 1s fuse: no injected turn may land, and no
    // second model call may run.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    {
        let guard = store.lock().await;
        let injected = guard
            .turns()
            .iter()
            .flat_map(|turn| &turn.messages)
            .any(|m| {
                m.content()
                    .map(|text| text.contains("wait timer elapsed"))
                    .unwrap_or(false)
            });
        assert!(!injected, "the dropped fuse must not inject its turn");
    }
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "the cancel must not start a round"
    );
    handle.abort();
}

/// The park generation's reason to exist. A cancel stamped while a round is
/// in flight names the park that round is about to leave behind, not the one
/// it is about to arm: the cell must die on the mismatch instead of cancelling
/// a wait nobody interrupted.
///
/// The round is held inside a gated tool, so the stamp provably happens mid-
/// round (the test awaits the tool's entry signal) rather than by timing.
#[tokio::test]
async fn a_cancel_stamped_mid_round_never_cancels_the_next_park() {
    let server = MockServer::start().await;
    let gate = GatedTool::new("hold");
    let entered = gate.entered.clone();
    let release = gate.release.clone();
    let _gated = Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    tool_call_sse_body("hold", "{}").into_bytes(),
                    "text/event-stream",
                ),
        )
        .up_to_n_times(1)
        .mount_as_scoped(&server)
        .await;
    mount_break_args_stream(&server, "{\"timeout_secs\":30}").await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source_with_tools(
        profiles,
        wiremock_source(map),
        fast_policy(),
        vec![Box::new(gate)],
    )
    .await;
    // Room for the tool round plus the round that parks: the fixture's
    // default budget is a single tool round.
    ctx.config.max_tool_rounds = 4;
    let wait_until = ctx.wait_until.clone();
    let park_epoch = ctx.park_epoch.clone();
    let park_cancel = ctx.park_cancel.clone();
    let notify = ctx.notify.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));
    ptx.send("go".into()).await.unwrap();

    // Provably mid-round: the round is inside the gated tool's call.
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .expect("the round must reach the gated tool");
    crate::agent_task::request_park_cancel(&park_epoch, &park_cancel, &notify);
    release.notify_one();

    // The round finishes in break(wait, 30s) and mirrors the new park.
    let mut seen = Vec::new();
    let parked = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let Ok(Some(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            else {
                continue;
            };
            let waiting = matches!(event, AgentEvent::Waiting { .. });
            seen.push(event);
            if waiting {
                return;
            }
        }
    })
    .await;
    assert!(
        parked.is_ok(),
        "the round must re-park on the wait fuse: {seen:?}"
    );

    // The notify arm drains the cell unconditionally: once it is gone, the
    // verdict on this stamp is already decided.
    let drained = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if park_cancel.lock().unwrap().is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(drained.is_ok(), "the notify arm must drain the cancel cell");
    // The branch that follows the drain runs immediately; this window only
    // has to be wide enough for that to become observable.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Interrupted | AgentEvent::WaitCancelled)),
        "a stale cancel must not interrupt the new park: {events:?}"
    );
    let until = *wait_until.lock().unwrap();
    assert!(
        until
            .is_some_and(|deadline| deadline
                > tokio::time::Instant::now() + std::time::Duration::from_secs(10)),
        "the new park must still be armed well into the future: {until:?}"
    );
    handle.abort();
}

/// Cancelling a park on the retry fuse: the transient park is dropped, its
/// fuse and its failure count go with it, and the cancelled deadline runs no
/// round of its own.
#[tokio::test]
async fn a_cancel_on_the_retry_fuse_clears_the_retry_and_its_count() {
    let server = MockServer::start().await;
    mount_status(&server, 500).await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let mut ctx = ctx_from_source(
        vec![profile("p1", "ep1", 500_000)],
        wiremock_source(map),
        fast_policy(),
    )
    .await;
    // One failed request per round, and a backoff long enough to cancel the
    // park before its fuse fires.
    ctx.config.max_transient_retries = 2;
    ctx.config.retry_policy.max_retries = 0;
    ctx.config.retry_policy.base_delay = std::time::Duration::from_secs(2);
    ctx.config.retry_policy.max_delay = std::time::Duration::from_secs(2);
    let retry_at = ctx.retry_at.clone();
    let park_epoch = ctx.park_epoch.clone();
    let park_cancel = ctx.park_cancel.clone();
    let notify = ctx.notify.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));
    ptx.send("go".into()).await.unwrap();

    // The chain fails: the terminal event carries the armed retry (attempt 1).
    let mut attempt = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && attempt.is_none() {
        if let Ok(Some(AgentEvent::FailoverChainExhausted {
            transient_retry: Some(info),
            ..
        })) = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {
            attempt = Some(info.attempt);
        }
    }
    assert_eq!(attempt, Some(1), "the first transient failure arms retry 1");
    assert!(
        retry_at.lock().unwrap().is_some(),
        "the retry fuse must be armed before the cancel"
    );

    crate::agent_task::request_park_cancel(&park_epoch, &park_cancel, &notify);

    expect_wait_cancelled(&mut rx, "retry fuse").await;
    assert!(
        retry_at.lock().unwrap().is_none(),
        "a cancelled retry park must leave no fuse armed"
    );

    // The cancelled fuse runs no round when its deadline passes.
    let before = server.received_requests().await.unwrap().len();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let after = server.received_requests().await.unwrap().len();
    assert_eq!(
        before, after,
        "the cancelled retry fuse must not start a round of its own"
    );

    // The count started over: the next failure arms attempt 1 again.
    ptx.send("again".into()).await.unwrap();
    let mut next_attempt = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && next_attempt.is_none() {
        if let Ok(Some(AgentEvent::FailoverChainExhausted {
            transient_retry: Some(info),
            ..
        })) = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {
            next_attempt = Some(info.attempt);
        }
    }
    assert_eq!(
        next_attempt,
        Some(1),
        "a later failure counts from zero after the cancel"
    );
    handle.abort();
}

/// A wake carrying both a profile reset and a park-cancel applies the reset:
/// the notify arm drains the reset ahead of ending the wake on the cancel,
/// so the order here is what keeps the reset from being deferred to a
/// later wake. The cancel still settles the agent and still starts no round.
#[tokio::test]
async fn a_cancel_does_not_swallow_the_reset_riding_the_same_wake() {
    let server = MockServer::start().await;
    mount_break_stream(&server).await; // break with `{}` → default wait

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let ctx = ctx_from_source(profiles, wiremock_source(map.clone()), fast_policy()).await;
    let wait_until = ctx.wait_until.clone();
    let park_epoch = ctx.park_epoch.clone();
    let park_cancel = ctx.park_cancel.clone();
    let notify = ctx.notify.clone();
    let reset_cell = ctx.pending_profile_reset.clone();

    // The pending reset the tagma would write: a one-profile "fresh" set.
    let fresh = ProfileSet {
        name: "fresh".into(),
        description: None,
        profiles: vec![profile("q0", "ep1", 500_000)],
    };
    let sets = std::collections::BTreeMap::from([("fresh".to_string(), fresh.clone())]);
    let registry = Arc::new(ProfileRegistry::new(sets, wiremock_source(map)).unwrap());

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));
    ptx.send("go".into()).await.unwrap();

    // Park on the wait fuse first, then queue both requests for one wake.
    let armed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(Some(AgentEvent::Waiting { .. })) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            {
                return;
            }
        }
    })
    .await;
    assert!(armed.is_ok(), "the round must park on the wait fuse");
    assert!(
        wait_until.lock().unwrap().is_some(),
        "the fuse must be armed"
    );

    *reset_cell.lock().unwrap() = Some(ProfileReset {
        set: fresh,
        registry,
    });
    crate::agent_task::request_park_cancel(&park_epoch, &park_cancel, &notify);

    expect_wait_cancelled(&mut rx, "reset-riding wake").await;
    assert!(
        reset_cell.lock().unwrap().is_none(),
        "the reset riding the same wake must be applied, not postponed"
    );
    assert!(
        wait_until.lock().unwrap().is_none(),
        "a cancelled park must leave no fuse armed"
    );
    handle.abort();
}

/// A cancel owns its wake but not the rest of the queue: the loop is woken
/// once more, so an inbox message that rode the same permit still gets its
/// own round instead of waiting for an external event that may never come.
#[tokio::test]
async fn a_cancel_still_serves_a_message_that_rode_the_same_wake() {
    use crate::agent_task::MessagePuller;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct StubPuller(Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl MessagePuller for StubPuller {
        async fn pull_undelivered(&self) -> Option<String> {
            if self.0.swap(false, Ordering::SeqCst) {
                Some("message riding the cancelled wake".to_string())
            } else {
                None
            }
        }
    }

    let server = MockServer::start().await;
    mount_break_stream(&server).await; // break with `{}` → default wait

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;
    let pull_flag = Arc::new(AtomicBool::new(false));
    ctx.message_puller = Some(Arc::new(StubPuller(pull_flag.clone())));
    let wait_until = ctx.wait_until.clone();
    let park_epoch = ctx.park_epoch.clone();
    let park_cancel = ctx.park_cancel.clone();
    let notify = ctx.notify.clone();
    let store = ctx.store.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));
    ptx.send("go".into()).await.unwrap();

    // Park on the wait fuse first, then let the cancel take that wake.
    let armed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(Some(AgentEvent::Waiting { .. })) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            {
                return;
            }
        }
    })
    .await;
    assert!(armed.is_ok(), "the round must park on the wait fuse");
    assert!(
        wait_until.lock().unwrap().is_some(),
        "the fuse must be armed"
    );

    // The message lands in the same wake the cancel owns.
    pull_flag.store(true, Ordering::SeqCst);
    crate::agent_task::request_park_cancel(&park_epoch, &park_cancel, &notify);

    expect_wait_cancelled(&mut rx, "message-riding wake").await;

    // The message that rode the same permit must still be served: its text
    // lands in the store when a round records the user turn.
    let served = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let guard = store.lock().await;
            let found = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
                m.content()
                    .map(|t| t.contains("message riding the cancelled wake"))
                    .unwrap_or(false)
            });
            drop(guard);
            if found {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
    handle.abort();
    assert!(
        served.unwrap_or(false),
        "a message riding the cancelled wake must still get its own round"
    );
}

#[tokio::test]
async fn bare_assistant_heartbeats_then_force_idles_at_cap() {
    // Consecutive bare-assistant rounds (no `break`, no tool calls) must not
    // terminate the run: the outer loop records the turn, injects a heartbeat
    // prompt, and re-loops; it force-idles only once `no_progress` exceeds
    // `max_heartbeat_rounds`. This pins the post-rewrite semantics.
    let server = MockServer::start().await;
    mount_ok_stream(&server, "musing").await; // bare assistant on every call
    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;
    ctx.config.max_heartbeat_rounds = 2; // force-idle once no_progress reaches 3
    ctx.record_turn(vec![user_msg("say something")]).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let disposition = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert_ne!(
        disposition,
        WakeDisposition::Terminated,
        "a heartbeat storm must not terminate the task"
    );
    let busy = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Busy))
        .count();
    let idle = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Idle))
        .count();
    assert_eq!(busy, 3, "three bare rounds ran before the cap tripped");
    assert_eq!(idle, 1, "force-idle emits exactly one Idle");
}

#[tokio::test]
async fn fatal_400_no_failover() {
    let primary = MockServer::start().await;
    mount_status(&primary, 400).await; // request-level → Fatal, no failover

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&primary.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (outcome, events) = run_rounds(&mut ctx).await;

    assert!(outcome.is_err(), "400 is Fatal → round errors");
    assert_eq!(ctx.failover.profile_idx(), 0);
    assert!(
        failover_hops(&events).is_empty(),
        "no failover event on a Fatal"
    );
}

#[tokio::test]
async fn chain_exhausted_single_profile_500() {
    let primary = MockServer::start().await;
    mount_status(&primary, 500).await; // exhausts → Failover, but no candidate → ChainExhausted

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&primary.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)]; // single-profile set
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (outcome, events) = run_rounds(&mut ctx).await;

    match outcome {
        Ok(RoundOutcome::Park(AgentOutcome::FailoverChainExhausted {
            reason: FailoverChainExhaustion::NoFailoverConfigured,
            ..
        })) => {}
        other => {
            panic!("expected Park(FailoverChainExhausted(NoFailoverConfigured)), got {other:?}")
        }
    }
    assert_eq!(ctx.failover.profile_idx(), 0);
    assert!(
        failover_hops(&events).is_empty(),
        "no failover event when the chain is exhausted"
    );
}

#[tokio::test]
async fn mid_stream_drop_retries_and_emits_stream_reset() {
    // First request drops mid-stream after a "PARTIAL" delta; the retry returns "done".
    let uri = dropping_then_ok_server("done");
    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&uri));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (outcome, events) = run_rounds(&mut ctx).await;

    let content = match outcome {
        Ok(RoundOutcome::BareAssistant { content, .. }) => content,
        other => panic!("expected BareAssistant, got {other:?}"),
    };
    // The retried full content — not the abandoned "PARTIAL".
    assert_eq!(content, "done");
    // Exactly one mid-stream retry, carrying the episode's send-budget telemetry.
    assert_eq!(stream_resets(&events), vec![(1, 2)]);
    // No failover: the same endpoint recovered.
    assert!(failover_hops(&events).is_empty());
}

#[tokio::test]
async fn mid_stream_drop_budget_exhausted_failovers() {
    // Every request drops mid-stream; after `max_retries` (2) mid-stream retries the endpoint
    // is treated as transient-exhausted → Failover, which a single-profile set surfaces as
    // ChainExhausted(NoFailoverConfigured).
    let uri = always_dropping_server();
    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&uri));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (outcome, events) = run_rounds(&mut ctx).await;

    match outcome {
        Ok(RoundOutcome::Park(AgentOutcome::FailoverChainExhausted {
            reason: FailoverChainExhaustion::NoFailoverConfigured,
            ..
        })) => {}
        other => {
            panic!("expected Park(FailoverChainExhausted(NoFailoverConfigured)), got {other:?}")
        }
    }
    // Two in-budget mid-stream retries ((1,2), (2,2)), then the third drop voids the partial
    // once more before failover — its `attempt` exceeds `max_attempts`, signalling the blown
    // budget — and the chain exhausts (single-profile set).
    assert_eq!(stream_resets(&events), vec![(1, 2), (2, 2), (3, 2)]);
}

// --- run_tool_bounded outer-timeout exemption ---

/// bash_exec owns its own timeout (it converts a timed-out command to a background
/// task instead of failing), so the runner must NOT wrap it in an outer timeout.
#[tokio::test]
async fn bash_exec_is_exempt_from_outer_timeout() {
    let slow = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        ToolCallOutcome::Success("{\"ok\":true}".to_string())
    };
    let out = run_tool_bounded("bash_exec", Duration::from_millis(50), slow).await;
    assert!(matches!(out, ToolCallOutcome::Success(_)), "got {out:?}");
}

/// Every other tool is still bounded: blowing past the runner timeout fails the
/// call with the standard timeout envelope instead of hanging the round.
#[tokio::test]
async fn other_tools_remain_bounded_by_outer_timeout() {
    let slow = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        ToolCallOutcome::Success("{\"ok\":true}".to_string())
    };
    let out = run_tool_bounded("bash_read", Duration::from_millis(50), slow).await;
    match out {
        ToolCallOutcome::Failed(s) => assert!(s.contains("timed out after"), "got {s}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

// --- retry exhaustion / budget probe / silent-retry-loss ---

/// Full-loop pin of the exhaustion path: with `max_transient_retries` spent,
/// the final FCE carries no retry payload and no fuse stays armed — nothing
/// will ever re-fire; only a kick or remove moves the agent.
#[tokio::test]
async fn transient_retry_exhaustion_parks_without_arming() {
    let server = MockServer::start().await;
    mount_status(&server, 500).await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let mut ctx = ctx_from_source(
        vec![profile("p1", "ep1", 500_000)],
        wiremock_source(map),
        fast_policy(),
    )
    .await;
    ctx.config.max_transient_retries = 1;
    let retry_at = ctx.retry_at.clone();
    let wait_until = ctx.wait_until.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));
    ptx.send("go".into()).await.unwrap();

    // Round 1 fails → FCE arms retry #1/1 (payload Some); the auto re-run
    // fails again → budget spent → final FCE carries payload None.
    let mut armed = false;
    let mut exhausted = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline && !(armed && exhausted) {
        if let Ok(Some(AgentEvent::FailoverChainExhausted {
            transient_retry, ..
        })) = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {
            if transient_retry.is_some() {
                armed = true;
            } else {
                exhausted = true;
            }
        }
    }
    handle.abort();
    assert!(armed, "the first FCE must arm a retry (payload Some)");
    assert!(exhausted, "the spent-budget FCE must carry payload None");
    assert!(
        retry_at.lock().unwrap().is_none(),
        "exhaustion must leave no retry fuse armed"
    );
    assert!(
        wait_until.lock().unwrap().is_none(),
        "exhaustion must leave no wait fuse armed"
    );
}

/// Full-loop pin of the recovery path: after an armed FCE the timer re-runs
/// the ORIGINAL prompt — no injected [system] turn appears in the store
/// between elapse and the model call (the Waiting/Retrying essential
/// difference). A `transient_fails` reset (cleared on the succeeded round)
/// is asserted via a second failure arming attempt #1 again.
#[tokio::test]
async fn transient_retry_reruns_original_prompt_without_injection() {
    // Server: fail once, then stream a plain break(idle) so the recovered
    // round ends the task cleanly.
    let server = MockServer::start().await;
    let _fail = Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount_as_scoped(&server)
        .await;
    mount_break_args_stream(&server, "{\"until\":\"idle\"}").await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let mut ctx = ctx_from_source(
        vec![profile("p1", "ep1", 500_000)],
        wiremock_source(map),
        fast_policy(),
    )
    .await;
    ctx.config.max_transient_retries = 3;
    let store = ctx.store.clone();
    let retry_at = ctx.retry_at.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));
    ptx.send("go".into()).await.unwrap();

    let mut recovered = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline && !recovered {
        if let Ok(Some(AgentEvent::Idle)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {
            recovered = true;
        }
    }
    handle.abort();
    assert!(
        recovered,
        "the armed retry must re-run and finish the round"
    );
    let failures = server.received_requests().await.unwrap().len();
    assert_eq!(failures, 2, "one failed call plus one clean re-run");

    // No [system] injection between the failure and the retry: the store
    // holds only the operator's turn (plus the model's answers).
    let guard = store.lock().await;
    let injected = guard
        .turns()
        .iter()
        .flat_map(|t| t.messages.clone())
        .any(|m| m.content().map(|c| c.contains("[system]")).unwrap_or(false));
    drop(guard);
    assert!(
        !injected,
        "the retry re-run must not inject a [system] turn (Waiting's marker)"
    );
    assert!(
        retry_at.lock().unwrap().is_none(),
        "a successful round must clear the armed retry fuse"
    );
}

/// Budget-probe path: an exhausted budget parks the agent
/// WAITING with a re-armed fuse, and while the budget stays exceeded every
/// probe re-checks the round gate BEFORE any LLM call — zero requests hit
/// the server across probe cycles.
#[tokio::test]
async fn budget_probe_rearms_waiting_with_zero_llm_calls() {
    let server = MockServer::start().await;
    mount_ok_stream(&server, "ok").await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let ctx = ctx_from_source(
        vec![profile("p1", "ep1", 500_000)],
        wiremock_source(map),
        fast_policy(),
    )
    .await;
    // Exhaust the shared budget: consumed >= budget.
    let budget = ctx.token_budget.clone();
    budget.set_remaining(0);
    assert!(budget.is_exceeded());
    let wait_until = ctx.wait_until.clone();
    let retry_at = ctx.retry_at.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));
    ptx.send("go".into()).await.unwrap();

    // The initial round: gate blocks BEFORE any LLM call → one
    // TokenBudgetExceeded event and a wait fuse armed (the 600s probe
    // cadence means a second block is not observable in test time; the
    // zero-request assertion below is the gate-first proof, and
    // `wait_timer_elapse_injects_system_turn_and_reruns` covers fuse-fire)
    let mut blocked = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && blocked < 1 {
        if let Ok(Some(AgentEvent::TokenBudgetExceeded { .. })) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {
            blocked += 1;
        }
    }
    handle.abort();
    assert!(
        blocked == 1,
        "exactly one budget block from the initial round ({blocked})"
    );
    assert!(
        wait_until.lock().unwrap().is_some(),
        "each probe cycle re-arms the wait fuse (still Waiting, not parked)"
    );
    assert!(
        retry_at.lock().unwrap().is_none(),
        "the budget path must not arm the retry fuse"
    );
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(
        requests, 0,
        "an exceeded budget must block before any LLM call ({requests} requests)"
    );
}

/// Regression for the budget-skip branch: a wait fuse that elapses while
/// the budget is still exceeded must NOT inject the elapsed-turn message
/// (the gate would block it again — pure context noise). Driven by
/// manually expiring the fuse so the 600s probe cadence need not pass.
#[tokio::test]
async fn budget_probe_fuse_fire_skips_injection() {
    let server = MockServer::start().await;
    mount_ok_stream(&server, "ok").await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let ctx = ctx_from_source(
        vec![profile("p1", "ep1", 500_000)],
        wiremock_source(map),
        fast_policy(),
    )
    .await;
    ctx.token_budget.set_remaining(0);
    let store = ctx.store.clone();
    let wait_notify = ctx.wait_notify.clone();
    let wait_until = ctx.wait_until.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(crate::agent_task::agent_task(ctx, None, prompt_rx, tx));
    ptx.send("go".into()).await.unwrap();

    // Initial round: gate blocks, fuse armed (600s cadence).
    let mut blocked = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && blocked < 1 {
        if let Ok(Some(AgentEvent::TokenBudgetExceeded { .. })) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {
            blocked += 1;
        }
    }
    assert_eq!(blocked, 1, "the initial round must hit the budget gate");

    // Expire the fuse by hand and ring the doorbell: this is exactly the
    // state the spawned 600s sleep would produce.
    *wait_until.lock().unwrap() = Some(tokio::time::Instant::now());
    wait_notify.notify_one();

    let mut second_block = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !second_block {
        if let Ok(Some(AgentEvent::TokenBudgetExceeded { .. })) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {
            second_block = true;
        }
    }
    handle.abort();
    assert!(
        second_block,
        "the expired probe fuse must re-check the gate (second block observed)"
    );
    let guard = store.lock().await;
    let injected = guard
        .turns()
        .iter()
        .flat_map(|t| t.messages.clone())
        .any(|m| {
            m.content()
                .map(|c| c.contains("wait timer elapsed"))
                .unwrap_or(false)
        });
    drop(guard);
    assert!(
        !injected,
        "a budget-probe wake must not inject the elapsed turn (noise the gate would re-block)"
    );
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 0, "the probe path must stay LLM-free");
}

// --- interjection entry cap ---

/// Interjections drained mid-run are external messages: each passes the
/// entry cap individually, so an oversized one is cut with its banner
/// and spilled while an ordinary one passes through raw.
#[tokio::test]
async fn interjections_pass_entry_cap_individually() {
    let _serial = crate::agent_task::message_spill_serial().await;
    let spill_dir = tempfile::TempDir::new().unwrap();
    crate::agent_task::set_message_spill_root(Some(spill_dir.path().join("spill")));
    let mut ctx = make_ctx(vec![profile("test", "ep1", 4096)], &["ep1"]).await;
    let ordinary = "a normal interjection";
    let oversized = "错".repeat(40_000);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(4);
    tx.send(ordinary.to_owned()).await.unwrap();
    tx.send(oversized.clone()).await.unwrap();
    drop(tx);
    drain_interjections(&mut ctx, &mut rx).await;

    let store = ctx.store.lock().await;
    let content = store.turns().back().unwrap().messages[0]
        .content()
        .unwrap()
        .to_owned();
    assert!(content.contains("[Interjected message]"));
    assert!(
        content.contains(ordinary),
        "ordinary interjection must pass through raw"
    );
    assert!(
        content.contains("message truncated"),
        "oversized interjection must be cut with a banner"
    );
    assert!(
        !content.contains(oversized.as_str()),
        "oversized interjection must not survive in full"
    );
    crate::agent_task::set_message_spill_root(None);
}

/// Two peer notices drained in one round merge into a single recorded
/// turn, in arrival order, each wrapped in the interjection markers — the
/// delivery-side notices present exactly like parked-kick turns do.
#[tokio::test]
async fn drained_notices_merge_into_one_turn_in_fifo_order() {
    let mut ctx = make_ctx(vec![profile("test", "ep1", 4096)], &["ep1"]).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(4);
    tx.send("peer message from alice is in your inbox (preview: first)".to_owned())
        .await
        .unwrap();
    tx.send("peer message from bob is in your inbox (preview: second)".to_owned())
        .await
        .unwrap();
    drop(tx);
    drain_interjections(&mut ctx, &mut rx).await;

    let store = ctx.store.lock().await;
    let injected: Vec<String> = store
        .turns()
        .iter()
        .filter_map(|t| t.messages[0].content())
        .filter(|c| c.contains("[Interjected message]"))
        .map(str::to_owned)
        .collect();
    assert_eq!(injected.len(), 1, "notices merge into a single turn");
    let first = injected[0].find("preview: first").unwrap();
    let second = injected[0].find("preview: second").unwrap();
    assert!(first < second, "FIFO order must hold across notices");
    assert!(injected[0].contains("[/Interjected message]"));
}

/// A tool round is a new execution: it does not inherit the previous round's
/// send budget. Round 0 spends its whole (zero-retry) budget and asks for a
/// tool call; round 1 must still reach the provider. Inheriting the ledger
/// would refuse round 1's first send and error the run instead.
#[tokio::test]
async fn a_tool_round_does_not_inherit_the_previous_rounds_send_budget() {
    let server = MockServer::start().await;
    // The tool call is served to the first request only; later requests get a
    // plain answer. The harness registers no tools, so the call itself comes
    // back as a failure envelope — the round loop continues either way.
    let _tool_call = Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    tool_call_sse_body("bash_read", "{\"path\":\"/nonexistent\"}").into_bytes(),
                    "text/event-stream",
                ),
        )
        .up_to_n_times(1)
        .mount_as_scoped(&server)
        .await;
    mount_ok_stream(&server, "done").await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;
    // Two tool rounds, and a policy whose whole send budget is one request:
    // the second round sends only if its ledger starts fresh.
    ctx.config.max_tool_rounds = 2;
    ctx.config.retry_policy.max_retries = 0;

    let (outcome, _events) = run_rounds(&mut ctx).await;

    assert!(
        matches!(outcome, Ok(RoundOutcome::BareAssistant { ref content, .. }) if content == "done"),
        "the second round must reach the provider, got {outcome:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "one tool round plus one answering round"
    );
}

// --- compaction-failure visibility ---

/// A compaction failure the class table rules terminal ends the round with its
/// own reason: the agent parks as a fatal error, the message reaches the event
/// stream, and the round's own request is never sent.
#[tokio::test]
async fn a_stopped_compaction_ends_the_round_with_its_reason() {
    let server = MockServer::start().await;
    mount_ok_stream(&server, "done").await;
    mount_summary_status(&server, 400).await;
    let mut ctx = failing_round_ctx(&server).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let disposition = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

    assert_ne!(
        disposition,
        WakeDisposition::Terminated,
        "a fatal park is not a lifecycle termination"
    );
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let message = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::Error(message) => Some(message.clone()),
            _ => None,
        })
        .expect("a stopped compaction must surface an error event");
    assert!(
        message.contains("compaction failed"),
        "the message must name the failure: {message}"
    );
    assert!(
        message.contains("the request itself was rejected"),
        "the message must carry the class label: {message}"
    );
    assert_eq!(
        request_counts(&server).await,
        (1, 0),
        "the summarizer was asked once and the round never sent its own request"
    );
    let parked = ctx
        .lifecycle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match parked {
        crate::lifecycle::LifecycleState::Parked {
            reason: kallipai_common::protocol::ParkedReason::FatalError { message },
            ..
        } => assert!(message.contains("compaction failed"), "{message}"),
        other => panic!("expected a FatalError park, got {other:?}"),
    }
}

/// The stretch the ledger carries is what bounds the attempts, not the number of
/// rounds: a backoff longer than the test body leaves three over-budget rounds
/// sharing one summarizer call while each round still sends its own request.
#[tokio::test]
async fn attempts_are_bounded_by_the_ledger_not_the_round_count() {
    let server = MockServer::start().await;
    mount_ok_stream(&server, "done").await;
    mount_summary_status(&server, 500).await;
    let policy = crate::retry::RetryPolicy {
        base_delay: Duration::from_secs(60),
        ..crate::retry::RetryPolicy::default()
    };
    let mut ctx = round_ctx_with_policy(&server, FAILURE_PIN_WINDOW, policy).await;
    seed_over_budget_store(&mut ctx).await;

    for round in 0..3 {
        let (outcome, _events) = run_rounds(&mut ctx).await;
        assert!(
            matches!(outcome, Ok(RoundOutcome::BareAssistant { .. })),
            "round {round} must finish on its own answer, got {outcome:?}"
        );
    }

    assert_eq!(
        request_counts(&server).await,
        (1, 3),
        "one compaction attempt covers the three rounds, each of which sends"
    );
    assert_eq!(
        ctx.compaction.attempts(),
        1,
        "one send charged to the stretch"
    );
}

/// Mount a 200 streaming response that first emits a reasoning delta and then
/// `content`, so the round ends on a `BareAssistant` that had reasoning.
async fn mount_ok_stream_with_reasoning(server: &MockServer, content: &str, reasoning: &str) {
    let body = format!(
        "data: {{\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"reasoning_content\":\"{reasoning}\"}}}}]}}\n\ndata: {{\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\ndata: [DONE]\n"
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// A bare assistant turn keeps the model's own reasoning: the next request has
/// to pass it back, so the round outcome must not drop it on the floor.
#[tokio::test]
async fn bare_assistant_carries_the_streams_reasoning() {
    let server = MockServer::start().await;
    mount_ok_stream_with_reasoning(&server, "done", "weighing it").await;

    let map = HashMap::from([("ep1".to_string(), wiremock_backend(&server.uri()))]);
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (outcome, _events) = run_rounds(&mut ctx).await;

    match outcome {
        Ok(RoundOutcome::BareAssistant {
            content, reasoning, ..
        }) => {
            assert_eq!(content, "done");
            assert_eq!(
                reasoning, "weighing it",
                "the model's own reasoning travels with the turn"
            );
        }
        other => panic!("expected BareAssistant, got {other:?}"),
    }
}
