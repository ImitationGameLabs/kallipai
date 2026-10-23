//! The acquisition loop's suite: the within-set walk driven end to end over the
//! recording backend and the shared fact table.

use super::facts::{emit_gateway_signal, rejection_payload};
use super::stream::fatal_provider_status;

use super::*;
use crate::agent_task::AgentContext;
use crate::event::AgentOutcome;
use crate::failover::FailoverOutcome;
use anyhow::Error;
use futures_util::StreamExt;
use just_llm_client::BackendError;
use just_llm_client::types::generation::{GenerationEvent, Message};
use kallipai_common::facts::display_until;
use kallipai_common::retry::RetryRecord;
use kallipai_common::timefmt;
use tokio_util::sync::CancellationToken;

use crate::budget_gate::{BudgetAction, enforce_pre_call_budget};
use crate::retry::{AttemptOrigin, Episode, RetryCall, RetryPolicy, stream_with_retry};
use crate::test_support::{
    FAILURE_PIN_WINDOW, MapSource, RecordingBackend, RecordingHealth, ctx_from_source, mock_source,
    profile, request, seed_over_budget_store, set_context_window, system_msg, user_msg,
};
use just_llm_client::LlmBackend;
use just_llm_client::types::generation::ReasoningEffort;
use kallipai_common::facts::{
    Block, BlockKind, BlockedFact, FactView, ProfilePacing, QUOTA_RESET_TRUST_MAX_SECS,
};
use kallipai_common::retry::RetryKind;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Effort rides the current profile into the wire request: a profile declaring
/// `effort` produces requests carrying that `reasoning_effort`.
#[tokio::test]
async fn acquire_stream_injects_profile_effort_into_the_request() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut p = profile("a", "p1", 100_000);
    p.effort = Some(ReasoningEffort::High);
    let mut ctx = ctx_from_source(vec![p], source, RetryPolicy::default()).await;

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &CancellationToken::new(),
        0,
    )
    .await;
    assert!(matches!(outcome, AcquireResult::Consumed(_)));
    let reqs = backend.take_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].reasoning_effort, Some(ReasoningEffort::High));
}

/// Store rides the profile into the rebuilt conversation: advancing to a candidate
/// declaring `store = false` yields a stateless chain even though the backend supports
/// continuation, while `None` keeps the client default (stateful).
#[tokio::test]
async fn failover_advance_applies_candidate_store_to_the_conversation() {
    let backend = RecordingBackend::new();
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p3".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut stateless = profile("b", "p2", 100_000);
    stateless.store = Some(false);
    let mut ctx = ctx_from_source(
        vec![
            profile("a", "p1", 100_000),
            stateless,
            profile("c", "p3", 100_000),
        ],
        source,
        RetryPolicy::default(),
    )
    .await;
    assert!(
        ctx.conversation.is_stateful(),
        "the opening chain keeps the client default"
    );

    let outcome = advance_failover(
        &mut ctx,
        vec![user_msg("q1")],
        anyhow::anyhow!("endpoint dead"),
        &CancellationToken::new(),
        0,
    )
    .await;
    assert!(matches!(outcome, FailoverOutcome::Advanced { .. }));
    assert!(
        !ctx.conversation.is_stateful(),
        "store = false advances to a stateless chain"
    );

    let outcome = advance_failover(
        &mut ctx,
        vec![user_msg("q2")],
        anyhow::anyhow!("endpoint dead"),
        &CancellationToken::new(),
        0,
    )
    .await;
    assert!(matches!(outcome, FailoverOutcome::Advanced { .. }));
    assert!(
        ctx.conversation.is_stateful(),
        "store = None advances back to the client default"
    );
}

/// Failover replaces the client AND the conversation chain: after an advance, the
/// next request opens full (no `previous_response_id`), because response ids issued
/// by the previous provider are meaningless to the new one.
#[tokio::test]
async fn failover_advance_rebuilds_the_conversation_chain() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    backend.queue_stream("resp-2");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;

    // One successful turn on the opening profile leaves a capture in the chain.
    let mut stream = stream_with_retry(
        RetryCall {
            conversation: &mut ctx.conversation,
            request: request("a-model", vec![user_msg("q1")]),
            policy: &ctx.config.retry_policy,
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
            provider_id: "p1",
            profile_id: "a",
            health: None,
        },
        &tokio::sync::mpsc::channel(16).0,
        &mut Vec::new(),
        CancellationToken::new(),
    )
    .await
    .expect("turn streams");
    while let Some(event) = stream.next().await {
        event.expect("turn events are healthy");
    }
    let response = stream
        .into_response()
        .expect("a drained test stream reaches `End` with a response id");
    ctx.conversation
        .adopt(response)
        .expect("the just-prepared turn adopts cleanly");
    assert!(ctx.conversation.last_message().is_some(), "turn 1 anchored");

    // Advance the chain the way a Failover outcome does.
    let outcome = advance_failover(
        &mut ctx,
        vec![user_msg("q1")],
        anyhow::anyhow!("endpoint dead"),
        &CancellationToken::new(),
        0,
    )
    .await;
    assert!(matches!(outcome, FailoverOutcome::Advanced { .. }));
    assert_eq!(ctx.failover.profile_idx(), 1);
    // Fresh chain: the previous provider's mirror is gone.
    assert!(ctx.conversation.last_message().is_none());

    // The next turn on the new profile opens full — no continuation id.
    let mut stream = stream_with_retry(
        RetryCall {
            conversation: &mut ctx.conversation,
            request: request("b-model", vec![user_msg("q1")]),
            policy: &ctx.config.retry_policy,
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
            provider_id: "p2",
            profile_id: "b",
            health: None,
        },
        &tokio::sync::mpsc::channel(16).0,
        &mut Vec::new(),
        CancellationToken::new(),
    )
    .await
    .expect("post-failover turn streams");
    while let Some(event) = stream.next().await {
        event.expect("turn events are healthy");
    }
    let response = stream
        .into_response()
        .expect("a drained test stream reaches `End` with a response id");
    ctx.conversation
        .adopt(response)
        .expect("the just-prepared turn adopts cleanly");

    let requests = backend.take_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].previous_response_id, None, "turn 1 opens full");
    assert_eq!(
        requests[1].previous_response_id, None,
        "a rebuilt chain cannot continue the old provider's ids"
    );
    assert_eq!(requests[1].messages, vec![user_msg("q1")]);
}

// -- deterministic image-rejection degrade -------------------------

use crate::context::compose_context;
use crate::history::HistoryWriter;
use just_llm_client::TransportError;
use just_llm_client::types::generation::ContentPart;

fn fatal_400() -> Error {
    BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: "images are not supported".to_owned(),
        },
    )
    .into()
}

