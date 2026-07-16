use std::sync::Arc;
use std::sync::atomic::Ordering;

use kallip_common::agentid::AgentId;
use kallip_common::approval::ApprovalStatus;
use kallip_common::protocol::{AgentState, SseEvent};
use kallip_runtime::event::AgentEvent;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::state::SharedState;

/// Route one agent's runtime events to SSE subscribers (and approval requests
/// to the agent's superior).
///
/// # Lifecycle
///
/// The bridge owns the agent's event-stream receiver and exits when that stream
/// ends — i.e. when the agent task drops its sender. The channel closes only on a
/// **lifecycle** end: `remove`, daemon shutdown, or a task panic. The agent task
/// emits its terminal `Cancelled` event on the way out, the bridge forwards it,
/// then observes `recv() == None` and exits.
///
/// **Interrupt** does *not* close the channel: it cancels only the current round
/// token, so the task aborts the round, emits `Interrupted`, and returns to its
/// outer loop — the bridge forwards `Interrupted` (setting state `IDLE`) and keeps
/// looping. The agent is still alive.
///
/// The `cancel` token is a secondary, *forced* exit for daemon-wide shutdown: it
/// preempts the bridge even if the agent task is mid-operation. It is the
/// daemon-wide parent token, **not** the agent's child, deliberately. The bridge
/// must outlive the agent task's terminal `Cancelled` emit so it can forward it;
/// if the bridge watched the child token its cancel arm would fire the instant a
/// per-agent cancel is signalled — before the agent task has emitted `Cancelled` —
/// and that terminal event would be lost. Keying the bridge off the channel (not
/// the child token) is precisely what preserves it. See
/// `bridge_delivers_terminal_cancelled_before_exit`.
pub async fn bridge_task(
    agent_id: AgentId,
    mut agent_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    events_tx: broadcast::Sender<SseEvent>,
    cancel: CancellationToken,
    state: Arc<std::sync::atomic::AtomicU8>,
    activity: Arc<std::sync::Mutex<String>>,
    shared_state: SharedState,
) {
    loop {
        // `biased` with the recv arm first: on forced cancel, an already-queued
        // event (including the terminal `Cancelled`) is processed before the
        // cancel arm preempts, so SSE subscribers still see it.
        tokio::select! {
            biased;

            // Channel-closed path (primary lifecycle). The agent task is gone;
            // exit without waiting for the daemon-wide cancel, which would
            // otherwise park this task until the shutdown bound force-aborts it.
            event = agent_rx.recv() => match event {
                Some(event) => match event {
                    AgentEvent::ApprovalCommitted { id, tool_name, arguments, commit_reason } => {
                        route_to_superior(&shared_state, &agent_id, id.clone(), tool_name, arguments, &commit_reason).await;
                        events_tx.send(SseEvent::ApprovalUpdated {
                            id,
                            status: ApprovalStatus::Committed,
                        }).ok();
                    }
                    other => {
                        match &other {
                            AgentEvent::Busy => state.store(AgentState::BUSY, Ordering::Relaxed),
                            AgentEvent::Error(msg) => {
                                // Fatal LLM/runtime error. `warn!` (not `error!`):
                                // the task stays alive — only the round ended. This
                                // log is also the sole observability channel for a
                                // headless/subagent run, where the SSE event below
                                // has no subscriber and is dropped silently.
                                warn!(id = %agent_id, "agent round ended in error: {msg}");
                                mark_idle(&state, &activity);
                            }
                            AgentEvent::FailoverChainExhausted { detail, .. } => {
                                warn!(id = %agent_id, "failover chain exhausted: {detail}");
                                mark_idle(&state, &activity);
                            }
                            AgentEvent::Finished(_)
                            | AgentEvent::MaxRoundsExceeded
                            | AgentEvent::Cancelled
                            | AgentEvent::Interrupted
                            | AgentEvent::TokenBudgetExceeded { .. } => {
                                mark_idle(&state, &activity);
                            }
                            _ => {}
                        }
                        // Best-effort broadcast: with no SSE subscriber the
                        // send errors, which is the normal steady state for a
                        // headless/subagent run. Subscribe/unsubscribe state
                        // transitions are logged at the SSE endpoint, not here
                        // (logging per event would spam on every token delta).
                        if let Some(sse) = convert_event(other) {
                            let _ = events_tx.send(sse);
                        }
                    }
                },
                None => {
                    mark_idle(&state, &activity);
                    info!("bridge task: agent channel closed, exiting");
                    break;
                }
            },

            // Forced shutdown (daemon-wide only): best-effort drain of anything
            // still queued before exiting. Per-agent cancellation reaches the
            // bridge via the channel-closed path above — see the lifecycle note.
            _ = cancel.cancelled() => {
                mark_idle(&state, &activity);
                while let Ok(event) = agent_rx.try_recv() {
                    if let Some(sse) = convert_event(event) {
                        events_tx.send(sse).ok();
                    }
                }
                info!("bridge task: cancelled, exiting");
                break;
            }
        }
    }
}

