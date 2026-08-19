use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use kallip_common::agentid::AgentId;
use kallip_common::policy::{ExecPolicy, PolicyPreset};
use kallip_common::protocol::{AgentState, FailoverChainExhaustion, SseEvent};
use kallip_runtime::event::AgentEvent;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::state::RegistryEntry;
use crate::test_helpers::*;

/// Helper: receive a notification from the prompt channel within a timeout.
async fn recv_notification(rx: &mut tokio::sync::mpsc::Receiver<String>) -> String {
    match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
        Ok(Some(text)) => text,
        Ok(None) => panic!("prompt channel closed unexpectedly"),
        Err(_) => panic!("timed out waiting for notification"),
    }
}

// -- Lifecycle: exit on channel close (primary) and on cancel (forced) --

/// Regression: the bridge must exit when the agent task drops its sender
/// (per-agent remove / interrupt), not park waiting for the tagma-wide
/// cancel token. Before the fix, `recv()` resolving to `None` disabled the
/// `Some` branch while the `cancel` arm stayed Pending, so the bridge hung
/// until the shutdown bound force-aborted it — the "agent did not shut down
/// in time" warning on remove.
#[tokio::test]
async fn bridge_exits_when_agent_channel_closes() {
    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
    // Tagma-wide token, deliberately NOT cancelled: per-agent cancellation
    // must reach the bridge solely via the channel closing.
    let cancel = CancellationToken::new();
    let state = Arc::new(AtomicU8::new(AgentState::BUSY));

    let bridge = tokio::spawn(super::bridge_task(
        AgentId::random(),
        agent_rx,
        events_tx,
        cancel,
        state.clone(),
        Arc::new(std::sync::Mutex::new(String::new())),
        make_state(),
    ));

    // Simulate the agent task finishing and dropping its sender.
    drop(agent_tx);

    // Promptness matters: the bug parked for ~10s. A generous bound here
    // would let a future regression that re-introduces a seconds-long park
    // slip through.
    let exited = tokio::time::timeout(Duration::from_millis(100), bridge)
        .await
        .is_ok();
    assert!(exited, "bridge did not exit after the agent channel closed");
    assert_eq!(state.load(Ordering::Relaxed), AgentState::IDLE);
}

/// A terminal event clears the ephemeral activity cell, so a stale "reading
/// docs" does not persist while the agent is idle.
#[tokio::test]
async fn bridge_clears_activity_on_terminal_event() {
    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    let state = Arc::new(AtomicU8::new(AgentState::BUSY));
    let activity = Arc::new(std::sync::Mutex::new("reading docs".to_owned()));

    let bridge = tokio::spawn(super::bridge_task(
        AgentId::random(),
        agent_rx,
        events_tx,
        cancel,
        state.clone(),
        activity.clone(),
        make_state(),
    ));

    // Drive a terminal event, then close the channel so the bridge exits.
    agent_tx.send(AgentEvent::Idle).await.ok();
    drop(agent_tx);
    let exited = tokio::time::timeout(Duration::from_millis(200), bridge)
        .await
        .is_ok();
    assert!(exited, "bridge did not exit");

    assert_eq!(state.load(Ordering::Relaxed), AgentState::IDLE);
    assert!(
        activity.lock().unwrap().is_empty(),
        "activity must be cleared on terminal event"
    );
}

