use std::sync::Arc;
use std::sync::atomic::Ordering;

use kallip_common::agentid::AgentId;
use kallip_common::approval::ApprovalStatus;
use kallip_common::protocol::{AgentState, SseEvent};
use kallip_runtime::event::AgentEvent;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

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
                            ev if ev.is_terminal() => {
                                // Fatal-error observability BEFORE the idle mark:
                                // for a headless/subagent run the SSE broadcast
                                // below has no subscriber and is dropped
                                // silently — this log is the sole channel.
                                match ev {
                                    AgentEvent::Error(msg) => {
                                        error!(id = %agent_id, "agent round ended in error: {msg}");
                                    }
                                    AgentEvent::FailoverChainExhausted { detail, .. } => {
                                        error!(id = %agent_id, "failover chain exhausted: {detail}");
                                    }
                                    _ => {
                                        // Only Error/FCE log today; Idle is
                                        // silent by design. A future error-class
                                        // terminal variant belongs in an arm
                                        // above, not silently falling through.
                                    }
                                }
                                // The idle mark happens BEFORE the superior is
                                // notified: the superior may act immediately,
                                // and a BUSY read in that window would
                                // contradict the message.
                                let notice = mark_idle_and_snapshot(
                                    &shared_state, &agent_id, &state, &activity,
                                ).await;
                                deferred = match ev {
                                    AgentEvent::Error(msg) => Some((
                                        "Subagent Error",
                                        format!("hit a fatal error and parked: {msg}"),
                                        notice,
                                    )),
                                    AgentEvent::FailoverChainExhausted { reason, detail, .. } => Some((
                                        "Subagent Error",
                                        format!(
                                            "exhausted its model failover chain and parked ({reason}: {detail})"
                                        ),
                                        notice,
                                    )),
                                    AgentEvent::Idle => {
                                        // A subagent going idle is actionable
                                        // information for its superior; root
                                        // agents have no superior, so delivery
                                        // no-ops.
                                        Some((
                                            "Subagent Idle",
                                            "is now idle.".to_string(),
                                            notice,
                                        ))
                                    }
                                    AgentEvent::MaxRoundsExceeded => Some((
                                        "Subagent Error",
                                        "hit the per-request tool-round limit and parked.".to_string(),
                                        notice,
                                    )),
                                    // Cancelled / Interrupted are operator-initiated and
                                    // TokenBudgetExceeded is a tagma-global event the
                                    // operator already observes: no superior
                                    // notification, but the idle mark above is still
                                    // linearized with everyone else's snapshot.
                                    _ => None,
                                };
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
        AgentEvent::Waiting { timeout_secs } => Some(SseEvent::Waiting { timeout_secs }),
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
        AgentEvent::FailoverChainExhausted {
            reason,
            detail,
            transient_retry,
        } => Some(SseEvent::FailoverChainExhausted {
            reason,
            detail,
            transient_retry,
        }),
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
        Self {
            target: None,
            role: String::new(),
            is_last: false,
            live_subordinates: 0,
        }
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
        return IdleNotice {
            target: None,
            role,
            is_last: false,
            live_subordinates: 0,
        };
    };
    let Some(superior_entry) = registry.get(&superior_id) else {
        warn!(id = %agent_id, superior = %superior_id, "superior not found in registry");
        return IdleNotice {
            role,
            ..IdleNotice::none()
        };
    };
    let Some(superior_live) = superior_entry.as_live() else {
        warn!(id = %agent_id, superior = %superior_id, "superior faulted; cannot deliver");
        return IdleNotice {
            role,
            ..IdleNotice::none()
        };
    };
    let target = SuperiorTarget {
        notify: superior_live.agent.notify.clone(),
        superior_id,
    };
    let mut live_subordinates = 0usize;
    let mut all_idle = true;
    for cid in &superior_live.subagent_ids {
        let Some(child) = registry.get(cid) else {
            continue;
        };
        let Some(child_live) = child.as_live() else {
            continue;
        };
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
    let mut body = format!("[{tag}] Subordinate agent {agent_id}{role_suffix} {detail}");
    if notice.is_last {
        body.push_str(&format!(
            " It is the last to go idle — all {} live subordinates of the superior are now idle.",
            notice.live_subordinates
        ));
    }
    deliver_to_superior(shared_state, &target, agent_id, body).await;
}

#[cfg(test)]
mod tests;
