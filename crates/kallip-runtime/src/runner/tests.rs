use super::*;
use std::collections::HashMap;
use std::sync::Arc;

use just_llm_client::{
    LlmBackend,
    types::chat::{ChatToolCall, ToolCallsMessage},
};
use kallip_common::protocol::FailoverChainExhaustion;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use crate::acquisition::advance_failover;
use crate::agent_task::{RoundToken, run_and_report};
use crate::failover::FailoverOutcome;
use crate::policy::ToolCallOutcome;
use crate::profile::BackendSource;
use crate::test_support::{MapSource, ctx_from_source, make_ctx, profile};
use crate::tool_execution::{run_tool_bounded, synthesize_unanswered_results};

fn no_cancel() -> CancellationToken {
    CancellationToken::new()
}

// --- synthesize_unanswered_results unit tests ---

use just_llm_client::types::chat::{FunctionCall, ToolType};

/// Build an assistant `tool_calls` message declaring the given (id, tool name) calls.
fn assistant_tool_calls(calls: &[(&str, &str)]) -> ChatMessage {
    ChatMessage::ToolCalls(ToolCallsMessage {
        role: "assistant".into(),
        content: None,
        name: None,
        tool_calls: calls
            .iter()
            .map(|(id, name)| ChatToolCall {
                id: (*id).into(),
                kind: ToolType::Function,
                function: FunctionCall {
                    name: (*name).into(),
                    arguments: "{}".into(),
                },
            })
            .collect(),
        reasoning_content: None,
    })
}

fn result_ids(msgs: &[ChatMessage]) -> Vec<&str> {
    msgs.iter().filter_map(|m| m.tool_call_id()).collect()
}

#[test]
fn synthesize_answers_break_and_trailing_calls() {
    // [break, bash_exec]: the loop returns at break, so neither ran — break gets
    // its success ack, bash_exec gets an honest not-executed result.
    let mut msgs = vec![assistant_tool_calls(&[
        ("c1", "break"),
        ("c2", "bash_exec"),
    ])];
    synthesize_unanswered_results(&mut msgs);
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
        assistant_tool_calls(&[("c1", "bash_exec"), ("c2", "break")]),
        ChatMessage::tool_result("done", "c1"),
    ];
    synthesize_unanswered_results(&mut msgs);
    assert_eq!(result_ids(&msgs), vec!["c1", "c2"]);
    assert_eq!(msgs[1].content().unwrap(), "done");
    assert!(msgs[2].content().unwrap().contains("parked"));
}

#[test]
fn synthesize_is_noop_on_complete_turn_and_idempotent() {
    let mut msgs = vec![
        assistant_tool_calls(&[("c1", "bash_exec")]),
        ChatMessage::tool_result("done", "c1"),
    ];
    synthesize_unanswered_results(&mut msgs);
    assert_eq!(msgs.len(), 2, "a fully-answered turn is untouched");
    // Running again over the completed turn must not duplicate anything.
    let before = msgs.clone();
    synthesize_unanswered_results(&mut msgs);
    assert_eq!(result_ids(&msgs), result_ids(&before));
    assert_eq!(msgs.len(), before.len());
}

#[test]
fn synthesize_keeps_assistant_content_and_reasoning_intact() {
    let mut msgs = vec![ChatMessage::ToolCalls(ToolCallsMessage {
        role: "assistant".into(),
        content: Some("parking now".into()),
        name: None,
        tool_calls: vec![ChatToolCall {
            id: "c1".into(),
            kind: ToolType::Function,
            function: FunctionCall {
                name: "break".into(),
                arguments: "{}".into(),
            },
        }],
        reasoning_content: Some("done thinking".into()),
    })];
    synthesize_unanswered_results(&mut msgs);
    assert_eq!(msgs[0].content(), Some("parking now"));
    assert_eq!(msgs[0].reasoning_content(), Some("done thinking"));
    assert_eq!(msgs[1].tool_call_id(), Some("c1"));
}

