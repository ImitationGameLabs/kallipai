//! One wake's rounds: which attempt is recorded, when the token is classified as
//! an interrupt, and where a repeated mid-stream shape parks.

use super::super::rounds::{CancelKind, RoundToken, run_and_report};
use super::super::wake::WakeDisposition;
use super::*;
use crate::event::AgentEvent;
use crate::history::HistoryWriter;
use crate::lifecycle::LifecycleState;
use crate::retry::AttemptOrigin;
use kallipai_common::protocol::ParkedReason;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The core inference: a round-token cancel is classified as an interrupt (keep
/// living) unless the lifecycle token was also cancelled — which also verifies
/// the parent→child propagation that makes the classification work.
#[test]
fn cancel_kind_classifies_by_lifecycle_token() {
    // Interrupt: cancel only the round token; lifecycle stays uncancelled.
    let lifecycle = CancellationToken::new();
    let round = RoundToken::new(&lifecycle);
    round.cancel();
    assert!(round.handle().is_cancelled());
    assert!(!lifecycle.is_cancelled());
    assert_eq!(CancelKind::classify(&lifecycle), CancelKind::Interrupt);

    // Lifecycle cancel: propagates to the round token (child).
    let lifecycle = CancellationToken::new();
    let round = RoundToken::new(&lifecycle);
    lifecycle.cancel();
    assert!(round.handle().is_cancelled());
    assert_eq!(CancelKind::classify(&lifecycle), CancelKind::Lifecycle);
}

/// A repeated response shape stops the round instead of arming a transient
/// re-run: the first shape failure retries, the second stops, and the agent
/// parks with the shape-class message rather than a retry-exhaustion one.
#[tokio::test]
async fn a_mid_stream_shape_repeat_parks_without_arming_a_rerun() {
    use crate::test_support::{MapSource, RecordingBackend, ctx_from_source, profile, user_msg};
    use just_llm_client::{LlmBackend, TransportError};

    let backend = RecordingBackend::new();
    // Four drops are queued: a re-run would spend one, so the request count
    // pins the no-re-run claim.
    for _ in 0..4 {
        backend.queue_dropped_stream(
            vec![just_llm_client::types::generation::GenerationEvent::Text {
                delta: "partial".to_owned(),
            }],
            TransportError::InvalidResponse("stream closed before the terminal event".to_owned()),
        );
    }
    let source = Arc::new(MapSource(std::collections::HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let policy = crate::retry::RetryPolicy {
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(5),
        ..crate::retry::RetryPolicy::default()
    };
    let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;
    ctx.record_turn(vec![user_msg("q1")]).await;
    let retry_at = ctx.retry_at.clone();
    let wait_until = ctx.wait_until.clone();

    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    assert_ne!(
        run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
        WakeDisposition::Terminated,
        "a shape stop parks the round instead of terminating the task"
    );
    assert_eq!(
        backend.take_requests().len(),
        2,
        "the first shape failure retries, the repeat stops"
    );
    assert_eq!(ctx.transient_fails, 0, "a stop is not a transient failure");
    assert!(
        retry_at.lock().unwrap().is_none(),
        "a stop must leave no retry fuse armed"
    );
    assert!(
        wait_until.lock().unwrap().is_none(),
        "a stop must leave no wait fuse armed"
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
            message.contains("the response shape failed twice in this episode"),
            "{message}"
        ),
        other => panic!("expected a FatalError park, got {other:?}"),
    }
}

/// A bare-assistant round records what the model reasoned along with what it
/// said: the recorded turn carries the model's own thinking, not just its text.
#[tokio::test]
async fn a_bare_assistant_round_records_the_models_reasoning() {
    use crate::test_support::{MapSource, RecordingBackend, ctx_from_source, profile, user_msg};
    use just_llm_client::LlmBackend;

    let backend = RecordingBackend::new();
    let source = Arc::new(MapSource(std::collections::HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        crate::retry::RetryPolicy::default(),
    )
    .await;
    // One answer per round: the heartbeat limit force-idles the wake, so the
    // queue never runs dry mid-test.
    for _ in 0..=ctx.config.max_heartbeat_rounds {
        backend.queue_stream_with_reasoning("r1", "weighing it");
    }
    ctx.record_turn(vec![user_msg("q1")]).await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

    let store = ctx.store.lock().await;
    let answer = store
        .turns()
        .iter()
        .flat_map(|t| &t.messages)
        .find(|m| m.role() == "assistant")
        .expect("the round records the assistant turn");
    assert_eq!(
        answer.reasoning().and_then(|r| r.text.as_deref()),
        Some("weighing it"),
        "the recorded turn carries the model's reasoning"
    );
}

/// A model round records what served it: the profile identity, the round
/// index, the response usage and the round clock ride the history line, so
/// reading the log back needs no second source.
#[tokio::test]
async fn a_model_round_records_the_attempt_that_served_it() {
    use crate::test_support::{
        MapSource, RecordingBackend, ctx_from_source, profile, usage_with_completion, user_msg,
    };
    use just_llm_client::LlmBackend;

    let backend = RecordingBackend::new();
    let source = Arc::new(MapSource(std::collections::HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        crate::retry::RetryPolicy::default(),
    )
    .await;
    for _ in 0..=ctx.config.max_heartbeat_rounds {
        backend.queue_stream_with_usage("r1", usage_with_completion(120, 30));
    }
    let dir = tempfile::tempdir().unwrap();
    ctx.history = Some(HistoryWriter::new(dir.path().to_owned()));
    ctx.record_turn(vec![user_msg("q1")]).await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

    let records = history_records(dir.path());
    let wanted = crate::history::RecordKind::Turn;
    let served = records
        .iter()
        .find(|r| r.kind == wanted && r.attempt.is_some())
        .expect("the model round's turn carries an attempt");
    let attempt = served.attempt.as_ref().unwrap();
    assert_eq!(attempt.profile_set, "default");
    assert_eq!(attempt.profile_id, "a");
    assert_eq!(attempt.provider, "p1");
    assert_eq!(attempt.model, "a-model");
    assert_eq!(attempt.effort, None);
    assert_eq!(attempt.round, 0);
    let usage = attempt.usage.expect("the response reported usage");
    assert_eq!(usage.prompt, 120);
    assert_eq!(usage.completion, 30);
    assert!(
        records
            .iter()
            .any(|r| r.kind == wanted && r.attempt.is_none()),
        "the prompt before the round carries no attempt"
    );
}

/// A backend that reports no usage leaves the attempt in place and its
/// usage absent: an unreported counter is not a reported zero.
#[tokio::test]
async fn a_round_without_reported_usage_keeps_the_attempt_but_no_usage() {
    use crate::test_support::{MapSource, RecordingBackend, ctx_from_source, profile, user_msg};
    use just_llm_client::LlmBackend;

    let backend = RecordingBackend::new();
    let source = Arc::new(MapSource(std::collections::HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        crate::retry::RetryPolicy::default(),
    )
    .await;
    // One answer per round: the heartbeat limit force-idles the wake, so the
    // queue never runs dry mid-test.
    for _ in 0..=ctx.config.max_heartbeat_rounds {
        backend.queue_stream("r1");
    }
    let dir = tempfile::tempdir().unwrap();
    ctx.history = Some(HistoryWriter::new(dir.path().to_owned()));
    ctx.record_turn(vec![user_msg("q1")]).await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

    let records = history_records(dir.path());
    let wanted = crate::history::RecordKind::Turn;
    let served = records
        .iter()
        .find(|r| r.kind == wanted && r.attempt.is_some())
        .expect("the model round's turn carries an attempt");
    let attempt = served.attempt.as_ref().unwrap();
    assert_eq!(attempt.round, 0);
    assert_eq!(
        attempt.usage, None,
        "a backend that reported nothing must not be recorded as a zero"
    );
}

/// The tool round writes its record after the tool ran, so its facts have to
/// be taken before it: the attempt names the profile the round asked, the
/// counters the backend reported, the round index, and a clock that stops
/// before the tool starts.
#[tokio::test]
async fn a_tool_round_records_the_attempt_before_the_tool_runs() {
    use crate::test_support::{
        GatedTool, MapSource, RecordingBackend, ctx_from_source_with_tools, profile_with_effort,
        usage_with_every_counter, user_msg,
    };
    use just_llm_client::LlmBackend;

    const TOOL_HOLD: std::time::Duration = std::time::Duration::from_millis(300);

    let backend = RecordingBackend::new();
    let gate = GatedTool::new("hold");
    let entered = gate.entered.clone();
    let release = gate.release.clone();
    let source = Arc::new(MapSource(std::collections::HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source_with_tools(
        vec![profile_with_effort(
            "a",
            "p1",
            100_000,
            just_llm_client::types::generation::ReasoningEffort::High,
        )],
        source,
        crate::retry::RetryPolicy::default(),
        vec![Box::new(gate)],
    )
    .await;
    // Round 0 asks for the gated tool, round 1 breaks the run.
    ctx.config.max_tool_rounds = 3;
    backend.queue_tool_call(
        "hold",
        "{}",
        usage_with_every_counter(11, 5, 3, 2, 1),
        Some(std::time::Duration::from_millis(60)),
    );
    backend.queue_tool_call(
        "break",
        "{\"timeout_secs\":30}",
        usage_with_every_counter(21, 9, 0, 0, 0),
        None,
    );
    let dir = tempfile::tempdir().unwrap();
    ctx.history = Some(HistoryWriter::new(dir.path().to_owned()));
    ctx.record_turn(vec![user_msg("q1")]).await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    let handle = tokio::spawn(async move {
        run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await
    });
    // Provably inside the tool: the round cannot have written its record yet.
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .expect("the round reaches the gated tool");
    tokio::time::sleep(TOOL_HOLD).await;
    release.notify_one();
    let _ = handle.await.expect("the agent run finishes");

    let records = history_records(dir.path());
    let attempts: Vec<_> = records.iter().filter_map(|r| r.attempt.as_ref()).collect();
    let tool_round = attempts
        .iter()
        .find(|a| a.round == 0)
        .expect("the tool round carries an attempt");
    assert_eq!(tool_round.profile_set, "default");
    assert_eq!(tool_round.profile_id, "a");
    assert_eq!(tool_round.provider, "p1");
    assert_eq!(tool_round.model, "a-model");
    assert_eq!(tool_round.effort.as_deref(), Some("high"));
    let usage = tool_round.usage.expect("the response reported usage");
    assert_eq!(
        (
            usage.prompt,
            usage.completion,
            usage.cache_read,
            usage.cache_write,
            usage.reasoning
        ),
        (11, 5, 3, 2, 1)
    );
    // The model call was held 60 ms and the tool 300 ms: a clock taken after
    // the tool ran would read past the second number.
    assert!(
        tool_round.elapsed_ms >= 40,
        "the wait on the model belongs to the round: {}",
        tool_round.elapsed_ms
    );
    assert!(
        tool_round.elapsed_ms < TOOL_HOLD.as_millis() as u64,
        "the tool's own time does not: {}",
        tool_round.elapsed_ms
    );
    // The round after the tool round records its own index.
    assert!(
        attempts.iter().any(|a| a.round == 1),
        "the break round carries round 1"
    );
}
