//! The outer loop under its entrances: a notify pull, a prompt, a kick turn, an
//! approval, and the ways a gate can refuse a wake.

use super::super::rounds::run_and_report;
use super::super::task_loop::{MessagePuller, agent_task};
use super::super::wake::WakeDisposition;
use super::*;
use crate::event::AgentEvent;
use crate::history::HistoryWriter;
use crate::lifecycle::LifecycleState;
use crate::retry::AttemptOrigin;
use kallipai_common::protocol::ParkedReason;
use kallipai_common::timefmt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Integration test: a MessagePuller returning a message drives the notify
/// arm to record the message as a user turn before any network call.
#[tokio::test]
async fn notify_pull_drives_round() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct StubPuller(Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl MessagePuller for StubPuller {
        async fn pull_undelivered(&self) -> Option<String> {
            if self.0.swap(false, Ordering::SeqCst) {
                Some("inbox test message".to_string())
            } else {
                None
            }
        }
    }

    // Loopback mock: this poll waits on a turn the round writes after it sends.
    let (mut ctx, _server) = crate::test_support::ctx_with_mock(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
        crate::test_support::PLACEHOLDER_KEY_STATUS,
    )
    .await;

    let flag = Arc::new(AtomicBool::new(true));
    ctx.message_puller = Some(Arc::new(StubPuller(flag.clone())));

    let notify = ctx.notify.clone();
    let cancel = ctx.cancel.clone();
    let store = ctx.store.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

    let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

    // Trigger the notify arm.
    notify.notify_one();

    // Poll the store until the message appears (record_turn runs before
    // any network call in run_and_report).
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let guard = store.lock().await;
            let has_msg = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
                m.content()
                    .map(|t| t.contains("inbox test message"))
                    .unwrap_or(false)
            });
            drop(guard);
            if has_msg {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;

    cancel.cancel();
    let _ = rx.recv().await;
    handle.abort();

    assert!(found.unwrap_or(false), "inbox message should drive a round");
}

/// A peer notice that arrives as the outer loop takes the prompt arm is
/// recorded, and the post-round pull still brings the message body —
/// the race-window guarantee that a notice is never lost even when it
/// lands between rounds.
#[tokio::test]
async fn prompt_arm_notice_is_followed_by_post_round_body_pull() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct StubPuller(Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl MessagePuller for StubPuller {
        async fn pull_undelivered(&self) -> Option<String> {
            if self.0.swap(false, Ordering::SeqCst) {
                Some("pulled message body".to_string())
            } else {
                None
            }
        }
    }
    // Loopback mock, as in the prompt twin: this poll waits on the notify turn.
    let (mut ctx, server) = crate::test_support::ctx_with_mock(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
        crate::test_support::PLACEHOLDER_KEY_STATUS,
    )
    .await;
    mount_streaming_ok(&server).await;

    let flag = Arc::new(AtomicBool::new(true));
    ctx.message_puller = Some(Arc::new(StubPuller(flag.clone())));

    let cancel = ctx.cancel.clone();
    let store = ctx.store.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (prompt_tx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

    prompt_tx
        .send("peer message from alice is in your inbox (preview: hi)".to_string())
        .await
        .unwrap();

    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let guard = store.lock().await;
            let mut contents = Vec::new();
            for t in guard.turns().iter() {
                for m in &t.messages {
                    if let Some(c) = m.content() {
                        contents.push(c.to_owned());
                    }
                }
            }
            drop(guard);
            let notice = contents
                .iter()
                .any(|c| c.contains("peer message from alice"));
            let body = contents.iter().any(|c| c.contains("pulled message body"));
            if notice && body {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;

    cancel.cancel();
    let _ = rx.recv().await;
    handle.abort();
    assert!(
        found.unwrap_or(false),
        "notice turn must be recorded and the post-round pull must bring the body"
    );
}

/// Five-wake-set pin: an approval decision (approve/
/// deny) wakes the parked agent through the shared notify arm and the
/// notification reaches the next round's context — no prompt needed.
/// (Inbox: `notify_pull_drives_round` above; prompt: every full-loop
/// test; duty: `update_duty_on_notifies_agent` at the tagma layer;
/// profile reset: `apply_pending_profile_reset`'s tagma tests.)
#[tokio::test]
async fn approval_decision_wakes_agent_and_drives_round() {
    // Loopback mock, as in the inbox twin: this poll waits on the approval notice.
    let (ctx, _server) = crate::test_support::ctx_with_mock(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
        crate::test_support::PLACEHOLDER_KEY_STATUS,
    )
    .await;

    // Commit + deny one approval so a notification is pending.
    let id = {
        let mut q = ctx.approvals.lock().await;
        let id = q.enqueue("bash_exec", "{}", None);
        q.commit(&id, "test justification").unwrap();
        q.deny(&id, "test deny").unwrap();
        assert!(q.has_notifications());
        id
    };

    let notify = ctx.notify.clone();
    let cancel = ctx.cancel.clone();
    let store = ctx.store.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

    notify.notify_one();

    // The notification is injected as a user turn before any network
    // call (same ordering as the inbox twin).
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let guard = store.lock().await;
            let has = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
                m.content()
                    .map(|t| t.contains(&format!("Approval {id} has been denied")))
                    .unwrap_or(false)
            });
            drop(guard);
            if has {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;

    cancel.cancel();
    let _ = rx.recv().await;
    handle.abort();
    assert!(
        found.unwrap_or(false),
        "an approval decision must wake the agent and reach the round context"
    );
}

/// Guard-matrix pin: a bare notify must NOT wake a parked agent — only
/// the delivery gate's kick turn (auto-wake on message arrival) exits
/// Parked. A notify-class event arriving while parked stays buffered;
/// no round runs.
#[tokio::test]
async fn notify_does_not_wake_parked_agent() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct StubPuller(Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl MessagePuller for StubPuller {
        async fn pull_undelivered(&self) -> Option<String> {
            if self.0.swap(false, Ordering::SeqCst) {
                Some("message while parked".to_string())
            } else {
                None
            }
        }
    }

    let mut ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()) = LifecycleState::Parked {
        reason: ParkedReason::FatalError {
            message: "test park".to_string(),
        },
        at: std::time::Instant::now(),
    };
    let flag = Arc::new(AtomicBool::new(true));
    ctx.message_puller = Some(Arc::new(StubPuller(flag.clone())));

    let notify = ctx.notify.clone();
    let cancel = ctx.cancel.clone();
    let store = ctx.store.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

    notify.notify_one();
    // Give the (wrong) wake a clear window to misbehave, then assert
    // nothing ran.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let guard = store.lock().await;
    let ran = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
        m.content()
            .map(|t| t.contains("message while parked"))
            .unwrap_or(false)
    });
    drop(guard);

    cancel.cancel();
    let _ = rx.recv().await;
    handle.abort();
    assert!(
        !ran,
        "a notify wake must not run a round while parked (the kick is the sole exit)"
    );
}