/// Forced shutdown via the tagma-wide cancel (preserved shutdown path). The
/// agent channel is kept OPEN so `recv()` stays Pending and only the cancel
/// arm can fire — isolating that path from the channel-closed path.
#[tokio::test]
async fn bridge_exits_on_cancel() {
    let (_agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    let state = Arc::new(AtomicU8::new(AgentState::BUSY));

    let bridge = tokio::spawn(super::bridge_task(
        AgentId::random(),
        agent_rx,
        events_tx,
        cancel.clone(),
        state.clone(),
        Arc::new(std::sync::Mutex::new(String::new())),
        make_state(),
    ));

    cancel.cancel();

    let exited = tokio::time::timeout(Duration::from_millis(100), bridge)
        .await
        .is_ok();
    assert!(exited, "bridge did not exit on cancel");
    assert_eq!(state.load(Ordering::Relaxed), AgentState::IDLE);
}

/// Load-bearing invariant: when the agent task emits its terminal `Cancelled`
/// and then drops the sender, the bridge must forward `Cancelled` to SSE
/// subscribers *before* exiting. This is the reason the bridge keys off
/// channel-close rather than the agent's child cancel token (see the
/// `bridge_task` lifecycle note): watching the child token would make the
/// cancel arm preempt and lose this terminal event.
#[tokio::test]
async fn bridge_delivers_terminal_cancelled_before_exit() {
    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, mut events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    let state = Arc::new(AtomicU8::new(AgentState::IDLE));

    let bridge = tokio::spawn(super::bridge_task(
        AgentId::random(),
        agent_rx,
        events_tx,
        cancel,
        state.clone(),
        Arc::new(std::sync::Mutex::new(String::new())),
        make_state(),
    ));

    // Agent task emits its terminal event, then finishes (drops sender).
    agent_tx.send(AgentEvent::Cancelled).await.unwrap();
    drop(agent_tx);

    tokio::time::timeout(Duration::from_millis(100), bridge)
        .await
        .expect("bridge did not exit within bound")
        .unwrap(); // propagate any bridge task panic

    let mut saw_cancelled = false;
    while let Ok(ev) = events_rx.try_recv() {
        if matches!(ev, SseEvent::Cancelled) {
            saw_cancelled = true;
        }
    }
    assert!(saw_cancelled, "terminal Cancelled event was not delivered");
}

/// On `AgentEvent::Interrupted` the bridge sets state IDLE and **stays alive** —
/// `Interrupted` is non-terminal: the bridge forwards it, sets state IDLE, and keeps
/// looping — proven by it then forwarding a subsequent `Finished` on the same channel.
#[tokio::test]
async fn bridge_interrupted_keeps_looping() {
    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, mut events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    let state = Arc::new(AtomicU8::new(AgentState::BUSY));

    let _bridge = tokio::spawn(super::bridge_task(
        AgentId::random(),
        agent_rx,
        events_tx,
        cancel,
        state.clone(),
        Arc::new(std::sync::Mutex::new(String::new())),
        make_state(),
    ));

    agent_tx.send(AgentEvent::Interrupted).await.unwrap();
    agent_tx.send(AgentEvent::Idle).await.unwrap();

    // Drain forwarded events until both are seen (the bridge looped past Interrupted).
    let mut saw_interrupted = false;
    let mut saw_idle = false;
    while !(saw_interrupted && saw_idle) {
        match tokio::time::timeout(Duration::from_millis(200), events_rx.recv()).await {
            Ok(Ok(SseEvent::Interrupted)) => saw_interrupted = true,
            Ok(Ok(SseEvent::Idle)) => saw_idle = true,
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break, // channel closed
            Err(_) => break,     // timeout
        }
    }
    assert!(saw_interrupted, "Interrupted was not forwarded");
    assert!(
        saw_idle,
        "Idle was not forwarded — bridge did not keep looping after Interrupted"
    );
    assert_eq!(state.load(Ordering::Relaxed), AgentState::IDLE);
    drop(agent_tx);
}

#[tokio::test]
async fn notification_delivered_to_direct_superior_inbox() {
    // The approval request is pushed to the superior's inbox with the
    // full notification payload. Verify the content lands in the inbox.
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent);
    }

    super::route_to_superior(
        &state,
        &child,
        "approval-1".into(),
        "bash_exec".into(),
        serde_json::json!({"command": "rm -rf /tmp/x"}),
        "test reason",
    )
    .await;

    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(msg.contains(&child.to_string()), "names the subordinate");
    assert!(msg.contains("bash_exec"), "names the tool");
    assert!(msg.contains("rm -rf /tmp/x"), "includes the arguments");
    assert!(msg.contains("test reason"), "includes the commit reason");
    assert!(msg.contains("approval-1"), "includes the action id");
    assert!(
        msg.contains("re-checked at approval time"),
        "carries the static review guidance"
    );
}

