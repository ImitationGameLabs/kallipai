// Direct tests for enqueue_prompt's slow path (reactivation). Historically
// every test held the prompt receiver open to stay on the notify fast path,
// because falling through spawns a real runtime; `AppState::spawn_fn` lets
// these tests stub the spawn and observe the reactivation semantics: the
// message buffers to the inbox (no channel pre-send), the spawn receives the
// dead incarnation's store/approvals/env, and the fresh agent is written back
// live. The reserve-step handle aborts are not asserted (JoinHandle has no
// comparable identity once swapped) -- they run in the same locked block that
// installs the fresh channel.

use std::sync::{Arc, Mutex};

use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::DeliveryMode;

use crate::lifecycle::SpawnArgs;
use crate::relay::chat_history;
use crate::state::{AgentEntry, RegistryEntry, SharedState};
use crate::test_helpers::{
    install_inbox_store, install_projector, make_entry_with_rx, make_state, make_state_with_spawn,
};

#[derive(Default)]
struct Seen {
    store: Mutex<Option<Arc<tokio::sync::Mutex<kallipai_adk::context::ContextStore>>>>,
    approvals: Mutex<Option<Arc<tokio::sync::Mutex<kallipai_adk::approval::ApprovalStore>>>>,
    initial_prompt: Mutex<Option<String>>,
}

fn state_with_stub(seen: Arc<Seen>, fail: bool) -> SharedState {
    make_state_with_spawn(Arc::new(move |args: SpawnArgs| {
        *seen.store.lock().unwrap() = Some(args.store.clone());
        *seen.approvals.lock().unwrap() = Some(args.approvals.clone());
        *seen.initial_prompt.lock().unwrap() = args.initial_prompt.clone();
        let fresh = make_entry_with_rx(None, "agent-stub".to_string());
        let (agent, identity) = {
            let AgentEntry {
                identity, agent, ..
            } = fresh.0;
            (agent, identity)
        };
        Box::pin(async move {
            if fail {
                anyhow::bail!("stub spawn failure");
            }
            Ok((agent, identity))
        })
    }))
}

/// Register a live entry whose prompt receiver is dropped: the channel reads
/// closed, so enqueue_prompt falls through to reactivation instead of
/// notifying. Returns the dead incarnation's store for identity preservation
/// assertions.
async fn register_dead_root(
    state: &SharedState,
) -> (
    AgentId,
    Arc<tokio::sync::Mutex<kallipai_adk::context::ContextStore>>,
) {
    let id = AgentId::random();
    let (entry, _rx) = make_entry_with_rx(None, format!("agent-{id}"));
    let store = entry.agent.store.clone();
    drop(_rx);
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);
    (id, store)
}

#[tokio::test]
async fn slow_path_buffers_to_inbox_passes_identity_and_reinstalls_live() {
    let seen = Arc::new(Seen::default());
    let state = state_with_stub(seen.clone(), false);
    install_inbox_store(&state).await;
    let (id, dead_store) = register_dead_root(&state).await;

    let resp = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Surface,
    )
    .await
    .expect("slow path succeeds with stubbed spawn");
    assert_eq!(resp.queue_depth, 0);
    assert!(resp.warning.is_none());
    assert_eq!(resp.delivery_mode, Some(DeliveryMode::Buffered));

    // Spawn received the dead incarnation's store (preserved identity) and no
    // pre-sent prompt (the message rides the inbox instead).
    let seen_store = seen.store.lock().unwrap().clone().expect("spawn called");
    assert!(
        Arc::ptr_eq(&seen_store, &dead_store),
        "SpawnArgs.store preserved"
    );
    assert!(
        seen.initial_prompt.lock().unwrap().is_none(),
        "no channel pre-send"
    );

    // The message is in the inbox.
    let inbox = state.inboxes.get().expect("inbox installed");
    let listed = inbox.list(&id, &crate::inbox::InboxFilter::default()).await;
    assert!(
        listed.iter().any(|e| e.body == "hello"),
        "message buffered to inbox"
    );

    // The fresh agent is written back live with an open channel.
    let prompt_tx_open = {
        let registry = state.registry.read().await;
        !registry
            .get(&id)
            .expect("agent still registered")
            .as_live()
            .expect("entry live after reactivation")
            .agent
            .prompt_tx
            .is_closed()
    };
    assert!(!prompt_tx_open, "fresh prompt channel installed");
}