/// A ctx whose single profile sits on a `RecordingBackend`, with a live
/// image turn (turn 0) plus its matching history sidecar record.
async fn image_ctx(
    backend: Arc<RecordingBackend>,
) -> (AgentContext, tempfile::TempDir, uuid::Uuid) {
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        Arc::clone(&backend) as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("p1", "p1", 100_000)],
        source,
        crate::retry::RetryPolicy::default(),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    ctx.agent_dir = Some(dir.path().to_owned());
    ctx.history = Some(HistoryWriter::new(dir.path().to_owned()));

    let record_id = uuid::Uuid::from_u128(0xD00D);
    let (turn_id, estimated) = {
        let mut guard = ctx.store.lock().await;
        guard.push_turn(vec![Message::user_parts(vec![
            ContentPart::Text {
                text: "chart caption".to_owned(),
            },
            ContentPart::Image {
                source: just_llm_client::types::generation::ImageSource::Base64 {
                    data: "aGk=".to_owned(),
                    media_type: "image/png".to_owned(),
                },
                detail: None,
            },
        ])])
    };
    ctx.append_history(
        Some(turn_id.0),
        &[Message::user("chart caption")],
        estimated,
        crate::history::RecordKind::Turn,
        None,
        &[crate::history::AttachmentRef {
            modality: kallipai_common::protocol::Modality::Image,
            record_id: Some(record_id),
            media_type: "image/png".to_owned(),
            blob_id: format!("sha256-{record_id:064x}"),
            caption: Some("chart caption".to_owned()),
        }],
    );
    (ctx, dir, record_id)
}

#[tokio::test]
async fn degrade_strips_retries_and_records_the_invalidation() {
    let backend = RecordingBackend::new();
    // Attempt 1: the provider 400s the request; attempt 2 (stripped) streams.
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: "unsupported image".to_owned(),
        },
    ));
    backend.queue_stream("resp-1");
    let (mut ctx, _dir, _record_id) = image_ctx(backend.clone()).await;

    let messages = compose_context(ctx.store.clone()).await;
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    // The round's first acquire fails 400 on the image-carrying request.
    let error = match acquire_stream(
        &mut ctx,
        messages,
        Vec::new(),
        &tx,
        &tokio_util::sync::CancellationToken::new(),
        0,
    )
    .await
    {
        AcquireResult::Error(e) => e,
        _other => panic!("expected a fatal image rejection"),
    };
    let result = degrade_on_image_rejection(
        &mut ctx,
        &error,
        Vec::new(),
        &tx,
        &tokio_util::sync::CancellationToken::new(),
        0,
    )
    .await;
    assert!(matches!(result, Some(AcquireResult::Consumed(_))));

    // The live image turn is text-only now, plus the plain-language note.
    {
        let guard = ctx.store.lock().await;
        let image_turn = &guard.turns()[0];
        assert!(
            image_turn
                .messages
                .iter()
                .all(|m| !crate::context::message_has_images(m))
        );
        let note = guard.turns().back().unwrap();
        assert!(format!("{:?}", note.messages).contains("[system]"));
    }
}

#[tokio::test]
async fn degraded_retry_failure_leaves_the_store_untouched() {
    let backend = RecordingBackend::new();
    // Both attempts 400: the images were not the (only) problem.
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: "still bad".to_owned(),
        },
    ));
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: "still bad".to_owned(),
        },
    ));
    let (mut ctx, _dir, record_id) = image_ctx(backend.clone()).await;

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let result = degrade_on_image_rejection(
        &mut ctx,
        &fatal_400(),
        Vec::new(),
        &tx,
        &tokio_util::sync::CancellationToken::new(),
        0,
    )
    .await;
    assert!(result.is_none());

    // Nothing moved: the image turn keeps its parts, no note turn was
    // appended.
    {
        let guard = ctx.store.lock().await;
        assert_eq!(guard.turn_count(), 1);
        assert!(
            guard.turns()[0]
                .messages
                .iter()
                .any(crate::context::message_has_images)
        );
    }
    let _ = record_id;
}

#[test]
fn fatal_provider_status_filters_non_request_level_statuses() {
    let mk = |status: reqwest::StatusCode| {
        BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status,
                body: String::new(),
            },
        )
        .into()
    };
    assert_eq!(
        fatal_provider_status(&mk(reqwest::StatusCode::BAD_REQUEST)),
        Some(reqwest::StatusCode::BAD_REQUEST)
    );
    // Auth/404/5xx statuses belong to failover/retry arms — never degrade.
    assert_eq!(
        fatal_provider_status(&mk(reqwest::StatusCode::UNAUTHORIZED)),
        None
    );
    assert_eq!(
        fatal_provider_status(&mk(reqwest::StatusCode::NOT_FOUND)),
        None
    );
    assert_eq!(
        fatal_provider_status(&mk(reqwest::StatusCode::INTERNAL_SERVER_ERROR)),
        None
    );
    // A pre-flight (non-provider) failure has no status at all.
    assert_eq!(fatal_provider_status(&anyhow::anyhow!("boom")), None);
}

/// The SSE transport keeps reading the body for a bounded window after the
/// terminal event, so a consumer that stops at `End` and drops the stream leaves
/// the connection closed instead of reused. The consumer's only lever is to read
/// on until the stream ends: pin that it does, and that the assembled content
/// still reaches the caller.
#[tokio::test]
async fn acquire_stream_polls_past_the_terminal_event_to_the_stream_end() {
    let backend = RecordingBackend::new();
    let polls_past_last_event = backend.queue_exhausting_stream(vec![
        GenerationEvent::Text {
            delta: "answer".to_owned(),
        },
        GenerationEvent::End {
            finish_reason: None,
            response_id: Some("resp-1".to_owned()),
        },
    ]);
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &CancellationToken::new(),
        0,
    )
    .await;

    let consumed = match outcome {
        AcquireResult::Consumed(consumed) => consumed,
        _ => panic!("a stream that ends completes the acquisition"),
    };
    assert_eq!(consumed.content, "answer");
    assert_eq!(
        polls_past_last_event.load(Ordering::SeqCst),
        1,
        "reading past the last event is what tells the transport to reuse it"
    );
}