#[tokio::test]
async fn no_notification_when_agent_has_no_superior() {
    // A root agent has no superior to notify; the function returns early.
    let state = make_state();
    let root = AgentId::random();

    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &root);
    }

    super::route_to_superior(
        &state,
        &root,
        "approval-2".into(),
        "some_tool".into(),
        serde_json::json!({}),
        "test reason",
    )
    .await;

    // No notification should be sent — verifies it completes without panic
    // and (implicitly) does not block on a channel that does not exist.
}

#[tokio::test]
async fn no_notification_when_superior_is_faulted() {
    // A faulted direct superior has no prompt channel; routing is skipped
    // rather than panicking.
    let state = make_state();
    let root = AgentId::random();
    let child = AgentId::random();

    {
        let mut reg = state.registry.write().await;
        add_faulted_root(&mut reg, &root, "restore failed");
        add_sub(&mut reg, &child, &root);
    }

    super::route_to_superior(
        &state,
        &child,
        "approval-3".into(),
        "bash_exec".into(),
        serde_json::json!({}),
        "test reason",
    )
    .await;

    // Completes without panic; no channel to receive from either way.
}

/// When the superior is off-duty, the approval notification is buffered to
/// their inbox instead of being delivered to the prompt channel.
#[tokio::test]
async fn approval_notification_buffered_when_superior_off_duty() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, mut prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent);
    }

    // Set the superior off-duty.
    state
        .duty
        .set(parent.clone(), crate::duty::DutyStatus::OffDuty);

    super::route_to_superior(
        &state,
        &child,
        "approval-off-duty".into(),
        "bash_exec".into(),
        serde_json::json!({"command": "echo hi"}),
        "test reason",
    )
    .await;

    // Nothing delivered to the prompt channel.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), prompt_rx.recv())
            .await
            .is_err(),
        "no notification should reach an off-duty superior"
    );
    // The notification IS in the inbox.
    assert_eq!(state.inboxes.get().unwrap().len_for(&parent).await, 1);
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(
        msg.contains("Approval Request"),
        "inbox should contain the approval: {msg}"
    );
}

// -- Subagent idle notification --

/// When a subagent goes idle, the superior's inbox receives a concise
/// `[Subagent Idle]` notification naming the agent and its role.
#[tokio::test]
async fn subagent_idle_notifies_superior() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    let (mut child_entry, _) = make_entry_with_policy_rx(
        Some(parent.clone()),
        format!("agent-{child}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    child_entry.identity.config.role = "reviewer".into();
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        reg.register(child.clone(), RegistryEntry::Live(child_entry));
    }

    let notice = super::snapshot_idle_notice(&state, &child).await;
    super::deliver_idle_notice(&state, &child, "Subagent Idle", "is now idle.", notice).await;

    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(
        msg.contains("[Subagent Idle]") && !msg.contains("[[Subagent"),
        "single-bracket prefix: {msg}"
    );
    assert!(
        msg.contains(&child.to_string()),
        "names the subagent: {msg}"
    );
    assert!(msg.contains("(role: reviewer)"), "includes role: {msg}");
    assert!(msg.contains("is now idle"), "states idle: {msg}");
}

/// A subagent with no configured role renders no "(role: ...)" suffix
/// rather than an empty "(role: )".
#[tokio::test]
async fn idle_notification_without_role_omits_suffix() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent); // add_sub leaves role empty
    }

    let notice = super::snapshot_idle_notice(&state, &child).await;
    super::deliver_idle_notice(&state, &child, "Subagent Idle", "is now idle.", notice).await;

    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(
        msg.contains("[Subagent Idle]") && !msg.contains("[[Subagent"),
        "single-bracket prefix: {msg}"
    );
    assert!(!msg.contains("(role:"), "no empty-role suffix: {msg}");
}

