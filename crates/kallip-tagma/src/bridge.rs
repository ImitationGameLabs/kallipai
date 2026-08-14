use std::sync::Arc;
use std::sync::atomic::Ordering;

use kallip_common::agentid::AgentId;
use kallip_common::approval::ApprovalStatus;
use kallip_common::protocol::{AgentState, SseEvent};
use kallip_runtime::event::AgentEvent;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use time::OffsetDateTime;

use crate::state::{AgentRegistry, SharedState};

/// Route one agent's runtime events to SSE subscribers (and approval requests
/// to the agent's superior).
///
/// # Lifecycle
///
/// The bridge owns the agent's event-stream receiver and exits when that stream
/// ends — i.e. when the agent task drops its sender. The channel closes only on a
/// **lifecycle** end: `remove`, tagma shutdown, or a task panic. The agent task
/// emits its terminal `Cancelled` event on the way out, the bridge forwards it,
/// then observes `recv() == None` and exits.
///
/// **Interrupt** does *not* close the channel: it cancels only the current round
/// token, so the task aborts the round, emits `Interrupted`, and returns to its
/// outer loop — the bridge forwards `Interrupted` (setting state `IDLE`) and keeps
/// looping. The agent is still alive.
///
/// The `cancel` token is a secondary, *forced* exit for tagma-wide shutdown: it
/// preempts the bridge even if the agent task is mid-operation. It is the
/// tagma-wide parent token, **not** the agent's child, deliberately. The bridge
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
            // exit without waiting for the tagma-wide cancel, which would
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
                        // Terminal events share one idle path: mark idle
                        // linearized with the all-subordinates snapshot
                        // (`mark_idle_and_snapshot`), then decide per event
                        // whether the superior gets a notification —
                        // delivered AFTER the SSE broadcast below, so
                        // subscribers never wait on the superior's inbox
                        // write.
                        let mut deferred: Option<(&'static str, String, IdleNotice)> = None;
                        match &other {
                            AgentEvent::Busy => state.store(AgentState::BUSY, Ordering::Relaxed),
                            AgentEvent::Error(msg) => {
                                // Fatal LLM/runtime error. `error!`: the round terminally
                                // failed (no retry). This is the sole observability channel
                                // for a headless/subagent run, where the SSE event below has
                                // no subscriber and is dropped silently.
                                error!(id = %agent_id, "agent round ended in error: {msg}");
                                let notice = mark_idle_and_snapshot(
                                    &shared_state, &agent_id, &state, &activity,
                                ).await;
                                deferred = Some((
                                    "Subagent Error",
                                    format!("hit a fatal error and parked: {msg}"),
                                    notice,
                                ));
                            }
                            AgentEvent::FailoverChainExhausted { reason, detail } => {
                                error!(id = %agent_id, "failover chain exhausted: {detail}");
                                let notice = mark_idle_and_snapshot(
                                    &shared_state, &agent_id, &state, &activity,
                                ).await;
                                deferred = Some((
                                    "Subagent Error",
                                    format!(
                                        "exhausted its model failover chain and parked ({reason}: {detail})"
                                    ),
                                    notice,
                                ));
                            }
                            AgentEvent::Idle => {
                                // The idle mark happens BEFORE the superior is
                                // notified: the superior may act immediately,
                                // and a BUSY read in that window would
                                // contradict the message. A subagent going
                                // idle is actionable information for its
                                // superior; root agents have no superior, so
                                // delivery no-ops.
                                let notice = mark_idle_and_snapshot(
                                    &shared_state, &agent_id, &state, &activity,
                                ).await;
                                deferred = Some((
                                    "Subagent Idle",
                                    "is now idle.".to_string(),
                                    notice,
                                ));
                            }
                            AgentEvent::MaxRoundsExceeded => {
                                let notice = mark_idle_and_snapshot(
                                    &shared_state, &agent_id, &state, &activity,
                                ).await;
                                deferred = Some((
                                    "Subagent Error",
                                    "hit the per-request tool-round limit and parked.".to_string(),
                                    notice,
                                ));
                            }
                            // Cancelled / Interrupted are operator-initiated and
                            // TokenBudgetExceeded is a tagma-global event the
                            // operator already observes: no superior
                            // notification, but the idle mark is still
                            // linearized with everyone else's snapshot.
                            AgentEvent::Cancelled
                            | AgentEvent::Interrupted
                            | AgentEvent::TokenBudgetExceeded { .. } => {
                                mark_idle_and_snapshot(
                                    &shared_state, &agent_id, &state, &activity,
                                ).await;
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
                        if let Some((tag, detail, notice)) = deferred {
                            deliver_idle_notice(
                                &shared_state, &agent_id, tag, &detail, notice,
                            ).await;
                        }
                    }
                },
                None => {
                    mark_idle(&state, &activity);
                    info!("bridge task: agent channel closed, exiting");
                    break;
                }
            },

            // Forced shutdown (tagma-wide only): best-effort drain of anything
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
        AgentEvent::Idle => Some(SseEvent::Idle),
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