/// Delivery-gate twin of the guard pin above: the kick turn (sent by the
/// delivery gate's auto-wake) exits Parked via the prompt arm, and the
/// post-round drain then pulls the buffered inbox message into its own
/// round — the deterministic kick→un-park→drain delivery chain.
/// Each step is waited for on a budget of its own: a round that runs
/// long must not spend the time the drain needs.
#[tokio::test]
async fn kick_turn_unparks_then_drains_inbox_message() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct StubPuller(Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl MessagePuller for StubPuller {
        async fn pull_undelivered(&self) -> Option<String> {
            if self.0.swap(false, Ordering::SeqCst) {
                Some("message while parked".to_string())
            } else {
                None
            }
        }
    }
    // Loopback mock: the drain shows up only if the round finishes in the budget.
    let (mut ctx, server) = crate::test_support::ctx_with_mock(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
        crate::test_support::PLACEHOLDER_KEY_STATUS,
    )
    .await;
    mount_streaming_ok(&server).await;

    *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()) = LifecycleState::Parked {
        reason: ParkedReason::FatalError {
            message: "test park".to_string(),
        },
        at: std::time::Instant::now(),
    };
    let flag = Arc::new(AtomicBool::new(true));
    ctx.message_puller = Some(Arc::new(StubPuller(flag.clone())));

    let cancel = ctx.cancel.clone();
    let store = ctx.store.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

    // The kick turn, exactly as the delivery gate's auto-wake sends it.
    ptx.send(
            "[system] you were parked 1s ago: fatal error: test park. Decide whether to retry, adjust, or report."
                .to_string(),
        )
        .await
        .expect("kick send");

    // Step one: the kick round runs to its end. A terminal event is
    // the fact that the round happened, and waiting for it on its own
    // budget keeps a slow round from spending the drain's time. The
    // event itself is kept: the Some/None split tells a failed drain
    // apart from a channel that closed, and the variant names the
    // ending when the drain misses.
    let step_one = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            if event.is_terminal() {
                return Some(event);
            }
        }
        None
    })
    .await;

    // Step two: the post-round drain pulls the buffered message into a
    // round of its own. It lands in the store, so poll for it there.
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let guard = store.lock().await;
            let texts: Vec<String> = guard
                .turns()
                .iter()
                .flat_map(|t| &t.messages)
                .filter_map(|m| m.content().map(|s| s.to_string()))
                .collect();
            drop(guard);
            let kick = texts
                .iter()
                .position(|t| t.starts_with("[system] you were parked"));
            let msg = texts
                .iter()
                .position(|t| t.contains("message while parked"));
            if let (Some(k), Some(m)) = (kick, msg) {
                return (k, m);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;

    cancel.cancel();
    let _ = rx.recv().await;
    handle.abort();
    // Read the waits' outcomes here, after the teardown: a spent
    // budget has to leave the task stopped, not running behind a
    // panic that skipped the cancel.
    let terminal = step_one.expect("the kick round must end inside its own budget");
    assert!(
        terminal.is_some(),
        "the event channel closed before the kick round ended"
    );
    let (kick_at, msg_at) = found.unwrap_or_else(|elapsed| {
        panic!(
            "kick round must run and the drain must deliver the message \
                 within {elapsed}: the round ended on {terminal:?}"
        )
    });
    assert!(
        kick_at < msg_at,
        "the kick round runs first; the drained message is its own later turn"
    );
}