/// A root agent (no `created_by`) going idle does not trigger a
/// notification — there is no superior to notify.
#[tokio::test]
async fn root_idle_does_not_notify() {
    let state = make_state();
    install_inbox_store(&state).await;
    let root = AgentId::random();

    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &root);
    }

    let notice = super::snapshot_idle_notice(&state, &root).await;
    super::deliver_idle_notice(&state, &root, "Subagent Idle", "is now idle.", notice).await;

    assert_eq!(state.inboxes.get().unwrap().len_for(&root).await, 0);
}

/// When the superior is off-duty, the idle notification is buffered to
/// their inbox instead of triggering an immediate wake.
#[tokio::test]
async fn idle_notification_buffered_when_superior_off_duty() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent);
    }

    state
        .duty
        .set(parent.clone(), crate::duty::DutyStatus::OffDuty);

    let notice = super::snapshot_idle_notice(&state, &child).await;
    super::deliver_idle_notice(&state, &child, "Subagent Idle", "is now idle.", notice).await;

    assert_eq!(state.inboxes.get().unwrap().len_for(&parent).await, 1);
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(
        msg.contains("[Subagent Idle]") && !msg.contains("[[Subagent"),
        "single-bracket prefix: {msg}"
    );
}

/// bridge_task-level dispatch of `AgentEvent::Idle`: the full
/// select!/match path must BOTH mark the agent idle (state + activity
/// cleared) AND notify the superior (inbox message + wake) — the
/// helper-level tests above cover each half in isolation.
#[tokio::test]
async fn bridge_dispatches_idle_to_superior() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    let parent_notify = parent_entry.agent.notify.clone();
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent);
    }

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    let atomic_state = Arc::new(AtomicU8::new(AgentState::BUSY));
    let activity = Arc::new(std::sync::Mutex::new("reading docs".to_string()));
    tokio::spawn(super::bridge_task(
        child.clone(),
        agent_rx,
        events_tx,
        cancel,
        atomic_state.clone(),
        activity.clone(),
        state.clone(),
    ));

    // Waiter registered before the event so the wake is observable even
    // if it fires before this task first polls (tokio stores a permit).
    let mut wake = std::pin::pin!(parent_notify.notified());
    agent_tx.send(AgentEvent::Idle).await.unwrap();

    let mut settled = false;
    for _ in 0..40 {
        if atomic_state.load(Ordering::Relaxed) == AgentState::IDLE
            && activity.lock().unwrap().is_empty()
            && state.inboxes.get().unwrap().len_for(&parent).await == 1
        {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        settled,
        "idle dispatch must mark idle and inbox the notification"
    );
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(
        msg.contains("[Subagent Idle]") && !msg.contains("[[Subagent"),
        "single-bracket prefix: {msg}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(500), wake.as_mut())
            .await
            .is_ok(),
        "on-duty superior must be woken"
    );
    drop(agent_tx); // let the bridge observe the channel close and exit
}

/// When the superior is off-duty, the idle dispatch still buffers the
/// notification to their inbox but must NOT wake them.
#[tokio::test]
async fn bridge_idle_dispatch_off_duty_superior_not_woken() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    let parent_notify = parent_entry.agent.notify.clone();
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent);
    }
    state
        .duty
        .set(parent.clone(), crate::duty::DutyStatus::OffDuty);

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    tokio::spawn(super::bridge_task(
        child.clone(),
        agent_rx,
        events_tx,
        cancel,
        Arc::new(AtomicU8::new(AgentState::BUSY)),
        Arc::new(std::sync::Mutex::new(String::new())),
        state.clone(),
    ));

    let mut wake = std::pin::pin!(parent_notify.notified());
    agent_tx.send(AgentEvent::Idle).await.unwrap();

    // Wait for the inbox push (delivery's observable half)...
    let mut delivered = false;
    for _ in 0..40 {
        if state.inboxes.get().unwrap().len_for(&parent).await == 1 {
            delivered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        delivered,
        "off-duty dispatch must still buffer the notification"
    );
    // ...then confirm no wake arrives after delivery: the waiter stays
    // pending (no permit is stored by the duty-gated path).
    assert!(
        tokio::time::timeout(Duration::from_millis(300), wake.as_mut())
            .await
            .is_err(),
        "off-duty superior must not be woken"
    );
    drop(agent_tx);
}