/// End to end: a mid-stream drop keeps the anchor, so the in-place retry re-sends
/// the same delta, and the next turn continues from the retry's response — a failed
/// turn never pollutes the chain.
#[tokio::test]
async fn mid_stream_drop_retries_delta_and_the_anchor_advances_past_it() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    backend.queue_dropped_stream(
        vec![just_llm_client::types::generation::GenerationEvent::Text {
            delta: "partial".to_owned(),
        }],
        just_llm_client::TransportError::InvalidResponse(
            "stream closed before the terminal event".to_owned(),
        ),
    );
    backend.queue_stream("resp-2");
    backend.queue_stream("resp-3");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let policy = RetryPolicy {
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(5),
        ..RetryPolicy::default()
    };
    let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;

    // Each turn below is a round: production reaches `acquire_stream` through the
    // episode boundary that restarts the retry ledger, so the drop in turn 2 is that
    // episode's first send.
    ctx.start_episode();
    // Turn 1: full open; resp-1 settles the anchor.
    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &tokio_util::sync::CancellationToken::new(),
        0,
    )
    .await;
    assert!(matches!(outcome, AcquireResult::Consumed(_)));
    let mirror1 = ctx.conversation.last_message().expect("turn 1 anchored");

    // Turn 2: the first stream drops after one delta; the in-place retry succeeds.
    ctx.start_episode();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1"), mirror1.clone(), user_msg("q2")],
        Vec::new(),
        &tx,
        &tokio_util::sync::CancellationToken::new(),
        1,
    )
    .await;
    assert!(matches!(outcome, AcquireResult::Consumed(_)));

    // The dropped attempt told downstream to fold the partial it had rendered.
    let mut resets = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let crate::event::AgentEvent::StreamReset {
            attempt,
            max_attempts,
            ..
        } = event
        {
            resets.push((attempt, max_attempts));
        }
    }
    assert_eq!(resets, vec![(1, ctx.config.retry_policy.max_retries)]);

    // Turn 3: continues from the retry's response — the drop left no trace.
    ctx.start_episode();
    let mirror2 = ctx.conversation.last_message().expect("turn 2 anchored");
    let outcome = acquire_stream(
        &mut ctx,
        vec![
            user_msg("q1"),
            mirror1.clone(),
            user_msg("q2"),
            mirror2,
            user_msg("q3"),
        ],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &tokio_util::sync::CancellationToken::new(),
        2,
    )
    .await;
    assert!(matches!(outcome, AcquireResult::Consumed(_)));

    let requests = backend.take_requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].previous_response_id, None, "turn 1 opens full");
    assert_eq!(
        requests[0].messages,
        vec![system_msg("sys"), user_msg("q1")],
        "full context, system prompt riding the opening turn"
    );
    assert_eq!(
        requests[1].previous_response_id.as_deref(),
        Some("resp-1"),
        "turn 2 chains the settled turn"
    );
    assert_eq!(requests[1].messages, vec![user_msg("q2")], "delta only");
    assert_eq!(
        requests[2].previous_response_id.as_deref(),
        Some("resp-1"),
        "the drop kept the anchor: the retry re-sends the same delta"
    );
    assert_eq!(requests[2].messages, vec![user_msg("q2")]);
    assert_eq!(
        requests[3].previous_response_id.as_deref(),
        Some("resp-2"),
        "the anchor advanced with the retry's response"
    );
    assert_eq!(requests[3].messages, vec![user_msg("q3")], "delta only");
}

/// The episode deadline bounds a mid-stream retry loop: a drop arriving with
/// `retry_timeout` already spent ends the round instead of re-sending, however
/// much send budget is left. The drop is the episode's first send, so the exit
/// comes from the episode's own deadline; the persisted retry log plays no part.
/// The same drop inside the window retries, and a repeated shape stops the round
/// outright (see the shape-repeat tests).
#[tokio::test]
async fn a_mid_stream_drop_past_the_deadline_advances_the_chain() {
    let backend = RecordingBackend::new();
    backend.queue_dropped_stream(
        vec![just_llm_client::types::generation::GenerationEvent::Text {
            delta: "partial".to_owned(),
        }],
        just_llm_client::TransportError::InvalidResponse(
            "stream closed before the terminal event".to_owned(),
        ),
    );
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let policy = RetryPolicy {
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(5),
        ..RetryPolicy::default()
    };
    let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;
    // The episode started well before its own deadline window.
    ctx.episode = Episode::new(timefmt::now_epoch() - 600);

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tx,
        &tokio_util::sync::CancellationToken::new(),
        0,
    )
    .await;

    // One send only, and a single-profile set has nowhere to fail over.
    match outcome {
        AcquireResult::Outcome(AgentOutcome::FailoverChainExhausted { .. }) => {}
        _ => panic!("expected FailoverChainExhausted, got a different outcome"),
    }
    assert_eq!(ctx.episode.attempts(), 1, "the drop spent one send");
    assert_eq!(backend.take_requests().len(), 1, "the drop was not re-sent");
    let mut resets = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let crate::event::AgentEvent::StreamReset {
            attempt,
            delay_secs,
            ..
        } = event
        {
            resets.push((attempt, delay_secs));
        }
    }
    assert_eq!(
        resets,
        vec![(1, 0.0)],
        "the exhausted arm voids the partial with no wait"
    );
}

/// A cancel while the stream is in flight adopts nothing — the anchor stays on
/// the last settled turn and the next turn continues it with a delta. (The cancel
/// path emits no `StreamReset`; that asymmetry is established behavior.)
#[tokio::test]
async fn mid_stream_cancel_keeps_the_anchor_and_the_next_turn_continues_it() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    backend.queue_pending_stream();
    backend.queue_stream("resp-2");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;

    // Turn 1: full open; resp-1 settles the anchor.
    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &tokio_util::sync::CancellationToken::new(),
        0,
    )
    .await;
    assert!(matches!(outcome, AcquireResult::Consumed(_)));
    let mirror1 = ctx.conversation.last_message().expect("turn 1 anchored");

    // Turn 2: the stream never yields; the round cancel fires mid-flight.
    let cancel = tokio_util::sync::CancellationToken::new();
    let canceller = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        })
    };
    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1"), mirror1.clone(), user_msg("q2")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &cancel,
        1,
    )
    .await;
    canceller.await.unwrap();
    assert!(
        matches!(
            outcome,
            AcquireResult::Outcome(crate::event::AgentOutcome::Cancelled)
        ),
        "the round cancels mid-stream"
    );
    assert_eq!(
        ctx.conversation.last_message().as_ref(),
        Some(&mirror1),
        "nothing was adopted past the settled turn"
    );

    // Turn 3: continues the untouched anchor with a delta.
    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1"), mirror1, user_msg("q3")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &tokio_util::sync::CancellationToken::new(),
        2,
    )
    .await;
    assert!(matches!(outcome, AcquireResult::Consumed(_)));

    let requests = backend.take_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].previous_response_id, None, "turn 1 opens full");
    assert_eq!(
        requests[1].previous_response_id.as_deref(),
        Some("resp-1"),
        "the cancelled turn chained normally"
    );
    assert_eq!(requests[1].messages, vec![user_msg("q2")], "delta only");
    assert_eq!(
        requests[2].previous_response_id.as_deref(),
        Some("resp-1"),
        "the cancel kept the anchor: the next turn continues it"
    );
    assert_eq!(requests[2].messages, vec![user_msg("q3")], "delta only");
}

// -- gateway auth-signal emission ------------------------------------

#[tokio::test]
async fn gateway_signal_carries_status_and_code_on_exhaustion_trigger() {
    let mut ctx = ctx_from_source(
        vec![profile("p1", "p1", 100_000)],
        Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            RecordingBackend::new() as Arc<dyn LlmBackend>,
        )]))),
        RetryPolicy::default(),
    )
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.gateway_signals = Some(tx);
    let trigger: Error = BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: r#"{"error":{"message":"revoked","code":"key_revoked"}}"#.to_owned(),
        },
    )
    .into();
    emit_gateway_signal(&ctx, rejection_payload(&trigger).unwrap());
    let signal = rx.recv().await.expect("exactly one signal");
    assert_eq!(signal.status, 401);
    assert_eq!(signal.code.as_deref(), Some("key_revoked"));
    assert!(rx.try_recv().is_err(), "no second signal");
}