/// Route an approval request to the agent's direct superior: inbox the
/// full request and wake them if on-duty (see [`deliver_to_superior`]).
async fn route_to_superior(
    shared_state: &SharedState,
    agent_id: &AgentId,
    approval_id: String,
    tool_name: String,
    arguments: serde_json::Value,
    commit_reason: &str,
) {
    // The notification always targets the direct superior. There is no longer an
    // escalation walk to find an "allow" superior: with a tagma-global classify
    // preset and monotone-inherited exec-policy, if any upper superior could
    // `Allow` a deferred `bash_exec`, the direct superior can too -- so the direct
    // superior is always the sufficient routing target. (The approval-time gate
    // in `routes::approval` re-runs classify against the approver's rule-set, so
    // routing cannot smuggle a command past policy.)
    let Some(target) = live_superior(shared_state, agent_id).await else {
        return;
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

    // The approval is now in BOTH the inbox (as an informational text message
    // the agent reads) and the ApprovalStore (checked via has_notifications in
    // the notify arm). This is intentional: the inbox message provides context,
    // while the ApprovalStore entry is the actionable record the agent acts on.
    deliver_to_superior(shared_state, &target, agent_id, notification).await;
}


/// A resolved, live delivery target for a subagent's direct superior.
struct SuperiorTarget {
    superior_id: AgentId,
    notify: std::sync::Arc<tokio::sync::Notify>,
}

/// Resolve the direct superior of `agent_id` into a [`SuperiorTarget`]:
/// follow `created_by`, then require the superior to be registered and
/// live (a faulted superior has no runtime to receive anything). Returns
/// `None` — with a warn where the state is anomalous — for root agents
/// (no superior: expected, silent) and for unregistered/faulted parties.
/// All lookups share one read-lock acquisition; nothing is held across
/// the async delivery that follows.
async fn live_superior(shared_state: &SharedState, agent_id: &AgentId) -> Option<SuperiorTarget> {
    let registry = shared_state.registry.read().await;
    let Some(entry) = registry.get(agent_id) else {
        warn!(id = %agent_id, "agent not found in registry; no superior to resolve");
        return None;
    };
    let Some(superior_id) = entry.identity().config.created_by.clone() else {
        return None; // root agent — no superior (expected, not a warning)
    };
    let Some(superior_entry) = registry.get(&superior_id) else {
        warn!(id = %agent_id, superior = %superior_id, "superior not found in registry");
        return None;
    };
    let Some(superior_live) = superior_entry.as_live() else {
        warn!(id = %agent_id, superior = %superior_id, "superior faulted; cannot deliver");
        return None;
    };
    Some(SuperiorTarget {
        notify: superior_live.agent.notify.clone(),
        superior_id,
    })
}

/// Push a notification body to the superior's inbox (as a message from
/// `from`) and wake them when on-duty. The inbox push is unconditional —
/// the inbox is the universal message record, pulled on wake — while the
/// wake is duty-gated: an off-duty superior has the message buffered and
/// pulls it when they transition back to on-duty.
async fn deliver_to_superior(
    shared_state: &SharedState,
    target: &SuperiorTarget,
    from: &AgentId,
    body: String,
) {
    let Some(store) = shared_state.inboxes.get() else {
        warn!("inbox store not installed, dropping notification for superior");
        return;
    };
    store
        .push(
            target.superior_id.clone(),
            crate::inbox::BufferedEvent {
                timestamp: OffsetDateTime::now_utc(),
                source: format!("agent:{from}"),
                body,
            },
        )
        .await;
    if shared_state.duty.is_off_duty(&target.superior_id) {
        info!(id = %target.superior_id, "superior off-duty, notification buffered to inbox");
        return;
    }
    target.notify.notify_one();
    info!(id = %target.superior_id, "notification pushed to inbox + agent notified");
}

/// A terminal-event snapshot taken atomically with the idle mark: the
/// notification target (None for root agents and anomalous registry
/// states), the agent's role, and whether every live direct subordinate of
/// the superior is idle — i.e. this agent is the LAST to go idle.
struct IdleNotice {
    target: Option<SuperiorTarget>,
    role: String,
    is_last: bool,
    live_subordinates: usize,
}

impl IdleNotice {
    fn none() -> Self {
        Self { target: None, role: String::new(), is_last: false, live_subordinates: 0 }
    }
}

/// Mark the agent idle and snapshot the sibling states under one registry
/// write lock. The lock is the linearization point: two subagents going
/// idle concurrently cannot both (or neither) observe "all subordinates
/// idle" — exactly one notification carries the last-idle annotation.
async fn mark_idle_and_snapshot(
    shared_state: &SharedState,
    agent_id: &AgentId,
    state: &std::sync::atomic::AtomicU8,
    activity: &std::sync::Mutex<String>,
) -> IdleNotice {
    let registry = shared_state.registry.write().await;
    mark_idle(state, activity);
    snapshot_from(&registry, agent_id)
}

/// Snapshot without marking — the read-only surface used by tests.
#[cfg(test)]
async fn snapshot_idle_notice(shared_state: &SharedState, agent_id: &AgentId) -> IdleNotice {
    let registry = shared_state.registry.read().await;
    snapshot_from(&registry, agent_id)
}

/// Resolve the idle notice from a locked registry: the agent's role, its
/// live superior (as a delivery target), and whether all of the superior's
/// live direct subordinates are idle. Faulted or unregistered siblings are
/// excluded from the wait set (a faulted agent never goes idle; waiting on
/// it would starve the superior of the "all idle" signal forever).
fn snapshot_from(registry: &AgentRegistry, agent_id: &AgentId) -> IdleNotice {
    let Some(entry) = registry.get(agent_id) else {
        warn!(id = %agent_id, "agent not found in registry; no idle notice");
        return IdleNotice::none();
    };
    let role = entry.identity().config.role.clone();
    let Some(superior_id) = entry.identity().config.created_by.clone() else {
        return IdleNotice { target: None, role, is_last: false, live_subordinates: 0 };
    };
    let Some(superior_entry) = registry.get(&superior_id) else {
        warn!(id = %agent_id, superior = %superior_id, "superior not found in registry");
        return IdleNotice { role, ..IdleNotice::none() };
    };
    let Some(superior_live) = superior_entry.as_live() else {
        warn!(id = %agent_id, superior = %superior_id, "superior faulted; cannot deliver");
        return IdleNotice { role, ..IdleNotice::none() };
    };
    let target = SuperiorTarget {
        notify: superior_live.agent.notify.clone(),
        superior_id,
    };
    let mut live_subordinates = 0usize;
    let mut all_idle = true;
    for cid in &superior_live.subagent_ids {
        let Some(child) = registry.get(cid) else { continue };
        let Some(child_live) = child.as_live() else { continue };
        live_subordinates += 1;
        if child_live.agent.state.load(Ordering::Relaxed) != AgentState::IDLE {
            all_idle = false;
        }
    }
    IdleNotice {
        target: Some(target),
        role,
        is_last: all_idle && live_subordinates > 0,
        live_subordinates,
    }
}

/// Compose and deliver a terminal-event notification to the superior:
/// `[<tag>] Subordinate agent <id> (role: r) <detail>`, plus the last-idle
/// annotation when this agent was the last live subordinate to go idle.
/// No-ops for root agents (no target).
async fn deliver_idle_notice(
    shared_state: &SharedState,
    agent_id: &AgentId,
    tag: &str,
    detail: &str,
    notice: IdleNotice,
) {
    let Some(target) = notice.target else { return };
    // An unset role renders nothing rather than an empty "(role: )".
    let role_suffix = if notice.role.is_empty() {
        String::new()
    } else {
        format!(" (role: {})", notice.role)
    };
    let mut body =
        format!("[{tag}] Subordinate agent {agent_id}{role_suffix} {detail}");
    if notice.is_last {
        body.push_str(&format!(
            " It is the last to go idle — all {} live subordinates of the superior are now idle.",
            notice.live_subordinates
        ));
    }
    deliver_to_superior(shared_state, &target, agent_id, body).await;
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

        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains(&child.to_string()), "names the subordinate");
        assert!(msg.contains("bash_exec"), "names the tool");
        assert!(msg.contains("rm -rf /tmp/x"), "includes the arguments");
        assert!(msg.contains("test reason"), "includes the commit reason");
        assert!(msg.contains("approval-1"), "includes the action id");
        assert!(msg.contains("re-checked at approval time"), "carries the static review guidance");
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
        state.duty.set(parent.clone(), crate::duty::DutyStatus::OffDuty);

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
        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains("Approval Request"), "inbox should contain the approval: {msg}");
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

        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains("[Subagent Idle]") && !msg.contains("[[Subagent"), "single-bracket prefix: {msg}");
        assert!(msg.contains(&child.to_string()), "names the subagent: {msg}");
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

        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains("[Subagent Idle]") && !msg.contains("[[Subagent"), "single-bracket prefix: {msg}");
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

        state.duty.set(parent.clone(), crate::duty::DutyStatus::OffDuty);

        let notice = super::snapshot_idle_notice(&state, &child).await;
        super::deliver_idle_notice(&state, &child, "Subagent Idle", "is now idle.", notice).await;

        assert_eq!(state.inboxes.get().unwrap().len_for(&parent).await, 1);
        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains("[Subagent Idle]") && !msg.contains("[[Subagent"), "single-bracket prefix: {msg}");
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
        assert!(settled, "idle dispatch must mark idle and inbox the notification");
        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains("[Subagent Idle]") && !msg.contains("[[Subagent"), "single-bracket prefix: {msg}");
        assert!(
            tokio::time::timeout(Duration::from_millis(500), wake.as_mut()).await.is_ok(),
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
        state.duty.set(parent.clone(), crate::duty::DutyStatus::OffDuty);

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
        assert!(delivered, "off-duty dispatch must still buffer the notification");
        // ...then confirm no wake arrives after delivery: the waiter stays
        // pending (no permit is stored by the duty-gated path).
        assert!(
            tokio::time::timeout(Duration::from_millis(300), wake.as_mut()).await.is_err(),
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
        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains("[Subagent Error]") && !msg.contains("[[Subagent"), "single-bracket prefix: {msg}");
        assert!(msg.contains("hit a fatal error and parked: model endpoint returned 500"), "detail: {msg}");
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
        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains("[Subagent Error]") && !msg.contains("[[Subagent"), "single-bracket prefix: {msg}");
        assert!(msg.contains("exhausted its model failover chain and parked"), "body: {msg}");
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
        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains("hit the per-request tool-round limit and parked"), "body: {msg}");
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
            state.inboxes.get().unwrap().len_for(&parent).await, 0,
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
            reg.get(&c1).unwrap().as_live().unwrap().agent.state.store(AgentState::IDLE, Ordering::Relaxed);
            reg.get(&c2).unwrap().as_live().unwrap().agent.state.store(AgentState::BUSY, Ordering::Relaxed);
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
            &state, &c2, &c2_atomic, &std::sync::Mutex::new(String::new()),
        ).await;
        assert!(n2.is_last, "c2 is the last to go idle");
        assert_eq!(n2.live_subordinates, 2);

        super::deliver_idle_notice(&state, &c2, "Subagent Idle", "is now idle.", n2).await;
        let msg = state.inboxes.get().unwrap().pull_undelivered(&parent).await.unwrap();
        assert!(msg.contains("It is the last to go idle"), "annotation: {msg}");
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
            &state, &c1, &c1_atomic, &std::sync::Mutex::new(String::new()),
        ).await;
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
        assert!(n.target.is_none(), "no target without a registered superior");
        assert!(!n.is_last);
        super::deliver_idle_notice(&state, &child, "Subagent Idle", "is now idle.", n).await;
        assert_eq!(state.inboxes.get().unwrap().len_for(&parent).await, 0);
    }
}