// -- B①: park-event notifications (Error / FCE / MaxRounds) --

/// Full-bridge dispatch of `AgentEvent::Error`: the superior's inbox
/// receives a `[Subagent Error]` notification carrying the error detail.
#[tokio::test]
async fn bridge_dispatches_error_to_superior() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent);
    }

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    tokio::spawn(super::bridge_task(
        child.clone(),
        agent_rx,
        events_tx,
        cancel,
        Arc::new(AtomicU8::new(AgentState::BUSY)),
        Arc::new(std::sync::Mutex::new(String::new())),
        state.clone(),
    ));

    agent_tx
        .send(AgentEvent::Error("model endpoint returned 500".into()))
        .await
        .unwrap();
    let mut delivered = false;
    for _ in 0..40 {
        if state.inboxes.get().unwrap().len_for(&parent).await == 1 {
            delivered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(delivered, "error dispatch must notify the superior");
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(
        msg.contains("[Subagent Error]") && !msg.contains("[[Subagent"),
        "single-bracket prefix: {msg}"
    );
    assert!(
        msg.contains("hit a fatal error and parked: model endpoint returned 500"),
        "detail: {msg}"
    );
    assert!(msg.contains(&child.to_string()), "names the agent: {msg}");
    drop(agent_tx);
}

/// Full-bridge dispatch of `AgentEvent::FailoverChainExhausted`.
#[tokio::test]
async fn bridge_dispatches_failover_exhausted_to_superior() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent);
    }

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    tokio::spawn(super::bridge_task(
        child.clone(),
        agent_rx,
        events_tx,
        cancel,
        Arc::new(AtomicU8::new(AgentState::BUSY)),
        Arc::new(std::sync::Mutex::new(String::new())),
        state.clone(),
    ));

    agent_tx
        .send(AgentEvent::FailoverChainExhausted {
            reason: kallip_common::protocol::FailoverChainExhaustion::NoFailoverConfigured,
            detail: "all tiers unhealthy".into(),
            transient_retry: None,
        })
        .await
        .unwrap();
    let mut delivered = false;
    for _ in 0..40 {
        if state.inboxes.get().unwrap().len_for(&parent).await == 1 {
            delivered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(delivered, "FCE dispatch must notify the superior");
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(
        msg.contains("[Subagent Error]") && !msg.contains("[[Subagent"),
        "single-bracket prefix: {msg}"
    );
    assert!(
        msg.contains("exhausted its model failover chain and parked"),
        "body: {msg}"
    );
    assert!(msg.contains("all tiers unhealthy"), "detail: {msg}");
    drop(agent_tx);
}
/// Full-bridge dispatch of `AgentEvent::MaxRoundsExceeded`.
#[tokio::test]
async fn bridge_dispatches_max_rounds_to_superior() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent);
    }

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    tokio::spawn(super::bridge_task(
        child.clone(),
        agent_rx,
        events_tx,
        cancel,
        Arc::new(AtomicU8::new(AgentState::BUSY)),
        Arc::new(std::sync::Mutex::new(String::new())),
        state.clone(),
    ));

    agent_tx.send(AgentEvent::MaxRoundsExceeded).await.unwrap();
    let mut delivered = false;
    for _ in 0..40 {
        if state.inboxes.get().unwrap().len_for(&parent).await == 1 {
            delivered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(delivered, "max-rounds dispatch must notify the superior");
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(
        msg.contains("hit the per-request tool-round limit and parked"),
        "body: {msg}"
    );
    drop(agent_tx);
}

/// `Interrupted` parks the agent but must NOT notify the superior
/// (operator-initiated; the interrupter already knows).
#[tokio::test]
async fn bridge_interrupted_does_not_notify_superior() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (parent_entry, _prompt_rx) = make_entry_with_policy_rx(
        None,
        format!("agent-{parent}"),
        PolicyPreset::Default,
        ExecPolicy::default(),
    );
    {
        let mut reg = state.registry.write().await;
        reg.register(parent.clone(), RegistryEntry::Live(parent_entry));
        add_sub(&mut reg, &child, &parent);
    }

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
    let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
    let cancel = CancellationToken::new();
    let atomic_state = Arc::new(AtomicU8::new(AgentState::BUSY));
    tokio::spawn(super::bridge_task(
        child.clone(),
        agent_rx,
        events_tx,
        cancel,
        atomic_state.clone(),
        Arc::new(std::sync::Mutex::new(String::new())),
        state.clone(),
    ));

    agent_tx.send(AgentEvent::Interrupted).await.unwrap();
    let mut settled = false;
    for _ in 0..40 {
        if atomic_state.load(Ordering::Relaxed) == AgentState::IDLE {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(settled, "interrupted must still mark idle");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        state.inboxes.get().unwrap().len_for(&parent).await,
        0,
        "interrupted must not notify the superior"
    );
    drop(agent_tx);
}

// -- C: last-idle annotation --

/// When the second of two live siblings goes idle, its notification
/// carries the last-idle annotation; the first one's does not.
#[tokio::test]
async fn last_idle_annotation_on_final_sibling() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let c1 = AgentId::random();
    let c2 = AgentId::random();

    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &parent);
        add_sub(&mut reg, &c1, &parent);
        add_sub(&mut reg, &c2, &parent);
        reg.get(&c1)
            .unwrap()
            .as_live()
            .unwrap()
            .agent
            .state
            .store(AgentState::IDLE, Ordering::Relaxed);
        reg.get(&c2)
            .unwrap()
            .as_live()
            .unwrap()
            .agent
            .state
            .store(AgentState::BUSY, Ordering::Relaxed);
    }

    let n1 = super::snapshot_idle_notice(&state, &c1).await;
    assert!(!n1.is_last, "c1 is not last while c2 is busy");
    // The atomic must be the SAME Arc the registry holds (production
    // shares it between the bridge and the entry) so the mark is visible
    // to the snapshot.
    let c2_atomic = {
        let reg = state.registry.read().await;
        reg.get(&c2).unwrap().as_live().unwrap().agent.state.clone()
    };
    let n2 = super::mark_idle_and_snapshot(
        &state,
        &c2,
        &c2_atomic,
        &std::sync::Mutex::new(String::new()),
    )
    .await;
    assert!(n2.is_last, "c2 is the last to go idle");
    assert_eq!(n2.live_subordinates, 2);

    super::deliver_idle_notice(&state, &c2, "Subagent Idle", "is now idle.", n2).await;
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(
        msg.contains("It is the last to go idle"),
        "annotation: {msg}"
    );
    assert!(msg.contains("all 2 live subordinates"), "live count: {msg}");
}