#[tokio::test]
async fn gateway_signal_is_silent_without_a_channel() {
    let ctx = ctx_from_source(
        vec![profile("p1", "p1", 100_000)],
        Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            RecordingBackend::new() as Arc<dyn LlmBackend>,
        )]))),
        RetryPolicy::default(),
    )
    .await;
    let trigger: Error = BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::FORBIDDEN,
            body: "denied".to_owned(),
        },
    )
    .into();
    emit_gateway_signal(&ctx, rejection_payload(&trigger).unwrap());
}

#[tokio::test]
async fn gateway_signal_ignores_non_auth_statuses() {
    let mut ctx = ctx_from_source(
        vec![profile("p1", "p1", 100_000)],
        Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            RecordingBackend::new() as Arc<dyn LlmBackend>,
        )]))),
        RetryPolicy::default(),
    )
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.gateway_signals = Some(tx);
    let trigger: Error = BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: "bad".to_owned(),
        },
    )
    .into();
    emit_gateway_signal(&ctx, rejection_payload(&trigger).unwrap());
    assert!(rx.try_recv().is_err(), "400 never signals");
}

/// A spent episode advances the chain instead of sending: the budget is the
/// ledger, so the loop turns a spent one into a failover, not a round error.
#[tokio::test]
async fn a_spent_episode_advances_the_chain_without_sending() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy {
            max_retries: 1,
            ..RetryPolicy::default()
        },
    )
    .await;

    // Spend the opening episode's two sends before the loop's first pass.
    ctx.start_episode();
    ctx.episode = ctx.episode.after_attempt();
    ctx.episode = ctx.episode.after_attempt();

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &CancellationToken::new(),
        0,
    )
    .await;

    assert!(matches!(outcome, AcquireResult::Consumed(_)));
    let reqs = backend.take_requests();
    assert_eq!(reqs.len(), 1, "only the second profile is asked");
    assert_eq!(
        reqs[0].model, "b-model",
        "the opening profile's episode is spent"
    );
}

/// The persisted retry log is an observation surface, not a budget: in-window
/// records for the endpoint leave the episode's sends untouched.
#[tokio::test]
async fn in_window_log_records_do_not_shrink_the_episode_budget() {
    use just_llm_client::{BackendError, TransportError};

    let backend = RecordingBackend::new();
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy {
            max_retries: 2,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
            retry_timeout: std::time::Duration::from_secs(10),
        },
    )
    .await;

    // Three records for this endpoint are already in the log: the episode's own
    // sends are unaffected by what the log holds.
    let now = timefmt::now_epoch();
    let records = (0..3)
        .map(|i| RetryRecord {
            timestamp: now,
            round: 0,
            attempt: i + 1,
            max_attempts: 2,
            error: "endpoint down".to_owned(),
            delay_secs: 0.0,
            endpoint: Some("p1".to_owned()),
            kind: RetryKind::Server,
            quota_reset: None,
        })
        .collect::<Vec<_>>();
    {
        let mut store = ctx.store.lock().await;
        store.record_retries(records, retry_keep_since(&ctx));
    }

    for _ in 0..3 {
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                body: "down".to_owned(),
            },
        ));
    }

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &CancellationToken::new(),
        0,
    )
    .await;

    assert!(matches!(
        outcome,
        AcquireResult::Outcome(AgentOutcome::FailoverChainExhausted { .. })
    ));
    assert_eq!(
        backend.take_requests().len(),
        3,
        "the episode spends max_retries + 1 sends whatever the log holds"
    );
}

/// A quota rejection is not retried at the same endpoint: the round advances to
/// the next profile, which serves the turn.
#[tokio::test]
async fn a_quota_rejection_advances_to_the_next_profile() {
    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "slow down".to_owned(),
        },
    ));
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy {
            max_retries: 2,
            ..RetryPolicy::default()
        },
    )
    .await;

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &CancellationToken::new(),
        0,
    )
    .await;

    assert!(matches!(outcome, AcquireResult::Consumed(_)));
    let reqs = backend.take_requests();
    assert_eq!(reqs.len(), 2, "one send per profile, none repeated");
    assert_eq!(reqs[0].model, "a-model");
    assert_eq!(reqs[1].model, "b-model");
}

/// The same rejection with no candidate left parks the round with the
/// structured reason: the endpoint, the account behind it, and the evidence.
/// The endpoint is asked exactly once, and nothing is armed.
#[tokio::test]
async fn a_quota_rejection_with_no_candidate_left_parks() {
    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "slow down".to_owned(),
        },
    ));
    // A stream is queued so a second send would succeed: the request count pins
    // the claim that no second send happens.
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &CancellationToken::new(),
        0,
    )
    .await;

    let AcquireResult::Outcome(AgentOutcome::QuotaBlocked(info)) = outcome else {
        panic!("a quota rejection with no candidate left parks with the structured reason");
    };
    assert_eq!(info.profile, "a");
    assert_eq!(info.reset, None, "the body carried no resumption hint");
    assert!(info.detail.contains("429"), "{:?}", info.detail);
    assert_eq!(health.recorded().len(), 1, "one rejection, one fact");
    assert_eq!(
        backend.take_requests().len(),
        1,
        "the endpoint is asked exactly once"
    );
}

/// A candidate that exists but cannot be built is no candidate: the switch finds
/// nothing to advance to, and the quota block parks the round.
#[tokio::test]
async fn an_unbuildable_candidate_turns_a_quota_switch_into_a_park() {
    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "slow down".to_owned(),
        },
    ));
    backend.queue_stream("resp-1");
    // p2 is declared but absent from the source, so building it fails.
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &CancellationToken::new(),
        0,
    )
    .await;

    assert!(
        matches!(
            outcome,
            AcquireResult::Outcome(AgentOutcome::QuotaBlocked(_))
        ),
        "an unbuildable candidate leaves nothing to switch to, so the block parks"
    );
    assert_eq!(ctx.failover.profile_idx(), 0, "the skip never adopts p2");
    assert_eq!(backend.take_requests().len(), 1);
}

/// An unbuildable candidate still reports the trigger to the gateway: the switch
/// walks the chain, the walk finds nothing, and the exhaustion hook fires on the
/// auth rejection that started it.
#[tokio::test]
async fn an_unbuildable_candidate_still_signals_the_gateway() {
    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "revoked".to_owned(),
        },
    ));
    // p2 is declared but absent from the source, so building it fails.
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.gateway_signals = Some(signal_tx);
    let (tx, _rx) = tokio::sync::mpsc::channel(64);

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tx,
        &CancellationToken::new(),
        0,
    )
    .await;

    let AcquireResult::Stop { message } = outcome else {
        panic!("an unbuildable candidate leaves nothing to switch to");
    };
    assert!(
        message.contains("endpoint or credentials rejected the request"),
        "{message}"
    );
    let signal = signal_rx
        .try_recv()
        .expect("the trigger reaches the gateway");
    assert_eq!(signal.status, 401);
    assert_eq!(backend.take_requests().len(), 1);
}

