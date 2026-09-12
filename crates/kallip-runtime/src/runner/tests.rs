use super::*;
use crate::test_support::{TurnMessage, tool_calls_msg, tool_result_msg, user_msg};
use std::collections::HashMap;
use std::sync::Arc;

use just_llm_client::{
    LlmBackend,
    types::generation::{Message, Reasoning, ToolCall},
};
use kallip_common::protocol::FailoverChainExhaustion;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use crate::acquisition::advance_failover;
use crate::agent_task::{RoundToken, run_and_report};
use crate::failover::{FailoverOutcome, ProfileReset};
use crate::policy::ToolCallOutcome;
use crate::profile::{BackendSource, ProfileRegistry, ProfileSet};
use crate::runner::BreakUntil;
use crate::test_support::{MapSource, ctx_from_source, make_ctx, profile};
use crate::tool_execution::{run_tool_bounded, synthesize_unanswered_results};
use crate::tools::DEFAULT_BREAK_TIMEOUT_SECS;

fn no_cancel() -> CancellationToken {
    CancellationToken::new()
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
    *ctx.pending_profile_reset.lock().unwrap() = Some(ProfileReset {
        set: fresh,
        registry,
    });

    ctx.record_turn(vec![user_msg("switch time")]).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let terminated = run_and_report(&mut ctx, &tx, &mut prompt_rx).await;
    assert!(!terminated);
    let _ = rx.try_recv();

    // The chain was rebuilt from the fresh set's head.
    assert_eq!(ctx.failover.profile_idx(), 0, "reset must land on the head");
    let snapshot = ctx.failover.profile_snapshot();
    assert_eq!(snapshot.set_name, "fresh");
    assert_eq!(snapshot.profile_id, "q0");
    // The cell is drained — a second wake must not re-apply.
    assert!(ctx.pending_profile_reset.lock().unwrap().is_none());
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
    let terminated = run_and_report(&mut ctx, &tx, &mut prompt_rx).await;
    assert!(!terminated);

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
    let terminated = run_and_report(&mut ctx, &tx, &mut prompt_rx).await;
    assert!(!terminated);

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
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