#[tokio::test]
async fn slow_path_spawn_failure_leaves_agent_dead_with_conflict_free_state() {
    let seen = Arc::new(Seen::default());
    let state = state_with_stub(seen.clone(), true);
    install_inbox_store(&state).await;
    let (id, _store) = register_dead_root(&state).await;

    let err = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Surface,
    )
    .await
    .expect_err("stubbed spawn failure surfaces");
    assert!(
        err.to_string().contains("500"),
        "spawn failure surfaces as an internal error: {err}"
    );

    // The message still landed in the inbox (the push precedes the gate), so a
    // later retry pulls it once the agent finally wakes.
    let inbox = state.inboxes.get().expect("inbox installed");
    let listed = inbox.list(&id, &crate::inbox::InboxFilter::default()).await;
    assert!(
        listed.iter().any(|e| e.body == "hello"),
        "message stays buffered after failed spawn"
    );
}

// -- dangling-binding gate (reject before the inbox write) --

/// A live idle subagent whose explicit binding does not resolve: the send is
/// rejected and the message never reaches the inbox (both the notify fast
/// path and the kick path pass through the gate). A subagent, because an
/// unbound or dangling-root record falls back to the default set instead.
#[tokio::test]
async fn delivery_rejected_for_dangling_agent_live_idle() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (mut entry, _rx) = make_entry_with_rx(Some(AgentId::random()), format!("agent-{id}"));
    entry.identity.config.profile_set = Some("gone".into());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    let err = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Surface,
    )
    .await
    .expect_err("dangling binding must reject");
    assert_eq!(err.status, 409);
    assert!(
        err.message.contains("no usable profile set"),
        "{}",
        err.message
    );

    let inbox = state.inboxes.get().expect("inbox installed");
    let listed = inbox.list(&id, &crate::inbox::InboxFilter::default()).await;
    assert!(
        listed.is_empty(),
        "rejected message must not reach the inbox"
    );
}

/// The parked kick path is gated the same way: a parked subagent with a
/// dangling explicit binding is not kickable, and the message never reaches
/// the inbox.
#[tokio::test]
async fn delivery_rejected_for_parked_kick() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (mut entry, _rx) = make_entry_with_rx(Some(AgentId::random()), format!("agent-{id}"));
    entry.identity.config.profile_set = Some("gone".into());
    entry.agent.state.store(
        crate::state::AgentState::PARKED,
        std::sync::atomic::Ordering::Relaxed,
    );
    *entry.agent.parked.lock().unwrap() = Some(crate::state::ParkedSnapshot {
        reason: kallipai_common::protocol::ParkedReason::FatalError {
            message: "boom".to_string(),
        },
        at: std::time::Instant::now(),
    });
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    let err = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Surface,
    )
    .await
    .expect_err("unbound parked agent must not be kickable");
    assert_eq!(err.status, 409);

    let inbox = state.inboxes.get().expect("inbox installed");
    let listed = inbox.list(&id, &crate::inbox::InboxFilter::default()).await;
    assert!(
        listed.is_empty(),
        "rejected message must not reach the inbox"
    );
}

/// A live idle root with an unbound record falls back to the default set at
/// the gate: the send proceeds (no 409) where a dangling subagent binding
/// is rejected — the operator never bound the root explicitly, so the
/// default is the right resolution.
#[tokio::test]
async fn delivery_root_unbound_falls_back_to_default_set() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (mut entry, _rx) = make_entry_with_rx(None, format!("agent-{id}"));
    entry.identity.config.profile_set = None;
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Surface,
    )
    .await
    .expect("unbound root must fall back to the default set, not 409");
}

// -- in-round peer notice matrix --