/// A single-profile set reports an auth rejection the same way: the chain has
/// nowhere to go, so exhaustion is reached on the first candidate.
#[tokio::test]
async fn an_auth_rejection_signals_the_gateway_at_exhaustion() {
    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "revoked".to_owned(),
        },
    ));
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("p1", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.gateway_signals = Some(signal_tx);
    let (tx, _rx) = tokio::sync::mpsc::channel(64);

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tx,
        &CancellationToken::new(),
        0,
    )
    .await;

    let AcquireResult::Stop { message } = outcome else {
        panic!("a single profile has nothing to switch to");
    };
    assert!(
        message.contains("endpoint or credentials rejected the request"),
        "{message}"
    );
    let signal = signal_rx
        .try_recv()
        .expect("the trigger reaches the gateway");
    assert_eq!(signal.status, 401);
    assert_eq!(backend.take_requests().len(), 1);
}

/// The image degrade is a new execution: it strips the images and re-sends even
/// when the failed attempt used the episode up (a zero-retry policy), because a
/// spent ledger must not starve the one retry that can still succeed.
#[tokio::test]
async fn degrade_retries_when_the_episode_budget_is_spent() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let (mut ctx, _dir, _record_id) = image_ctx(backend.clone()).await;
    ctx.config.retry_policy.max_retries = 0;
    ctx.episode = ctx.episode.after_attempt();

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let result = degrade_on_image_rejection(
        &mut ctx,
        &fatal_400(),
        Vec::new(),
        &tx,
        &tokio_util::sync::CancellationToken::new(),
        0,
    )
    .await;

    assert!(matches!(result, Some(AcquireResult::Consumed(_))));
    assert_eq!(
        backend.take_requests().len(),
        1,
        "the stripped request was sent despite the spent ledger"
    );
    assert_eq!(ctx.episode.attempts(), 1, "the retry ran its own episode");
}

/// A local client-configuration failure never reached a provider: the round stops
/// on the first profile without walking the chain.
#[tokio::test]
async fn a_local_configuration_error_stops_without_walking_the_chain() {
    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::InvalidConfig("no key"),
    ));
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;

    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tokio::sync::mpsc::channel(16).0,
        &CancellationToken::new(),
        0,
    )
    .await;

    let AcquireResult::Stop { message } = outcome else {
        panic!("a local configuration failure stops the round");
    };
    assert!(
        message.contains("local client configuration error"),
        "{message}"
    );
    assert!(message.contains("no failover attempted"), "{message}");
    assert_eq!(
        ctx.failover.profile_idx(),
        0,
        "a local failure is not worth another profile"
    );
    assert_eq!(backend.take_requests().len(), 1);
}
/// The mid-stream path writes what the rejection taught too: the 401 that
/// stops this endpoint leaves an endpoint-level credential fact.
#[tokio::test]
async fn the_mid_stream_path_writes_the_fact_a_rejection_leaves() {
    let backend = RecordingBackend::new();
    backend.queue_dropped_stream(
        vec![just_llm_client::types::generation::GenerationEvent::Text {
            delta: "partial".to_owned(),
        }],
        just_llm_client::TransportError::HttpStatus {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "{\"error\":\"bad key\"}".to_owned(),
        },
    );
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    ctx.profile_pacing = Some(health.clone());

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let outcome = acquire_stream(
        &mut ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tx,
        &tokio_util::sync::CancellationToken::new(),
        0,
    )
    .await;

    // A single-profile set has nowhere to fail over, so the round stops here
    // with the rejection in hand — and the fact is on the record either way.
    let AcquireResult::Stop { message } = outcome else {
        panic!("a 401 with no sibling to try stops the round");
    };
    assert!(message.contains("401"), "the stop carries it: {message}");
    let recorded = health.recorded();
    assert_eq!(recorded.len(), 1, "one rejection, one fact");
    assert_eq!(recorded[0].0, "a", "the fact is keyed by the profile");
    assert_eq!(recorded[0].1.kind, BlockKind::EndpointAuth);
    assert!(
        !recorded[0].1.kind.parks_the_round(),
        "a credential rejection stops the walk instead of parking the round"
    );
}

/// A fact another agent's failure left: the table's own value, seeded so a
/// reader's verdict sees it without a tagma behind the context.
fn seeded(kind: BlockKind, until: Option<u64>) -> Block {
    Block {
        kind,
        until,
        detail: "seeded evidence".to_owned(),
        status: None,
        recorded_at: timefmt::now_epoch(),
    }
}

/// The same fact with the status the upstream answered: what the signal face
/// reads when a fact ends a round.
fn seeded_with_status(kind: BlockKind, until: Option<u64>, status: u16) -> Block {
    Block {
        status: Some(status),
        ..seeded(kind, until)
    }
}

/// Drive one acquisition pass over `ctx`, with the retry records discarded.
async fn acquire_once(ctx: &mut AgentContext) -> AcquireResult {
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    acquire_stream(
        ctx,
        vec![user_msg("q1")],
        Vec::new(),
        &tx,
        &CancellationToken::new(),
        0,
    )
    .await
}

/// A fact the table carries stops a system-initiated attempt before it sends:
/// the endpoint is asked nothing at all, and the account-level fact parks the
/// round with the structured reason.
#[tokio::test]
async fn a_fact_the_table_carries_skips_the_send_and_parks() {
    let backend = RecordingBackend::new();
    // A stream is queued so a send would succeed: the request count pins the
    // claim that no send happens.
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded(BlockKind::Quota, None));
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_once(&mut ctx).await;

    let AcquireResult::Outcome(AgentOutcome::QuotaBlocked(info)) = outcome else {
        panic!("a quota fact with no candidate left parks the round");
    };
    assert_eq!(info.profile, "a");
    assert!(info.detail.contains("seeded evidence"), "{:?}", info.detail);
    assert_eq!(
        backend.take_requests().len(),
        0,
        "the send the fact marks never happens"
    );
}

/// A credential rejection names one profile, so the next profile stays a
/// candidate: the sibling serves the turn. Requiring a second rule here would
/// leave the sibling unused — the flip this case is written to catch.
#[tokio::test]
async fn a_marked_profile_leaves_the_next_one_a_candidate() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded(BlockKind::EndpointAuth, None));
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(outcome, AcquireResult::Consumed(_)),
        "the sibling serves the turn"
    );
    let requests = backend.take_requests();
    assert_eq!(requests.len(), 1, "only the sibling is asked");
    assert_eq!(
        requests[0].model, "b-model",
        "a sibling profile is a candidate once the first one is marked"
    );
}

/// A quota fact names one profile: a mark on another profile leaves this
/// round alone, so the turn goes to the profile the chain is on. A mark
/// leaking across profiles is the flip this case catches.
#[tokio::test]
async fn a_quota_fact_on_another_profile_leaves_the_round_alone() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("b", seeded(BlockKind::Quota, None));
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(outcome, AcquireResult::Consumed(_)),
        "the profile this round runs on is unmarked, so the turn is sent"
    );
    let requests = backend.take_requests();
    assert_eq!(requests.len(), 1, "the unmarked profile is asked once");
    assert_eq!(
        requests[0].model, "a-model",
        "another profile's mark does not stop this round"
    );
}