/// A kick wake that the modality gate refuses never reaches the round loop, so
/// the post-round inbox drain never runs: the buffered message stays buffered
/// for a wake that can serve it. Draining it here would deliver a message into
/// a round the wake itself could not start.
#[tokio::test]
async fn a_refused_kick_wake_leaves_the_buffered_message_undelivered() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CountingPuller {
        calls: Arc<AtomicUsize>,
        pending: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl MessagePuller for CountingPuller {
        async fn pull_undelivered(&self) -> Option<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.pending.swap(false, Ordering::SeqCst) {
                Some("message while parked".to_string())
            } else {
                None
            }
        }
    }

    let (mut ctx, backend) = crate::test_support::ctx_with_unservable_image().await;
    *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()) = LifecycleState::Parked {
        reason: ParkedReason::FatalError {
            message: "test park".to_string(),
        },
        at: std::time::Instant::now(),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    ctx.message_puller = Some(Arc::new(CountingPuller {
        calls: calls.clone(),
        pending: Arc::new(AtomicBool::new(true)),
    }));

    let cancel = ctx.cancel.clone();
    let store = ctx.store.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

    // The kick turn, exactly as the delivery gate's auto-wake sends it.
    ptx.send(
            "[system] you were parked 1s ago: fatal error: test park. Decide whether to retry, adjust, or report."
                .to_string(),
        )
        .await
        .expect("kick send");

    // Read the channel out to a quiet window: the refused wake emits its
    // verdict and then nothing.
    let mut events = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await
    {
        events.push(event);
    }
    cancel.cancel();
    let _ = rx.recv().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a wake that never ran a round must not drain the inbox"
    );
    let guard = store.lock().await;
    let drained = guard
        .turns()
        .iter()
        .flat_map(|turn| &turn.messages)
        .any(|m| {
            m.content()
                .is_some_and(|content| content.contains("message while parked"))
        });
    drop(guard);
    assert!(
        !drained,
        "the buffered message waits for a wake that can serve it"
    );

    handle.abort();

    match events.as_slice() {
        [AgentEvent::Error(text)] => assert!(
            text.contains("cannot serve the restored context"),
            "the refused kick wake surfaces the gate's verdict: {text}"
        ),
        other => panic!("expected exactly one Error event, got {other:?}"),
    }
    assert!(
        backend.take_requests().is_empty(),
        "the refused wake never reaches the model"
    );
}