/// A faulted sibling is excluded from the wait set: one live sibling
/// going idle is already "the last".
#[tokio::test]
async fn faulted_sibling_excluded_from_wait_set() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let c1 = AgentId::random();
    let c2 = AgentId::random();

    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &parent);
        add_sub(&mut reg, &c1, &parent);
        add_faulted_sub(&mut reg, &c2, &parent, "workspace missing");
    }

    let c1_atomic = {
        let reg = state.registry.read().await;
        reg.get(&c1).unwrap().as_live().unwrap().agent.state.clone()
    };
    let n = super::mark_idle_and_snapshot(
        &state,
        &c1,
        &c1_atomic,
        &std::sync::Mutex::new(String::new()),
    )
    .await;
    assert_eq!(n.live_subordinates, 1, "faulted sibling not counted");
    assert!(n.is_last, "sole live sibling is the last by construction");
}

/// A child whose superior is not registered gets no delivery target —
/// anomalous registry state degrades to silence, not panic.
#[tokio::test]
async fn missing_superior_yields_no_target() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    {
        let mut reg = state.registry.write().await;
        add_sub(&mut reg, &child, &parent); // parent itself never registered
    }

    let n = super::snapshot_idle_notice(&state, &child).await;
    assert!(
        n.target.is_none(),
        "no target without a registered superior"
    );
    assert!(!n.is_last);
    super::deliver_idle_notice(&state, &child, "Subagent Idle", "is now idle.", n).await;
    assert_eq!(state.inboxes.get().unwrap().len_for(&parent).await, 0);
}