/// A marked candidate is skipped rather than chosen: the walk keeps looking,
/// so a marked sibling does not hide a usable endpoint behind it.
#[tokio::test]
async fn a_marked_candidate_is_skipped_for_the_next_one() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p3".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![
            profile("a", "p1", 100_000),
            profile("b", "p2", 100_000),
            profile("c", "p3", 100_000),
        ],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded(BlockKind::Quota, None));
    // The sibling in the middle carries a fact of its own.
    health.seed("b", seeded(BlockKind::EndpointAuth, None));
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(outcome, AcquireResult::Consumed(_)),
        "the unmarked sibling serves the turn"
    );
    let requests = backend.take_requests();
    assert_eq!(requests.len(), 1, "the marked sibling is never asked");
    assert_eq!(requests[0].model, "c-model");
}

/// A sibling whose fact lands mid-walk: it answers unmarked for the walk's own
/// readings and marked on the reading the attempt makes after it.
struct FactLandsMidWalk {
    walk_profile: Block,
    sibling: Block,
    sibling_reads_before_visible: usize,
    sibling_reads: AtomicUsize,
}

impl ProfilePacing for FactLandsMidWalk {
    fn record(&self, _profile_id: &str, _block: Block) {}

    fn paced_for(&self, profile_id: &str, _now: u64) -> Option<BlockedFact> {
        let block = match profile_id {
            "a" => Some(&self.walk_profile),
            "b" => {
                let read = self.sibling_reads.fetch_add(1, Ordering::SeqCst);
                (read >= self.sibling_reads_before_visible).then_some(&self.sibling)
            }
            _ => None,
        }?;
        Some(BlockedFact {
            block: block.clone(),
            profile: profile_id.to_owned(),
        })
    }

    fn snapshot(&self, _now: u64) -> Vec<FactView> {
        Vec::new()
    }
}

/// The fact table is shared: a fact another agent writes while the walk runs
/// ends the attempt the walk settled on. The walk's own reading of the sibling
/// passes, the sibling is marked from the third read on, and the attempt the
/// walk hands back takes the reading that sees it — so nothing is sent to an
/// endpoint the table has just refused.
#[tokio::test]
async fn a_fact_written_mid_walk_stops_the_next_send() {
    let backend = RecordingBackend::new();
    // A stream is queued so a send would succeed: the request count pins the
    // claim that no send happens.
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    ctx.profile_pacing = Some(Arc::new(FactLandsMidWalk {
        walk_profile: seeded(BlockKind::EndpointAuth, None),
        sibling: seeded(BlockKind::EndpointAuth, None),
        // The count matters: 1 would mark the sibling for the walk itself, 3 would
        // let the attempt read past it, and either way the window this test pins
        // is gone.
        sibling_reads_before_visible: 2,
        sibling_reads: AtomicUsize::new(0),
    }));

    let outcome = acquire_once(&mut ctx).await;

    let AcquireResult::Stop { message } = outcome else {
        panic!("a fact the sibling carries ends the round");
    };
    assert!(
        message.contains("p2"),
        "the fact that ended the round is the sibling's: {message}"
    );
    assert_eq!(
        backend.take_requests().len(),
        0,
        "the send the mid-walk fact marks never happens"
    );
}

/// The wake-triggered execution's first attempt sends once anyway: a user
/// asking is served even against an endpoint the table has marked, at the
/// price of one bounded attempt.
#[tokio::test]
async fn the_wake_first_attempt_sends_once_through_a_fact() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded(BlockKind::Quota, None));
    ctx.profile_pacing = Some(health.clone());
    ctx.attempt_origin = AttemptOrigin::External;

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(outcome, AcquireResult::Consumed(_)),
        "the wake's own attempt is served"
    );
    assert_eq!(
        backend.take_requests().len(),
        1,
        "the user asking is served once, not skipped"
    );
}

/// A rejection that carries a resumption hint hands it to the payload: the
/// park says when the upstream expects to serve again.
#[tokio::test]
async fn a_quota_rejection_with_a_reset_hint_carries_it() {
    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "{\"error\":\"slow down\",\"retry_after\":120}".to_owned(),
        },
    ));
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    ctx.profile_pacing = Some(health.clone());
    let before = timefmt::now_epoch();

    let outcome = acquire_once(&mut ctx).await;

    let AcquireResult::Outcome(AgentOutcome::QuotaBlocked(info)) = outcome else {
        panic!("a quota rejection parks with the structured reason");
    };
    let reset = info.reset.expect("the upstream gave a moment to show");
    assert!(
        reset >= before + 120,
        "the hint is the upstream's own value: {reset} vs {before}"
    );
    assert_eq!(info.profile, "a", "{info:?}");
}

/// A fact stop on a set with nothing to walk says so, rather than claiming
/// every endpoint ahead is marked.
#[tokio::test]
async fn a_fact_stop_on_a_single_profile_names_that() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded(BlockKind::EndpointAuth, None));
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_once(&mut ctx).await;

    let AcquireResult::Stop { message } = outcome else {
        panic!("an endpoint-level fact with no sibling stops the round");
    };
    assert!(
        message.contains("the set has a single profile"),
        "{message}"
    );
}

/// An endpoint-level fact that carries a reset hint reports it: the stop
/// message names the moment the upstream gave, in the same sentence the
/// failover log line uses.
#[tokio::test]
async fn a_fact_stop_reports_the_resumption_the_upstream_named() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    let until = timefmt::now_epoch() + 300;
    health.seed("a", seeded(BlockKind::EndpointAuth, Some(until)));
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_once(&mut ctx).await;

    let AcquireResult::Stop { message } = outcome else {
        panic!("an endpoint-level fact with no sibling stops the round");
    };
    assert!(
        message.contains(&format!(
            "upstream said it reopens at {until} (unix seconds)"
        )),
        "{message}"
    );
    assert_eq!(backend.take_requests().len(), 0, "nothing is asked");
}

/// The diagnostic faces quote the upstream's own value: a hint past the trust
/// bound still reads as the upstream gave it, while the display face of the
/// same fact reads as unknown — the bound is applied once, where a value
/// becomes a promise a reader may show.
#[tokio::test]
async fn a_fact_stop_quotes_the_hint_the_display_face_clamps() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    let recorded_at = timefmt::now_epoch();
    let until = recorded_at + QUOTA_RESET_TRUST_MAX_SECS + 3_600;
    health.seed(
        "a",
        Block {
            kind: BlockKind::EndpointAuth,
            until: Some(until),
            detail: "seeded evidence".to_owned(),
            status: None,
            recorded_at,
        },
    );
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_once(&mut ctx).await;

    let AcquireResult::Stop { message } = outcome else {
        panic!("an endpoint-level fact with no sibling stops the round");
    };
    assert_eq!(display_until(recorded_at, Some(until)), None);
    assert!(
        message.contains(&format!(
            "upstream said it reopens at {until} (unix seconds)"
        )),
        "{message}"
    );
}

/// The other case: a chain that does exist, with every candidate ahead
/// marked. The note names the marking.
#[tokio::test]
async fn a_fact_stop_with_every_candidate_marked_names_the_marking() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded(BlockKind::EndpointAuth, None));
    health.seed("b", seeded(BlockKind::EndpointAuth, None));
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_once(&mut ctx).await;

    let AcquireResult::Stop { message } = outcome else {
        panic!("a marked chain stops the round");
    };
    assert!(
        message.contains("marked in the shared fact table"),
        "{message}"
    );
    assert_eq!(backend.take_requests().len(), 0, "nothing is asked");
}

