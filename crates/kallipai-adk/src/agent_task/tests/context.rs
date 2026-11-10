//! What the agent itself records: the persist streak, an episode restart, the
//! turn a round leaves behind, and a lifecycle move that must be refused.

use super::super::context::PERSIST_FAILURES_WARN_AFTER;
use super::super::rounds::run_and_report;
use crate::event::AgentEvent;
#[cfg(debug_assertions)]
use crate::lifecycle::LifecycleState;
use crate::retry::AttemptOrigin;
#[cfg(debug_assertions)]
use kallipai_common::protocol::ParkedReason;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The persist-failure streak: each failed context persist counts, the
/// warning threshold is hit on the third consecutive miss, and any
/// success clears the streak. An agent_dir pointing at a regular file
/// forces the write failure (root-proof).
#[tokio::test]
async fn persist_failure_streak_counts_and_resets() {
    let mut ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    // agent_dir pointing at a regular file: every temp-file creation
    // inside it fails (ENOTDIR), which works even when tests run as
    // root (where directory permissions would not).
    let dir = tempfile::TempDir::new().unwrap();
    let not_a_dir = dir.path().join("not-a-dir");
    std::fs::write(&not_a_dir, "file").unwrap();
    ctx.agent_dir = Some(not_a_dir);

    for expected in 1..=PERSIST_FAILURES_WARN_AFTER {
        ctx.persist().await;
        assert_eq!(ctx.persist_failures.load(Ordering::Relaxed), expected);
    }

    ctx.agent_dir = Some(dir.path().to_path_buf());
    ctx.persist().await;
    assert_eq!(ctx.persist_failures.load(Ordering::Relaxed), 0);
}

/// `start_episode` replaces the ledger: sends charged in the previous episode do
/// not carry over, and the deadline restarts with the new one.
#[tokio::test]
async fn start_episode_replaces_the_ledger() {
    let mut ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("a", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    ctx.episode = crate::retry::Episode::new(kallipai_common::timefmt::now_epoch() - 100);
    ctx.episode = ctx.episode.after_attempt();
    ctx.episode = ctx.episode.after_attempt();

    ctx.start_episode();

    assert_eq!(ctx.episode.attempts(), 0, "the send count restarts");
    assert_eq!(
        ctx.episode.elapsed(kallipai_common::timefmt::now_epoch()),
        std::time::Duration::ZERO,
        "the deadline restarts with the episode"
    );
}

/// A rejected move trips the legality check and leaves the state where it
/// was: the state is applied before it is recorded, so a refused call
/// cannot advance the agent's lifecycle.
/// The refusal rides a debug assertion, so this test is gated to debug
/// builds: a release build writes the new state unchecked.
#[cfg(debug_assertions)]
#[tokio::test]
async fn an_illegal_move_is_rejected_and_leaves_the_state_untouched() {
    let ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;

    // A fresh context sits in idle, whose only edge leads to running: asking
    // for a wait trips the legality assertion.
    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.settle(
            LifecycleState::Waiting {
                until: std::time::Instant::now(),
            },
            "illegal move",
        );
    }));
    assert!(rejected.is_err(), "the illegal move must be rejected");
    assert_eq!(
        *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()),
        LifecycleState::Idle,
        "the rejected wait must leave the state untouched"
    );

    // The parked arm is behind the same check: idle has no edge into
    // parked either.
    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.settle(
            LifecycleState::Parked {
                reason: ParkedReason::MaxRoundsExceeded,
                at: std::time::Instant::now(),
            },
            "illegal move",
        );
    }));
    assert!(rejected.is_err(), "the illegal move must be rejected");
    assert_eq!(
        *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()),
        LifecycleState::Idle,
        "the rejected park must leave the state untouched"
    );

    // A legal move still lands: the refusals above did not wedge the context.
    ctx.settle(LifecycleState::Running, "wake entry");
    assert_eq!(
        *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()),
        LifecycleState::Running,
        "the next legal move lands"
    );
}

/// The recorded reasoning reaches the next request itself: a stateless
/// backend receives the full history, so the second request must carry the
/// assistant turn with the reasoning the stream sent.
#[tokio::test]
async fn a_recorded_turn_carries_the_reasoning_into_the_next_request() {
    use crate::test_support::{MapSource, RecordingBackend, ctx_from_source, profile, user_msg};
    use just_llm_client::LlmBackend;

    let backend = RecordingBackend::new_stateless();
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
        backend.queue_stream_with_reasoning("r1", "weighing it");
    }
    ctx.record_turn(vec![user_msg("q1")]).await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

    let requests = backend.take_requests();
    assert!(
        requests.len() >= 2,
        "the wake reaches a second round: {}",
        requests.len()
    );
    let carried = requests[1]
        .messages
        .iter()
        .find(|m| m.role() == "assistant")
        .expect("the second request carries the recorded assistant turn");
    assert_eq!(
        carried.reasoning().and_then(|r| r.text.as_deref()),
        Some("weighing it"),
        "the recorded reasoning travels in the next request"
    );
}