/// Cross-side parity for the terminal classification: every `AgentEvent` must
/// bridge to an SSE event carrying the same `is_terminal` verdict, and only a
/// non-terminal event may map to `None`. The per-side exhaustive snapshots
/// (runtime `event.rs`, common `sse.rs`) each pin only their own side — this
/// test is the mechanical link that makes the mirror obligation real. The
/// `len()` assert is a count nail: a new variant added without extending the
/// table below fails here instead of silently escaping parity coverage.
#[test]
fn convert_event_preserves_terminal_parity() {
    assert_eq!(all_agent_events().len(), 21);
    for ev in all_agent_events() {
        let debug = format!("{ev:?}");
        let terminal = ev.is_terminal();
        match super::convert_event(ev) {
            Some(sse) => assert_eq!(
                sse.is_terminal(),
                terminal,
                "terminal parity broken for {debug}"
            ),
            None => assert!(!terminal, "terminal event {debug} bridged to None"),
        }
    }
}

/// One instance of every `AgentEvent` variant, for the parity test above
/// (mirrors the snapshot fixtures in runtime `event.rs` / common `sse.rs`,
/// but as an owned Vec because `convert_event` consumes the event).
fn all_agent_events() -> Vec<AgentEvent> {
    vec![
        AgentEvent::Reasoning(String::new()),
        AgentEvent::AssistantContent(String::new()),
        AgentEvent::AssistantContentDelta {
            delta: String::new(),
        },
        AgentEvent::ReasoningDelta {
            delta: String::new(),
        },
        AgentEvent::ToolCall {
            name: String::new(),
            args: String::new(),
        },
        AgentEvent::ToolResult(String::new()),
        AgentEvent::Idle,
        AgentEvent::MaxRoundsExceeded,
        AgentEvent::Error(String::new()),
        AgentEvent::Status(String::new()),
        AgentEvent::Busy,
        AgentEvent::ApprovalCommitted {
            id: String::new(),
            tool_name: String::new(),
            arguments: serde_json::Value::Null,
            commit_reason: String::new(),
        },
        AgentEvent::Retrying {
            attempt: 0,
            max_attempts: 0,
            error: String::new(),
            delay_secs: 0.0,
        },
        AgentEvent::StreamReset {
            error: String::new(),
            attempt: 0,
            max_attempts: 0,
            delay_secs: 0.0,
        },
        AgentEvent::Failover {
            from: String::new(),
            to: String::new(),
            reason: String::new(),
        },
        AgentEvent::ApprovalRedeemed { id: String::new() },
        AgentEvent::ApprovalCancelled { id: String::new() },
        AgentEvent::Cancelled,
        AgentEvent::Interrupted,
        AgentEvent::TokenBudgetExceeded {
            consumed: 0,
            budget: 0,
        },
        AgentEvent::FailoverChainExhausted {
            reason: FailoverChainExhaustion::NoFailoverConfigured,
            detail: String::new(),
            transient_retry: None,
        },
    ]
}