/// The wake's own attempt is the only one the table lets through: a
/// wake-triggered execution sends once even against a marked endpoint, and the
/// attempt it would make next is withheld. The origin is spent on the first
/// reading — a second reading that still read `External` would send again.
#[tokio::test]
async fn the_wake_origin_is_spent_on_the_first_attempt() {
    let backend = RecordingBackend::new();
    backend.queue_dropped_stream(
        vec![just_llm_client::types::generation::GenerationEvent::Text {
            delta: "partial".to_owned(),
        }],
        just_llm_client::TransportError::InvalidResponse(
            "stream closed before the terminal event".to_owned(),
        ),
    );
    // A second stream is queued so a second send would succeed: the request
    // count pins the claim that no second send happens.
    backend.queue_stream("resp-2");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy {
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            max_retries: 2,
            ..RetryPolicy::default()
        },
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded(BlockKind::Quota, None));
    ctx.profile_pacing = Some(health.clone());
    ctx.attempt_origin = AttemptOrigin::External;

    let outcome = acquire_once(&mut ctx).await;

    assert_eq!(
        backend.take_requests().len(),
        1,
        "the wake's own attempt is served, the attempt the table marks is not"
    );
    assert!(
        matches!(
            outcome,
            AcquireResult::Outcome(AgentOutcome::QuotaBlocked(_))
        ),
        "the fact the second reading sees parks the round"
    );
}

/// A mid-stream quota rejection leaves the same observation record the send
/// path does: the class decides it, the upstream's reset moment rides it, and
/// no wait follows.
#[tokio::test]
async fn a_mid_stream_quota_rejection_leaves_an_observation_record() {
    let backend = RecordingBackend::new();
    backend.queue_dropped_stream(
        vec![just_llm_client::types::generation::GenerationEvent::Text {
            delta: "partial".to_owned(),
        }],
        just_llm_client::TransportError::HttpStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "{\"error\":\"slow down\",\"retry_after\":120}".to_owned(),
        },
    );
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    ctx.profile_pacing = Some(health.clone());
    let before = timefmt::now_epoch();

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(
            outcome,
            AcquireResult::Outcome(AgentOutcome::QuotaBlocked(_))
        ),
        "the mid-stream rejection parks with the structured reason"
    );
    let records = ctx.store.lock().await.retry_log.clone();
    assert_eq!(records.len(), 1, "one rejection, one observation");
    assert!(matches!(records[0].kind, RetryKind::RateLimit));
    assert_eq!(records[0].delay_secs, 0.0, "no wait follows a quota stop");
    let reset = records[0].quota_reset.expect("the hint rides the record");
    assert!(reset >= before + 120, "{reset}");
    assert_eq!(records[0].endpoint.as_deref(), Some("p1"));
}

// -- a fact that ends a round: what the gateway hears ------------------

/// A fact stop on endpoint credentials tells the gateway what a rejection
/// tells it. The fact path has no error to walk for a status, so the status
/// rides the fact itself — and a fact stores no upstream error code.
#[tokio::test]
async fn an_endpoint_auth_fact_signals_the_gateway_when_it_ends_the_round() {
    let backend = RecordingBackend::new();
    // A stream is queued so a send would succeed: the request count pins the
    // claim that the marked send never happens.
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded_with_status(BlockKind::EndpointAuth, None, 401));
    ctx.profile_pacing = Some(health.clone());
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.gateway_signals = Some(signal_tx);

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(outcome, AcquireResult::Stop { .. }),
        "an endpoint-level fact with no sibling stops the round"
    );
    let signal = signal_rx.try_recv().expect("the fact reaches the gateway");
    assert_eq!(signal.status, 401);
    assert_eq!(signal.code, None, "a fact stores no upstream error code");
    assert!(signal_rx.try_recv().is_err(), "one round, one signal");
    assert_eq!(
        backend.take_requests().len(),
        0,
        "the marked send never happens"
    );
}

/// An endpoint-level verdict the gateway does not act on stays silent: 404
/// says the endpoint is gone, not that its credentials were rejected.
#[tokio::test]
async fn an_endpoint_auth_fact_with_a_status_the_gateway_ignores_stays_silent() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded_with_status(BlockKind::EndpointAuth, None, 404));
    ctx.profile_pacing = Some(health.clone());
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.gateway_signals = Some(signal_tx);

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(outcome, AcquireResult::Stop { .. }),
        "the fact still stops the round"
    );
    assert!(signal_rx.try_recv().is_err(), "404 never signals");
}

/// An account-level fact never signals, whatever status it carries: a quota
/// block arrives as a 403 too, and the level — not the status — is what says
/// this is not an auth rejection. The walk it drives finds no buildable
/// candidate, so the round parks and the gateway hears nothing.
#[tokio::test]
async fn a_quota_fact_never_signals_the_gateway() {
    let backend = RecordingBackend::new();
    // p2 is declared but absent from the source, so building it fails: the
    // fact-driven walk ends here rather than on another endpoint's answer.
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded_with_status(BlockKind::Quota, None, 403));
    ctx.profile_pacing = Some(health.clone());
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.gateway_signals = Some(signal_tx);

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(
            outcome,
            AcquireResult::Outcome(AgentOutcome::QuotaBlocked(_))
        ),
        "an unbuildable candidate leaves nothing to switch to, so the block parks"
    );
    assert!(
        signal_rx.try_recv().is_err(),
        "a quota block is not an auth rejection, whatever status it carries"
    );
}

/// The walk hands the round on: the profile that serves the attempt owns what
/// the attempt leaves behind. The requested model pins where the send went, and
/// the recorded fact, the retry record and the park all name that profile.
#[tokio::test]
async fn a_post_walk_failure_is_recorded_against_the_profile_that_served_it() {
    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "slow down".to_owned(),
        },
    ));
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy {
            max_retries: 1,
            ..RetryPolicy::default()
        },
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded_with_status(BlockKind::EndpointAuth, None, 401));
    ctx.profile_pacing = Some(health.clone());

    let outcome = acquire_once(&mut ctx).await;
    let AcquireResult::Outcome(AgentOutcome::QuotaBlocked(info)) = outcome else {
        panic!("the walk's 429 parks the round");
    };
    assert_eq!(
        info.profile, "b",
        "the park names the profile that served the attempt"
    );

    let requests = backend.take_requests();
    assert_eq!(
        requests
            .iter()
            .map(|r| r.model.as_str())
            .collect::<Vec<_>>(),
        ["b-model"],
        "the walk's new endpoint is the one that is asked"
    );
    let quota: Vec<String> = health
        .recorded()
        .into_iter()
        .filter(|(_, block)| block.kind == BlockKind::Quota)
        .map(|(profile, _)| profile)
        .collect();
    assert_eq!(
        quota,
        ["b"],
        "the failure belongs to the profile that served the attempt"
    );

    let log = ctx.store.lock().await.retry_log.clone();
    assert!(
        !log.is_empty()
            && log
                .iter()
                .all(|record| record.endpoint.as_deref() == Some("p2")),
        "the retry records name the endpoint that served the attempt"
    );
}