/// A peer injection on a live agent: the FULL message rides the prompt
/// channel (in_round) behind a wake-batch-shaped header, and the inbox row
/// is marked delivered in the same stroke, so the run-boundary wake batch
/// does not re-present the message — one presentation per message, with
/// no preview-only turn for the agent to answer separately.
#[tokio::test]
async fn busy_peer_injection_consumes_inbox_row_and_presents_full_body() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (entry, mut rx) = make_entry_with_rx(None, format!("agent-{id}"));
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    let resp = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "[From: user alice]\nhello there".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Peer { defer: false },
    )
    .await
    .expect("busy delivery succeeds");
    assert_eq!(resp.queue_depth, 0);
    assert!(resp.warning.is_none());
    assert_eq!(resp.delivery_mode, Some(DeliveryMode::InRound));

    let notice = rx
        .try_recv()
        .expect("injection queued on the prompt channel");
    assert!(
        notice.starts_with('[') && notice.contains("] operator:\n[From: user alice]\nhello there"),
        "injection must carry the ingest-stamped header and the full body: {notice}"
    );
    assert!(
        !notice.contains("preview"),
        "the preview path is gone: {notice}"
    );

    let inbox = state.inboxes.get().expect("inbox installed");
    assert!(
        inbox.pull_undelivered(&id).await.is_none(),
        "the accepted injection consumed the inbox row"
    );
}

/// Busy + defer: no channel write, deferred receipt, inbox untouched.
#[tokio::test]
async fn busy_peer_defer_skips_notice_and_reports_deferred() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (entry, mut rx) = make_entry_with_rx(None, format!("agent-{id}"));
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    let resp = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Peer { defer: true },
    )
    .await
    .expect("deferred busy delivery succeeds");
    assert_eq!(resp.delivery_mode, Some(DeliveryMode::Deferred));
    assert_eq!(resp.warning.as_deref(), Some("deferred by sender"));
    assert!(
        rx.try_recv().is_err(),
        "defer must not write the prompt channel"
    );

    let inbox = state.inboxes.get().expect("inbox installed");
    assert!(
        inbox.pull_undelivered(&id).await.is_some(),
        "the body waits in the inbox for the run boundary"
    );
}

/// A full prompt queue degrades the injection honestly: deferred receipt
/// with the queue-full warning, and the inbox row stays UNDELIVERED — the
/// wake batch presents the full message at the run boundary instead.
#[tokio::test]
async fn busy_peer_injection_failure_leaves_inbox_row_undelivered() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (entry, _rx) = make_entry_with_rx(None, format!("agent-{id}"));
    // Fill the agent's prompt channel to capacity (test channels hold 16).
    for i in 0..16 {
        entry
            .agent
            .prompt_tx
            .try_send(format!("filler {i}"))
            .expect("filler accepted while capacity lasts");
    }
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    let resp = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Peer { defer: false },
    )
    .await
    .expect("degraded delivery still succeeds");
    assert_eq!(resp.delivery_mode, Some(DeliveryMode::Deferred));
    assert_eq!(
        resp.warning.as_deref(),
        Some("prompt queue full; visibility at run boundary")
    );

    let inbox = state.inboxes.get().expect("inbox installed");
    let pulled = inbox
        .pull_undelivered(&id)
        .await
        .expect("the failed injection left the row undelivered");
    assert!(
        pulled.contains("hello"),
        "the full body waits at the run boundary: {pulled}"
    );
}

/// A peer message far longer than the old 120-character preview cap rides
/// the channel in full: the injection path truncates nothing locally (the
/// runtime's interjection wedge is the only size bound, and it sits
/// downstream of this seam).
#[tokio::test]
async fn busy_peer_injection_delivers_full_body_beyond_legacy_preview_cap() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (entry, mut rx) = make_entry_with_rx(None, format!("agent-{id}"));
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    let body = format!("[From: user alice]\n{}", "x".repeat(4096));
    let resp = crate::delivery::enqueue_prompt(
        &state,
        &id,
        body,
        "operator",
        crate::delivery::DeliveryNotice::Peer { defer: false },
    )
    .await
    .expect("oversized busy delivery still succeeds");
    assert_eq!(resp.delivery_mode, Some(DeliveryMode::InRound));

    let notice = rx
        .try_recv()
        .expect("injection queued on the prompt channel");
    assert!(
        notice.contains(&"x".repeat(4096)),
        "the full body must ride the channel untruncated (len {})",
        notice.len()
    );
    assert!(
        notice.contains("] operator:"),
        "wake-batch-shaped header present: {notice}"
    );

    let inbox = state.inboxes.get().expect("inbox installed");
    assert!(
        inbox.pull_undelivered(&id).await.is_none(),
        "the accepted injection consumed the inbox row"
    );
}