// --- advance_failover unit tests (fast, no wiremock; summarize_and_evict no-ops on empty store) ---

#[tokio::test]
async fn advance_lands_on_next_buildable() {
    let profiles = vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)];
    let mut ctx = make_ctx(profiles, &["ep1", "ep2"]).await;
    let before_window = ctx.config.context_window_tokens;

    let outcome =
        advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;

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
    let outcome =
        advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
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
    let outcome = advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &cancel).await;
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
    let outcome =
        advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
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
    let outcome =
        advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
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
    let outcome =
        advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
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

    let outcome =
        advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;

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
    let outcome =
        advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
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

/// A `MapSource` mapping endpoint id → wiremock backend.
fn wiremock_source(map: HashMap<String, Arc<dyn LlmBackend>>) -> Arc<dyn BackendSource> {
    Arc::new(MapSource(map))
}

/// Drive one `run_agent_rounds`: seed a user turn, mint a round token, run, collect events.
async fn run_rounds(ctx: &mut AgentContext) -> (Result<RoundOutcome>, Vec<AgentEvent>) {
    ctx.record_turn(vec![ChatMessage::user(
        "respond with the single word: done",
    )])
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
        for attempt in 0..2u32 {
            let (mut socket, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => return,
            };
            // Best-effort drain of the request (the small JSON body); HTTP is full-duplex so
            // writing the response does not depend on fully reading the request.
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let _ = socket.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
                );
            if attempt == 0 {
                write_chunk(&mut socket, &sse_delta("PARTIAL"));
                // Drop mid-stream: close without the terminating zero-length chunk.
                let _ = socket.shutdown(std::net::Shutdown::Both);
            } else {
                write_chunk(&mut socket, &sse_delta(&retry));
                write_chunk(&mut socket, b"data: [DONE]\n\n");
                let _ = socket.write_all(b"0\r\n\r\n");
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
        Ok(RoundOutcome::BareAssistant { content }) => content,
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

#[tokio::test]
async fn break_tool_call_yields_break_outcome() {
    // A `break` tool call short-circuits the round: the outcome is `Break`
    // (not BareAssistant, not a continued tool round). This pins the
    // R-M2/R-M3 contract — break terminates the round via the hoisted
    // name-check in execute_tool_calls.
    let server = MockServer::start().await;
    mount_break_stream(&server).await;

    let mut map = HashMap::new();
    map.insert("ep1".into(), wiremock_backend(&server.uri()));
    let profiles = vec![profile("p1", "ep1", 500_000)];
    let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

    let (outcome, _events) = run_rounds(&mut ctx).await;
    assert!(
        matches!(outcome, Ok(RoundOutcome::Break)),
        "expected RoundOutcome::Break, got {outcome:?}"
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
    ctx.record_turn(vec![ChatMessage::user("say something")])
        .await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let terminated = run_and_report(&mut ctx, &tx, &mut prompt_rx).await;

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(!terminated, "a heartbeat storm must not terminate the task");
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
    let profiles = vec![profile("p1", "ep1", 500_000)]; // single-profile tier
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
        Ok(RoundOutcome::BareAssistant { content }) => content,
        other => panic!("expected BareAssistant, got {other:?}"),
    };
    // The retried full content — not the abandoned "PARTIAL".
    assert_eq!(content, "done");
    // Exactly one mid-stream retry, carrying the shared per-endpoint budget telemetry.
    assert_eq!(stream_resets(&events), vec![(1, 2)]);
    // No failover: the same endpoint recovered.
    assert!(failover_hops(&events).is_empty());
}

#[tokio::test]
async fn mid_stream_drop_budget_exhausted_failovers() {
    // Every request drops mid-stream; after `max_retries` (2) mid-stream retries the endpoint
    // is treated as transient-exhausted → Failover, which a single-profile tier surfaces as
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
    // budget — and the chain exhausts (single-profile tier).
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