/// A fact the chain recovers from stays silent: the signal is for a round that
/// ended on a rejection, not for one that walked past a marked endpoint.
#[tokio::test]
async fn a_fact_the_chain_recovers_from_stays_silent() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        RetryPolicy::default(),
    )
    .await;
    let health = Arc::new(RecordingHealth::default());
    health.seed("a", seeded_with_status(BlockKind::EndpointAuth, None, 401));
    ctx.profile_pacing = Some(health.clone());
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.gateway_signals = Some(signal_tx);

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(outcome, AcquireResult::Consumed(_)),
        "the sibling serves the turn"
    );
    assert_eq!(
        backend.take_requests().len(),
        1,
        "only the sibling is asked"
    );
    assert!(
        signal_rx.try_recv().is_err(),
        "a walk that recovered says nothing to the gateway"
    );
}

// -- acquisition-level shape repeats -----------------------------------

/// A shape repeat is an endpoint-level verdict, and the profile it advances to
/// starts a fresh ledger: the same shape failing there is that episode's first,
/// so it retries in place instead of stopping. The send count pins it — a
/// carried-over shape slot would end the round after three sends.
#[tokio::test]
async fn a_shape_repeat_switches_endpoints_and_the_next_episode_counts_its_own() {
    let backend = RecordingBackend::new();
    for _ in 0..3 {
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::InvalidResponse("empty body".into()),
        ));
    }
    backend.queue_stream("resp-1");
    let source = Arc::new(MapSource(HashMap::from([
        ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
    ])));
    let policy = RetryPolicy {
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(5),
        ..RetryPolicy::default()
    };
    let mut ctx = ctx_from_source(
        vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
        source,
        policy,
    )
    .await;

    let outcome = acquire_once(&mut ctx).await;

    assert!(
        matches!(outcome, AcquireResult::Consumed(_)),
        "the second endpoint serves the turn"
    );
    let models: Vec<String> = backend
        .take_requests()
        .iter()
        .map(|request| request.model.clone())
        .collect();
    assert_eq!(
        models,
        vec!["a-model", "a-model", "b-model", "b-model"],
        "the repeat switches endpoints, and the next episode spends its own shape slot"
    );
    assert_eq!(
        ctx.episode.attempts(),
        2,
        "the new episode counts its own sends"
    );
}

/// The same shape failing twice mid-stream ends the round: the first drop
/// retries in place, the repeat is endpoint-level, and a single-profile set has
/// nowhere to walk. The send count pins the stop — a third send would mean the
/// repeat had been retried as well.
#[tokio::test]
async fn a_mid_stream_shape_repeat_stops_the_round() {
    let backend = RecordingBackend::new();
    for _ in 0..2 {
        backend.queue_dropped_stream(
            vec![GenerationEvent::Text {
                delta: "partial".to_owned(),
            }],
            TransportError::InvalidResponse("stream closed before the terminal event".into()),
        );
    }
    let source = Arc::new(MapSource(HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let policy = RetryPolicy {
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(5),
        ..RetryPolicy::default()
    };
    let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;

    let outcome = acquire_once(&mut ctx).await;

    let AcquireResult::Stop { message } = outcome else {
        panic!("a repeated mid-stream shape with no candidate left stops the round");
    };
    assert!(
        message.contains("the response shape failed twice in this episode"),
        "the stop names the repeat: {message}"
    );
    assert_eq!(
        backend.take_requests().len(),
        2,
        "the first drop retries in place; the repeat stops without a third send"
    );
}

/// A failed compaction during a failover advance sends the prior messages as they
/// are: the rebuilt window is not lost, the chain still advances, and the ledger
/// starts over for the new endpoint — the stretch that failed against the old one
/// is not charged against the one about to be tried.
///
/// Both profiles declare [`FAILURE_PIN_WINDOW`]: `advance_failover` re-applies the
/// *target* profile's window before compacting, so that declaration is what makes
/// the attempt happen — the explicit `set_context_window` below only serves the
/// seed precondition.
///
/// The summarizer prompt this test installs: every POST must carry it, which proves
/// the mock answered compaction calls and not a round request.
const SUMMARY_PROBE_MARKER: &str = "acquisition-summary-probe-marker";

#[tokio::test]
async fn the_advance_arm_records_the_class_and_clears_the_ledger_for_the_new_endpoint() {
    let (source, server) = mock_source(&["p1", "p2"], 429).await;
    let mut ctx = ctx_from_source(
        vec![
            profile("a", "p1", FAILURE_PIN_WINDOW),
            profile("b", "p2", FAILURE_PIN_WINDOW),
        ],
        source,
        RetryPolicy::default(),
    )
    .await;
    set_context_window(&mut ctx, FAILURE_PIN_WINDOW).await;
    // The progressive-warning levels would re-compose before the compaction branch;
    // one threshold leaves the auto-compact trigger as the only one in play.
    ctx.config.context_thresholds = vec![80];
    seed_over_budget_store(&mut ctx).await;
    ctx.summarizer.prompt = SUMMARY_PROBE_MARKER.to_owned();
    let health = Arc::new(RecordingHealth::default());
    ctx.profile_pacing = Some(health.clone());
    let prior_messages = vec![user_msg("carried turn")];

    // The pre-call gate charges the first failure to the ledger before the
    // advance: the stretch is one send old when the new endpoint is built.
    let prompt_tokens = ctx.store.lock().await.total_estimated_tokens();
    let gated =
        enforce_pre_call_budget(&mut ctx, prompt_tokens, 0, &CancellationToken::new()).await;
    assert!(matches!(gated, BudgetAction::Proceed));
    assert_eq!(ctx.compaction.attempts(), 1);

    let outcome = advance_failover(
        &mut ctx,
        prior_messages.clone(),
        anyhow::anyhow!("endpoint dead"),
        &CancellationToken::new(),
        0,
    )
    .await;

    let FailoverOutcome::Advanced { messages, .. } = outcome else {
        panic!("a failed compaction must not stop the advance, got {outcome:?}");
    };
    assert_eq!(
        messages, prior_messages,
        "the prior window is sent unchanged"
    );
    assert_eq!(
        ctx.compaction.attempts(),
        1,
        "the new endpoint's stretch starts at the attempt just made"
    );
    assert!(
        ctx.compaction.is_latched(timefmt::now_epoch()),
        "the defer verdict keeps the gate quiet until the main send's outcome"
    );
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 2, "one compaction attempt per endpoint");
    for request in &requests {
        let body = String::from_utf8_lossy(&request.body);
        assert!(
            body.contains(SUMMARY_PROBE_MARKER),
            "every POST must be a summarizer request, not a round request"
        );
    }
    assert_eq!(
        health.recorded().len(),
        2,
        "each arm writes its class to the endpoint-health view"
    );
    let log = ctx.store.lock().await.retry_log.clone();
    assert_eq!(log.len(), 2, "each arm reaches the retry log");
    assert!(
        log.iter().all(|record| record.kind == RetryKind::RateLimit),
        "429 is the quota class in both arms: {:?}",
        log.iter().map(|record| record.kind).collect::<Vec<_>>()
    );
}