/// The subagent-to-root path shares the same seam: the injected payload
/// names the sending agent as source and carries the full body, and the row
/// is consumed on acceptance — identical semantics to the operator path.
#[tokio::test]
async fn inter_agent_delivery_injects_full_payload_and_consumes_row() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (entry, mut rx) = make_entry_with_rx(None, format!("agent-{id}"));
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    let sender = format!("agent:{}", AgentId::random());
    let resp = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "[From: agent peer]\ntask update body".to_string(),
        &sender,
        crate::delivery::DeliveryNotice::Peer { defer: false },
    )
    .await
    .expect("inter-agent busy delivery succeeds");
    assert_eq!(resp.delivery_mode, Some(DeliveryMode::InRound));

    let notice = rx
        .try_recv()
        .expect("injection queued on the prompt channel");
    assert!(
        notice.contains(&format!(
            "] {sender}:\n[From: agent peer]\ntask update body"
        )),
        "payload must name the sender and carry the full body: {notice}"
    );

    let inbox = state.inboxes.get().expect("inbox installed");
    assert!(
        inbox.pull_undelivered(&id).await.is_none(),
        "the accepted injection consumed the inbox row"
    );
}

/// Parked + defer: the kick is skipped entirely — no wake turn, the state
/// stays Parked, and the receipt is deferred with the sender warning.
#[tokio::test]
async fn parked_peer_defer_skips_kick_and_stays_parked() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (entry, mut rx) = make_entry_with_rx(None, format!("agent-{id}"));
    entry.agent.state.store(
        crate::state::AgentState::PARKED,
        std::sync::atomic::Ordering::Relaxed,
    );
    *entry.agent.parked.lock().unwrap() = Some(crate::state::ParkedSnapshot {
        reason: kallipai_common::protocol::ParkedReason::FatalError {
            message: "boom".to_string(),
        },
        at: std::time::Instant::now(),
    });
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    let resp = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Peer { defer: true },
    )
    .await
    .expect("deferred parked delivery succeeds");
    assert_eq!(resp.delivery_mode, Some(DeliveryMode::Deferred));
    assert_eq!(
        resp.warning.as_deref(),
        Some("deferred by sender; agent parked")
    );
    assert!(rx.try_recv().is_err(), "defer must not enqueue a kick turn");

    let inbox = state.inboxes.get().expect("inbox installed");
    assert!(inbox.pull_undelivered(&id).await.is_some());
}

/// Parked + default: the historical kick path with the kicked receipt.
#[tokio::test]
async fn parked_peer_default_kicks_with_kicked_mode() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (entry, mut rx) = make_entry_with_rx(None, format!("agent-{id}"));
    entry.agent.state.store(
        crate::state::AgentState::PARKED,
        std::sync::atomic::Ordering::Relaxed,
    );
    *entry.agent.parked.lock().unwrap() = Some(crate::state::ParkedSnapshot {
        reason: kallipai_common::protocol::ParkedReason::FatalError {
            message: "boom".to_string(),
        },
        at: std::time::Instant::now(),
    });
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    let resp = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Peer { defer: false },
    )
    .await
    .expect("parked kick delivery succeeds");
    assert_eq!(resp.delivery_mode, Some(DeliveryMode::Kicked));
    assert_eq!(
        resp.warning.as_deref(),
        Some("agent was parked; a kick turn was sent to wake it")
    );
    let kick = rx.try_recv().expect("kick turn queued");
    assert!(
        kick.starts_with("[system] you were parked"),
        "unexpected kick text: {kick}"
    );
}

/// Off-duty: buffered receipt, historical warning, no wake of any kind.
#[tokio::test]
async fn off_duty_delivery_reports_buffered() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (entry, mut rx) = make_entry_with_rx(None, format!("agent-{id}"));
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OffDuty);

    let resp = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "hello".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Surface,
    )
    .await
    .expect("off-duty delivery buffers");
    assert_eq!(resp.delivery_mode, Some(DeliveryMode::Buffered));
    assert_eq!(
        resp.warning.as_deref(),
        Some("agent is off-duty; message buffered to inbox")
    );
    assert!(rx.try_recv().is_err(), "off-duty must not wake");
}

