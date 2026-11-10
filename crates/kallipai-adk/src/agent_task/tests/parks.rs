//! The park machine end to end: a cancel clears every fuse, a quota park arms its
//! wake, a paced park wakes itself to serve the round it owed, and a rerun after a
//! park starts a fresh episode.

use super::super::fuses::ParkArm;
use super::super::rounds::run_and_report;
use super::super::task_loop::agent_task;
use super::super::wake::WakeDisposition;
use crate::event::AgentEvent;
use crate::lifecycle::LifecycleState;
use crate::retry::AttemptOrigin;
use kallipai_common::protocol::ParkedReason;
use std::sync::Arc;

/// A cancelled park drops every fuse and settles idle in one place: the
/// bridge mirror is event-driven, so only `ctx.lifecycle` proves the
/// cancel landed on the authoritative state.
#[tokio::test]
async fn a_cancelled_park_settles_idle_and_clears_the_fuses() {
    let mut ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    ctx.settle(LifecycleState::Running, "test round");
    let park = ctx.arm_park(ParkArm::Wait { timeout_secs: 30 });
    ctx.settle_park(park, "test park");
    ctx.transient_fails = 2;
    *ctx.pacing_wake_at.lock().unwrap() = Some(tokio::time::Instant::now());
    assert!(
        matches!(
            *ctx.lifecycle.lock().unwrap(),
            LifecycleState::Waiting { .. }
        ),
        "the park must mirror as waiting before the cancel"
    );

    ctx.end_cancelled_park();

    assert!(
        matches!(*ctx.lifecycle.lock().unwrap(), LifecycleState::Idle),
        "a cancel must settle the authoritative state to idle"
    );
    assert!(
        ctx.wait_until.lock().unwrap().is_none(),
        "a cancelled park must leave no fuse armed"
    );
    assert!(
        ctx.retry_at.lock().unwrap().is_none(),
        "a cancelled park must leave no retry armed"
    );
    assert!(
        ctx.pacing_wake_at.lock().unwrap().is_none(),
        "a cancelled park must leave no pacing wake armed"
    );
    assert_eq!(ctx.transient_fails, 0, "the episode's count starts over");
}

/// A rerun of the prompt is a new execution: the episode ledger restarts, so
/// the rerun sends. Carrying the spent ledger over would turn every send into a
/// failover and park again without a single request.
#[tokio::test]
async fn a_rerun_starts_a_fresh_episode_and_sends() {
    use crate::test_support::{MapSource, RecordingBackend, ctx_from_source, profile, user_msg};
    use just_llm_client::{BackendError, LlmBackend, TransportError};

    fn endpoint_down() -> BackendError {
        BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                body: "down".to_owned(),
            },
        )
    }

    let backend = RecordingBackend::new();
    // Execution 1: both sends of the episode fail transiently, so the episode is
    // spent by the time the chain exhausts and the agent parks for a retry.
    backend.queue_error(endpoint_down());
    backend.queue_error(endpoint_down());
    // Execution 2: the endpoint answers (plus the heartbeat rounds that follow).
    for _ in 0..5 {
        backend.queue_stream("resp-1");
    }

    let source = Arc::new(MapSource(std::collections::HashMap::from([(
        "p1".to_string(),
        backend.clone() as Arc<dyn LlmBackend>,
    )])));
    let policy = crate::retry::RetryPolicy {
        max_retries: 1,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(2),
        retry_timeout: std::time::Duration::from_secs(10),
    };
    let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;
    ctx.record_turn(vec![user_msg("q1")]).await;

    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    assert_ne!(
        run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
        WakeDisposition::Terminated,
        "a transient exhaustion parks the round instead of terminating the task"
    );
    assert_eq!(
        backend.take_requests().len(),
        2,
        "the episode spent its two sends"
    );
    assert_eq!(
        ctx.episode.attempts(),
        2,
        "the parked agent keeps the spent ledger"
    );

    assert_ne!(
        run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
        WakeDisposition::Terminated
    );
    let rerun = backend.take_requests();
    assert!(
        !rerun.is_empty(),
        "the rerun sends: its episode budget is fresh"
    );
    assert_eq!(rerun[0].model, "a-model");
}

/// A quota rejection ends the round as a structured park instead of arming a
/// transient re-run: the endpoint is asked once, the agent parks with the
/// payload, and the wake it leaves answers to the window the fact opened.
#[tokio::test]
async fn a_quota_block_parks_and_arms_a_wake_at_the_window_end() {
    use crate::test_support::{MapSource, RecordingBackend, ctx_from_source, profile, user_msg};
    use just_llm_client::{BackendError, LlmBackend, TransportError};

    let backend = RecordingBackend::new();
    // Four errors are queued: a re-run would spend one, so the request count
    // pins the no-re-run claim.
    for _ in 0..4 {
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: crate::test_support::QUOTA_RESET_BODY.to_owned(),
            },
        ));
    }
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
    ctx.record_turn(vec![user_msg("q1")]).await;
    let retry_at = ctx.retry_at.clone();
    let wait_until = ctx.wait_until.clone();
    let pacing_wake_at = ctx.pacing_wake_at.clone();

    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    assert_ne!(
        run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
        WakeDisposition::Terminated,
        "a quota stop parks the round instead of terminating the task"
    );
    assert_eq!(
        backend.take_requests().len(),
        1,
        "the endpoint is asked exactly once"
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
    assert!(
        pacing_wake_at.lock().unwrap().is_some(),
        "a quota park leaves a wake armed so the round can leave on its own"
    );
    let parked = ctx
        .lifecycle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match parked {
        LifecycleState::Parked {
            reason: ParkedReason::QuotaBlocked(info),
            ..
        } => {
            assert_eq!(info.profile, "a");
            assert!(
                info.detail.contains("429"),
                "the payload carries the evidence: {info:?}"
            );
        }
        other => panic!("expected a structured quota park, got {other:?}"),
    }
}

/// A parked quota round comes back on its own: the wake fires at the window's
/// end with nothing external arriving, and the round it starts claims the
/// reopened window and sends. A round resuming the parked episode could not:
/// its episode already holds the window, so the claim would read as spent.
#[tokio::test]
async fn a_paced_park_wakes_itself_and_serves_the_round() {
    use crate::test_support::{
        MapSource, RecordingBackend, RecordingPacing, ctx_from_source, profile,
    };
    use just_llm_client::{BackendError, LlmBackend, TransportError};

    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: r#"{"error":"slow down","retry_after":1}"#.to_owned(),
        },
    ));
    backend.queue_stream("r1");
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
    ctx.profile_pacing = Some(Arc::new(RecordingPacing::default()));

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(agent_task(ctx, Some("q1".to_owned()), prompt_rx, tx));

    let parked = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = rx.recv().await {
            if matches!(event, AgentEvent::QuotaBlocked(_)) {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(parked.ok(), Some(true), "the round parks on the rejection");

    // Nothing else can start the next round — no prompt, no notify, no
    // approval — so the send this waits for is the wake's own doing.
    backend.take_requests();
    let resumed = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if !backend.take_requests().is_empty() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert_eq!(
        resumed.ok(),
        Some(true),
        "the parked round sends again with nothing external arriving"
    );
    handle.abort();
}