/// A stop the pre-loop wrapper met outside a round is the verdict the round
/// fails on: an external wake restarts the count and keeps the stop, so the
/// gate takes it before sending — one summarizer request, one history line,
/// and the round ends on the message the wrapper itself reached.
#[tokio::test]
async fn a_pre_loop_stop_fails_the_round_after_an_external_wake() {
    // The same setup twice: once for the message the parked verdict carries
    // (the gate called where the wrapper left it), once for the round that
    // reads it after a wake.
    let (mut taken_ctx, taken_server) = stop_pin_ctx().await;
    let prompt_tokens = taken_ctx.store.lock().await.total_estimated_tokens();
    crate::context::compact_pre_loop(&mut taken_ctx).await;
    let action = crate::budget_gate::enforce_pre_call_budget(
        &mut taken_ctx,
        prompt_tokens,
        0,
        &CancellationToken::new(),
    )
    .await;
    let crate::budget_gate::BudgetAction::Fail(error) = action else {
        panic!("the parked stop ends the round");
    };
    let verdict = crate::llm_error::render_error(error.as_ref());
    assert_eq!(
        taken_server
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        1,
        "the wrapper's own failure is the only request"
    );

    let (mut ctx, server) = stop_pin_ctx().await;
    let dir = tempfile::tempdir().unwrap();
    ctx.history = Some(HistoryWriter::new(dir.path().to_owned()));
    crate::context::compact_pre_loop(&mut ctx).await;
    assert_eq!(ctx.compaction.attempts(), 1, "the wrapper charged the send");

    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    assert_eq!(
        run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::External).await,
        WakeDisposition::Held,
        "a stop parks the round: the wake is held, so nothing drains"
    );

    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        1,
        "the wake keeps the stop: the summarizer is not asked again"
    );
    assert_eq!(history_lines(dir.path()), 1, "one line for the failure");
    assert_eq!(
        ctx.compaction.attempts(),
        0,
        "the wake restarts the count the wrapper charged"
    );
    assert!(
        !ctx.compaction.is_latched(timefmt::now_epoch()),
        "a stop is taken by the gate, not delayed by a wait"
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
        } => assert_eq!(
            message, verdict,
            "the round ends on the wrapper's own verdict"
        ),
        other => panic!("expected a FatalError park, got {other:?}"),
    }
}

/// A wake blocked by the modality gate parks the agent through the wake
/// entry's own move: the round never starts, so the state is Running when
/// the gate refuses it and the park rides a legal table edge. Every state
/// the entry can be reached from is covered — a refusal from a parked,
/// waiting, retrying or idle agent must not park over an edge the table
/// rejects.
#[tokio::test]
async fn a_wake_blocked_by_the_modality_gate_parks_from_the_wake_entry() {
    for (from, start) in [
        ("idle", LifecycleState::Idle),
        (
            "waiting",
            LifecycleState::Waiting {
                until: std::time::Instant::now(),
            },
        ),
        (
            "retrying",
            LifecycleState::Retrying {
                attempt: 1,
                max_attempts: 3,
                retry_at: std::time::Instant::now(),
            },
        ),
        (
            "parked",
            LifecycleState::Parked {
                reason: ParkedReason::MaxRoundsExceeded,
                at: std::time::Instant::now(),
            },
        ),
    ] {
        assert_gate_refusal_parks_from(from, start).await;
    }
}

/// An external wake restarts the count and keeps what the last verdict left:
/// the backoff a pre-loop failure armed still holds, so the wake's own gate
/// calls send without asking the summarizer again.
#[tokio::test]
async fn an_external_wake_restarts_the_count_and_keeps_the_armed_backoff() {
    use std::time::Duration;

    const MARKER: &str = "wake-keeps-the-verdict-marker";
    let (source, server) = crate::test_support::mock_source(&["ep1"], 500).await;
    let mut ctx = crate::test_support::ctx_from_source(
        vec![crate::test_support::profile("p1", "ep1", 500_000)],
        source,
        crate::retry::RetryPolicy {
            base_delay: Duration::from_secs(60),
            ..crate::retry::RetryPolicy::default()
        },
    )
    .await;
    crate::test_support::set_context_window(&mut ctx, crate::test_support::FAILURE_PIN_WINDOW)
        .await;
    crate::test_support::seed_over_budget_store(&mut ctx).await;
    ctx.config.context_thresholds = vec![80];
    ctx.summarizer.prompt = MARKER.to_owned();

    // The round's own sends answer with a short stream — a bare assistant keeps
    // the round loop turning without asking anything of the summarizer — while
    // the requests carrying the marker, which are the summarizer's, fail. That
    // failure is what arms the backoff.
    let stream = "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n";
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains(MARKER))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(stream.as_bytes().to_vec(), "text/event-stream"),
        )
        .with_priority(1)
        .mount(&server)
        .await;

    // The stretch the last round left open: one send charged, the backoff
    // armed for a minute, which would cover the wake's own gate calls.
    crate::context::compact_pre_loop(&mut ctx).await;
    assert_eq!(ctx.compaction.attempts(), 1);
    assert!(
        ctx.compaction.is_latched(timefmt::now_epoch()),
        "the pre-loop failure armed the backoff"
    );

    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
    let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::External).await;

    let summaries = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains(MARKER))
        .count();
    assert_eq!(
        summaries, 1,
        "the wake keeps the armed backoff: the summarizer is not asked again"
    );
    assert_eq!(
        ctx.compaction.attempts(),
        0,
        "the wake starts the count over"
    );
    assert!(
        ctx.compaction.is_latched(timefmt::now_epoch()),
        "the armed backoff outlives the count reset"
    );
}