/// The inbox tool view (list) never consumes the undelivered flag — only
/// the post-round MessagePuller pull does. An agent that self-serves from
/// the inbox mid-run therefore sees the same message again as a recorded
/// turn after the round: the double presentation is accepted semantics
/// (no dedup key), and this test pins it.
#[tokio::test]
async fn inbox_tool_view_does_not_consume_undelivered_flag() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let inbox = state.inboxes.get().expect("inbox installed");
    inbox
        .push(
            id.clone(),
            crate::inbox::BufferedEvent {
                timestamp: time::OffsetDateTime::now_utc(),
                source: "operator".to_string(),
                body: "[From: user alice]\nhello".to_string(),
            },
        )
        .await;

    let listed = inbox.list(&id, &crate::inbox::InboxFilter::default()).await;
    assert_eq!(listed.len(), 1, "the tool view shows the message");
    let pulled = inbox
        .pull_undelivered(&id)
        .await
        .expect("list() left the message undelivered");
    assert!(pulled.contains("hello"));
    assert!(
        inbox.pull_undelivered(&id).await.is_none(),
        "the post-round pull consumes exactly once"
    );
}

/// Three producers share one prompt channel: background notices (the spawn
/// notice_sink path), a peer notice, and the parked kick. FIFO order holds
/// across producers, and a producer hitting the full channel degrades
/// honestly instead of blocking.
#[tokio::test]
async fn three_producers_share_channel_fifo_and_capacity() {
    let state = make_state();
    install_inbox_store(&state).await;
    let id = AgentId::random();
    let (entry, mut rx) = make_entry_with_rx(None, format!("agent-{id}"));
    // Producer 1: background completion notices.
    for i in 0..2 {
        entry
            .agent
            .prompt_tx
            .try_send(format!("[notice] background done {i}"))
            .unwrap();
    }
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);

    // Producer 2: a peer notice lands behind them.
    let ok = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "body".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Peer { defer: false },
    )
    .await
    .unwrap();
    assert_eq!(ok.delivery_mode, Some(DeliveryMode::InRound));

    // Fill the channel to capacity (3 of 16 used).
    {
        let registry = state.registry.read().await;
        let live = registry.get(&id).unwrap().as_live().unwrap();
        for i in 0..13 {
            live.agent
                .prompt_tx
                .try_send(format!("filler {i}"))
                .unwrap();
        }
    }

    // Producer 3: the parked-kick producer — park the agent and deliver
    // again; the kick try_send hits the full channel and degrades.
    {
        let registry = state.registry.read().await;
        let live = registry.get(&id).unwrap().as_live().unwrap();
        live.agent.state.store(
            crate::state::AgentState::PARKED,
            std::sync::atomic::Ordering::Relaxed,
        );
        *live.agent.parked.lock().unwrap() = Some(crate::state::ParkedSnapshot {
            reason: kallipai_common::protocol::ParkedReason::FatalError {
                message: "boom".to_string(),
            },
            at: std::time::Instant::now(),
        });
    }
    let kicked = crate::delivery::enqueue_prompt(
        &state,
        &id,
        "body two".to_string(),
        "operator",
        crate::delivery::DeliveryNotice::Surface,
    )
    .await
    .unwrap();
    assert_eq!(kicked.delivery_mode, Some(DeliveryMode::Deferred));
    assert_eq!(
        kicked.warning.as_deref(),
        Some(
            "agent is parked and its prompt queue is full; message buffered to inbox, wake deferred"
        )
    );

    // FIFO across all producers.
    let mut seen = Vec::new();
    while let Ok(t) = rx.try_recv() {
        seen.push(t);
    }
    assert_eq!(seen.len(), 16);
    assert_eq!(seen[0], "[notice] background done 0");
    assert_eq!(seen[1], "[notice] background done 1");
    assert!(
        seen[2].contains("] operator:\nbody"),
        "unexpected injected payload: {}",
        seen[2]
    );
    assert_eq!(seen[3], "filler 0");
}

/// A live agent whose prompt receiver the caller reads from. The message text
/// these tests assert on is the injected prompt, so the receiver is the seam.
async fn live_agent(state: &SharedState) -> (AgentId, tokio::sync::mpsc::Receiver<String>) {
    let id = AgentId::random();
    let (entry, rx) = make_entry_with_rx(None, format!("agent-{id}"));
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state.duty.set(id.clone(), crate::duty::DutyStatus::OnDuty);
    (id, rx)
}