/// Mark the agent idle: drop state to [`AgentState::IDLE`] and clear the ephemeral
/// activity string so a stale "reading docs" doesn't persist while idle. Shared by
/// every turn-end / terminal / shutdown path in [`bridge_task`].
fn mark_idle(state: &std::sync::atomic::AtomicU8, activity: &std::sync::Mutex<String>) {
    state.store(AgentState::IDLE, Ordering::Relaxed);
    activity.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Convert a runtime [`AgentEvent`] to a wire-format [`SseEvent`].
///
/// Returns `None` for events handled by other means (e.g., routed to superiors).
fn convert_event(event: AgentEvent) -> Option<SseEvent> {
    match event {
        AgentEvent::ApprovalCommitted { .. } => None,
        AgentEvent::ApprovalRedeemed { id } => Some(SseEvent::ApprovalUpdated {
            id,
            status: ApprovalStatus::Redeemed,
        }),
        AgentEvent::ApprovalCancelled { id } => Some(SseEvent::ApprovalUpdated {
            id,
            status: ApprovalStatus::Cancelled,
        }),
        AgentEvent::Reasoning(content) => Some(SseEvent::Reasoning { content }),
        AgentEvent::AssistantContent(content) => Some(SseEvent::AssistantContent { content }),
        AgentEvent::AssistantContentDelta { delta } => {
            Some(SseEvent::AssistantContentDelta { delta })
        }
        AgentEvent::ReasoningDelta { delta } => Some(SseEvent::ReasoningDelta { delta }),
        AgentEvent::ToolCall { name, args } => Some(SseEvent::ToolCall { name, args }),
        AgentEvent::ToolResult(result) => Some(SseEvent::ToolResult { result }),
        AgentEvent::Finished(content) => Some(SseEvent::Finished { content }),
        AgentEvent::MaxRoundsExceeded => Some(SseEvent::MaxRoundsExceeded),
        AgentEvent::Error(msg) => Some(SseEvent::Error { message: msg }),
        AgentEvent::Status(msg) => Some(SseEvent::Status { message: msg }),
        AgentEvent::Busy => Some(SseEvent::Busy),
        AgentEvent::Retrying {
            attempt,
            max_attempts,
            error,
            delay_secs,
        } => Some(SseEvent::Retrying {
            attempt,
            max_attempts,
            error,
            delay_secs,
        }),
        AgentEvent::StreamReset {
            error,
            attempt,
            max_attempts,
            delay_secs,
        } => Some(SseEvent::StreamReset {
            error,
            attempt,
            max_attempts,
            delay_secs,
        }),
        AgentEvent::Failover { from, to, reason } => Some(SseEvent::Failover { from, to, reason }),
        AgentEvent::FailoverChainExhausted { reason, detail } => {
            Some(SseEvent::FailoverChainExhausted { reason, detail })
        }
        AgentEvent::Cancelled => Some(SseEvent::Cancelled),
        AgentEvent::Interrupted => Some(SseEvent::Interrupted),
        AgentEvent::TokenBudgetExceeded { consumed, budget } => {
            Some(SseEvent::TokenBudgetExceeded { consumed, budget })
        }
    }
}

async fn route_to_superior(
    shared_state: &SharedState,
    agent_id: &AgentId,
    approval_id: String,
    tool_name: String,
    arguments: serde_json::Value,
    commit_reason: &str,
) {
    // Collect the direct superior's id + prompt channel inside the lock so we
    // don't hold it across the async send.
    //
    // The notification always targets the direct superior. There is no longer an
    // escalation walk to find an "allow" superior: with a daemon-global classify
    // preset and monotone-inherited exec-policy, if any upper superior could
    // `Allow` a deferred `bash_exec`, the direct superior can too -- so the direct
    // superior is always the sufficient routing target. (The approval-time gate
    // in `routes::approval` re-runs classify against the approver's rule-set, so
    // routing cannot smuggle a command past policy.)
    let (superior_id, prompt_tx) = {
        let registry = shared_state.registry.read().await;
        let Some(entry) = registry.get(agent_id) else {
            warn!(id = %agent_id, "agent not found in registry during superior routing");
            return;
        };
        let Some(ref superior_id) = entry.identity().config.created_by else {
            return;
        };
        let Some(superior_entry) = registry.get(superior_id) else {
            warn!(id = %superior_id, "superior not found in registry");
            return;
        };
        // The direct superior must be live to receive the notification. A faulted
        // superior has no prompt channel.
        let Some(superior_live) = superior_entry.as_live() else {
            warn!(id = %superior_id, "direct superior is faulted; cannot route approval");
            return;
        };
        (superior_id.clone(), superior_live.agent.prompt_tx.clone())
    };

    let notification = format!(
        "[Approval Request] Subordinate agent {agent_id} requests approval for:\n\
         Tool: {tool_name}\n\
         Arguments: {arguments}\n\
         Reason: {commit_reason}\n\
         Action ID: {approval_id}\n\n\
         Review the request and approve only if the action is safe. Your classify \
         rule-set is re-checked at approval time, so you cannot delegate a command \
         your own policy would gate.\n\n\
         Use `kallip approval approve {approval_id}` to approve \
         or `kallip approval deny {approval_id} <reason>` to deny."
    );
    // Non-blocking send: never stall the bridge task waiting for queue space.
    // If the superior's message queue is full, drop the notification and log a
    // warning — the superior can still query pending approvals via the API.
    match prompt_tx.try_send(notification) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            warn!(
                id = %superior_id,
                "superior message queue full, approval notification dropped (query pending approvals via API)"
            );
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            warn!(id = %superior_id, "superior message channel closed, approval notification dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::Duration;

    use kallip_common::agentid::AgentId;
    use kallip_common::policy::{ExecPolicy, PolicyPreset};
    use kallip_common::protocol::{AgentState, SseEvent};
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
    /// (per-agent remove / interrupt), not park waiting for the daemon-wide
    /// cancel token. Before the fix, `recv()` resolving to `None` disabled the
    /// `Some` branch while the `cancel` arm stayed Pending, so the bridge hung
    /// until the shutdown bound force-aborted it — the "agent did not shut down
    /// in time" warning on remove.
    #[tokio::test]
    async fn bridge_exits_when_agent_channel_closes() {
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (events_tx, _events_rx) = broadcast::channel::<SseEvent>(16);
        // Daemon-wide token, deliberately NOT cancelled: per-agent cancellation
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
        agent_tx
            .send(AgentEvent::Finished("done".into()))
            .await
            .ok();
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

    /// Forced shutdown via the daemon-wide cancel (preserved shutdown path). The
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
        agent_tx
            .send(AgentEvent::Finished("done".into()))
            .await
            .unwrap();

        // Drain forwarded events until both are seen (the bridge looped past Interrupted).
        let mut saw_interrupted = false;
        let mut saw_finished = false;
        while !(saw_interrupted && saw_finished) {
            match tokio::time::timeout(Duration::from_millis(200), events_rx.recv()).await {
                Ok(Ok(SseEvent::Interrupted)) => saw_interrupted = true,
                Ok(Ok(SseEvent::Finished { .. })) => saw_finished = true,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break, // channel closed
                Err(_) => break,     // timeout
            }
        }
        assert!(saw_interrupted, "Interrupted was not forwarded");
        assert!(
            saw_finished,
            "Finished was not forwarded — bridge did not keep looping after Interrupted"
        );
        assert_eq!(state.load(Ordering::Relaxed), AgentState::IDLE);
        drop(agent_tx);
    }

    #[tokio::test]
    async fn notification_delivered_to_direct_superior() {
        // The approval request is routed to the direct superior with a static
        // review section (no escalation walk). Verify the full payload lands.
        let state = make_state();
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

        super::route_to_superior(
            &state,
            &child,
            "approval-1".into(),
            "bash_exec".into(),
            serde_json::json!({"command": "rm -rf /tmp/x"}),
            "test reason",
        )
        .await;

        let notification = recv_notification(&mut prompt_rx).await;
        assert!(
            notification.contains(&format!("{child}")),
            "names the subordinate"
        );
        assert!(notification.contains("bash_exec"), "names the tool");
        assert!(
            notification.contains("rm -rf /tmp/x"),
            "includes the arguments"
        );
        assert!(
            notification.contains("test reason"),
            "includes the commit reason"
        );
        assert!(
            notification.contains("approval-1"),
            "includes the action id"
        );
        assert!(
            notification.contains("re-checked at approval time"),
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
}