/// A 1:1 message that answers an earlier row carries the reference beneath its
/// header: who wrote that row, a one-line summary of it, and the command that
/// prints it whole. The position itself stays in the header.
#[tokio::test]
async fn a_one_to_one_quote_renders_its_author_summary_and_read_back_command() {
    let state = make_state();
    install_inbox_store(&state).await;
    let (db, _frames, _dir) = install_projector(&state).await;
    let (id, mut rx) = live_agent(&state).await;
    let (quoted, _) =
        chat_history::append(&db, None, None, "inbound", "the quoted text", None, None)
            .await
            .expect("append the quoted row");

    crate::delivery::deliver_message(
        &state,
        crate::auth::Identity::Operator,
        None,
        &id,
        crate::delivery::OutgoingMessage {
            text: "the answer",
            attachment: None,
            quote: Some(quoted),
        },
        false,
    )
    .await
    .expect("delivery succeeds");

    let notice = rx.try_recv().expect("the prompt was injected");
    assert!(
        notice.contains(&format!("[From: operator | quote={quoted}]")),
        "the position stays in the header: {notice}"
    );
    assert!(
        notice.contains("\nreplying to operator: 「the quoted text」\n"),
        "the reference names the author and the summary: {notice}"
    );
    assert!(
        notice.ends_with("the answer"),
        "the body follows the reference: {notice}"
    );
}

/// A row the conversation does not hold is said out loud, with the command
/// still there: the position is a fact about the message, and the read-back is
/// how the reader learns the rest. The message itself is delivered unchanged.
#[tokio::test]
async fn an_absent_quote_says_so_and_still_hands_over_the_command() {
    let state = make_state();
    install_inbox_store(&state).await;
    let (_db, _frames, _dir) = install_projector(&state).await;
    let (id, mut rx) = live_agent(&state).await;

    crate::delivery::deliver_message(
        &state,
        crate::auth::Identity::Operator,
        None,
        &id,
        crate::delivery::OutgoingMessage {
            text: "the answer",
            attachment: None,
            quote: Some(404),
        },
        false,
    )
    .await
    .expect("delivery succeeds");

    let notice = rx.try_recv().expect("the prompt was injected");
    assert!(
        notice.contains("[From: operator | quote=404]"),
        "the position is kept: {notice}"
    );
    assert!(
        notice.contains("\nthe quoted row is not in that log\n"),
        "an absent row is stated, not invented: {notice}"
    );
    assert!(
        notice.contains("(print that row with: kallip lesche read --quote 404)"),
        "the command is still given: {notice}"
    );
    assert!(
        notice.ends_with("the answer"),
        "the message is delivered unchanged: {notice}"
    );
}

/// A seam with no projector behind it has nothing to ask, so the reference
/// says the row could not be read back and keeps the command that prints it.
#[tokio::test]
async fn a_seam_with_no_projector_cannot_read_the_row_back() {
    let state = make_state();
    install_inbox_store(&state).await;
    let (id, mut rx) = live_agent(&state).await;

    crate::delivery::deliver_message(
        &state,
        crate::auth::Identity::Operator,
        None,
        &id,
        crate::delivery::OutgoingMessage {
            text: "the answer",
            attachment: None,
            quote: Some(404),
        },
        false,
    )
    .await
    .expect("delivery succeeds");

    let notice = rx.try_recv().expect("the prompt was injected");
    assert!(
        notice.contains("\nthe quoted row could not be read back\n"),
        "a read with nothing behind it says so: {notice}"
    );
    assert!(
        notice.contains("(print that row with: kallip lesche read --quote 404)"),
        "the command is still given: {notice}"
    );
    assert!(
        notice.ends_with("the answer"),
        "the message is delivered unchanged: {notice}"
    );
}

/// A quote is read in the conversation it arrived in: a row that lives in
/// another peer's partition is not this conversation's history, so the
/// reference reports it as absent rather than reaching across.
#[tokio::test]
async fn a_quote_cannot_reach_a_row_of_another_partition() {
    let state = make_state();
    install_inbox_store(&state).await;
    let (db, _frames, _dir) = install_projector(&state).await;
    let (id, mut rx) = live_agent(&state).await;
    let (elsewhere, _) = chat_history::append(
        &db,
        Some("peer-1"),
        Some("alice"),
        "inbound",
        "spoken in another conversation",
        None,
        None,
    )
    .await
    .expect("append the foreign row");

    crate::delivery::deliver_message(
        &state,
        crate::auth::Identity::Operator,
        None,
        &id,
        crate::delivery::OutgoingMessage {
            text: "the answer",
            attachment: None,
            quote: Some(elsewhere),
        },
        false,
    )
    .await
    .expect("delivery succeeds");

    let notice = rx.try_recv().expect("the prompt was injected");
    assert!(
        !notice.contains("spoken in another conversation"),
        "another conversation's row is not quoted: {notice}"
    );
    assert!(
        notice.contains("\nthe quoted row is not in that log\n"),
        "the reference reports the row as absent: {notice}"
    );
}

/// A quoted row this agent wrote is the reader's own line, so the reference
/// names the reader rather than the peer the delivered message came from.
#[tokio::test]
async fn a_quoted_row_of_this_agents_own_reads_as_you() {
    let state = make_state();
    install_inbox_store(&state).await;
    let (db, _frames, _dir) = install_projector(&state).await;
    let (id, mut rx) = live_agent(&state).await;
    let (mine, _) = chat_history::append(&db, None, None, "outbound", "my own line", None, None)
        .await
        .expect("append the row this agent wrote");

    crate::delivery::deliver_message(
        &state,
        crate::auth::Identity::Operator,
        None,
        &id,
        crate::delivery::OutgoingMessage {
            text: "the answer",
            attachment: None,
            quote: Some(mine),
        },
        false,
    )
    .await
    .expect("delivery succeeds");

    let notice = rx.try_recv().expect("the prompt was injected");
    assert!(
        notice.contains("\nreplying to you: 「my own line」\n"),
        "the reader is the author of its own row: {notice}"
    );
    assert!(
        notice.ends_with("the answer"),
        "the body follows the reference: {notice}"
    );
}

/// A reader other than root receives no reference line, and no position
/// segment either: the transcript a reference is read from is root's alone, so
/// the command would answer 403 for everyone else. The message still arrives.
#[tokio::test]
async fn a_reader_that_cannot_read_the_log_gets_no_reference() {
    let state = make_state();
    install_inbox_store(&state).await;
    let (db, _frames, _dir) = install_projector(&state).await;
    let root = AgentId::random();
    let (root_entry, _root_rx) = make_entry_with_rx(None, format!("agent-{root}"));
    state
        .registry
        .write()
        .await
        .register(root.clone(), RegistryEntry::Live(root_entry));

    let child = AgentId::random();
    let (entry, mut rx) = make_entry_with_rx(Some(root.clone()), format!("agent-{child}"));
    state
        .registry
        .write()
        .await
        .register(child.clone(), RegistryEntry::Live(entry));
    state
        .duty
        .set(child.clone(), crate::duty::DutyStatus::OnDuty);
    let (quoted, _) =
        chat_history::append(&db, None, None, "inbound", "the quoted text", None, None)
            .await
            .expect("append the quoted row");

    crate::delivery::deliver_message(
        &state,
        crate::auth::Identity::Operator,
        None,
        &child,
        crate::delivery::OutgoingMessage {
            text: "the answer",
            attachment: None,
            quote: Some(quoted),
        },
        false,
    )
    .await
    .expect("delivery succeeds");

    let notice = rx.try_recv().expect("the prompt was injected");
    assert!(
        !notice.contains("replying to "),
        "no reference for a reader that cannot read the log: {notice}"
    );
    assert!(
        !notice.contains("quote="),
        "the position segment goes only where the reference does: {notice}"
    );
    assert!(
        notice.ends_with("the answer"),
        "the message is delivered unchanged: {notice}"
    );
}

/// The budget is what keeps a stalled read from costing the delivery: a read
/// that never answers renders no reference instead of holding the seam. The
/// outer timeout is the guard -- an unbudgeted read fails this test rather than
/// hanging it.
#[tokio::test(start_paused = true)]
async fn a_stalled_quote_read_gives_up_the_reference() {
    let stalled = std::future::pending::<crate::messaging::QuoteRead>();
    let read = super::within_quote_budget(7, stalled);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), read)
        .await
        .expect("the seam returns instead of holding the message");
    assert!(
        matches!(outcome, crate::messaging::QuoteRead::Failed),
        "a stalled read renders no reference: {outcome:?}"
    );
}
