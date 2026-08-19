use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use kallip_common::agentid::AgentId;
use kallip_common::approval::ApprovalStatus;
use kallip_common::protocol::{AgentState, ParkedReason, SseEvent, TransientRetryInfo};
use kallip_runtime::event::AgentEvent;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::state::{AgentRegistry, ParkedSnapshot, SharedState};

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
    parked: Arc<std::sync::Mutex<Option<ParkedSnapshot>>>,
    retrying: Arc<std::sync::Mutex<Option<TransientRetryInfo>>>,
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
                        // Terminal events share one state-dispatch path: the
                        // terminal triage below maps the event to a post-turn
                        // state (idle / waiting / parked / retrying), the mark
                        // is linearized with the all-subordinates snapshot
                        // (`mark_and_snapshot`), then the per-event decision on
                        // whether the superior gets a notification —
                        // delivered AFTER the SSE broadcast below, so
                        // subscribers never wait on the superior's inbox
                        // write.
                        let mut deferred: Option<(&'static str, String, IdleNotice)> = None;
                        match &other {
                            AgentEvent::Busy => {
                                // A new round: BUSY plus dropping the parked
                                // payload — the turn is alive, so no park
                                // reason from a previous turn can still be
                                // current. The retrying cell survives on
                                // purpose: the terminal triage reads it to tell
                                // a spent retry budget (last armed attempt ==
                                // max) from a chain with retries disabled.
                                let mut cell = parked.lock().unwrap_or_else(|e| e.into_inner());
                                state.store(AgentState::BUSY, Ordering::Relaxed);
                                *cell = None;
                            }
                            AgentEvent::Retrying { .. } | AgentEvent::StreamReset { .. } => {
                                // Layer-1 overlay: an in-request backoff or
                                // stream reset shows RETRYING while the turn
                                // is still open; this is display state, not a
                                // terminal transition. Any other in-flight
                                // event below ends the overlay.
                                state.store(AgentState::RETRYING, Ordering::Relaxed);
                            }
                            ev if ev.is_terminal() => {
                                // Fatal-error observability BEFORE the state mark:
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
                                // Layer-2 triage: map the terminal event to the
                                // post-turn state plus the parked/retrying cell
                                // payloads. The state mark happens BEFORE the
                                // superior is notified: the superior may act
                                // immediately, and a BUSY read in that window
                                // would contradict the message.
                                let (new_state, parked_reason, retry_info) = match ev {
                                    AgentEvent::Idle
                                    | AgentEvent::Cancelled
                                    | AgentEvent::Interrupted => (AgentState::IDLE, None, None),
                                    AgentEvent::Waiting { .. } => (AgentState::WAITING, None, None),
                                    // The budget gate re-arms a waiting timer
                                    // runtime-side (design D6, case b): the agent
                                    // stays WAITING for the zero-cost recovery
                                    // probe, not parked.
                                    AgentEvent::TokenBudgetExceeded { .. } => {
                                        (AgentState::WAITING, None, None)
                                    }
                                    AgentEvent::Error(msg) => (
                                        AgentState::PARKED,
                                        Some(ParkedReason::FatalError { message: msg.clone() }),
                                        None,
                                    ),
                                    AgentEvent::MaxRoundsExceeded => (
                                        AgentState::PARKED,
                                        Some(ParkedReason::MaxRoundsExceeded),
                                        None,
                                    ),
                                    AgentEvent::FailoverChainExhausted {
                                        reason,
                                        detail,
                                        transient_retry,
                                    } => match transient_retry {
                                        Some(info) => {
                                            (AgentState::RETRYING, None, Some(info.clone()))
                                        }
                                        None => {
                                            // No retry armed: park. Exhaustion
                                            // (the final FCE after the last
                                            // allowed retry) is told apart from
                                            // a chain with retries disabled by
                                            // the cell the last armed retry
                                            // wrote: attempt == max_attempts.
                                            let spent = {
                                                let cell = retrying
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                matches!(&*cell, Some(info)
                                                    if info.attempt >= info.max_attempts)
                                            };
                                            let parked_reason = if spent {
                                                ParkedReason::TransientRetryExhausted
                                            } else {
                                                ParkedReason::FailoverChainExhausted {
                                                    reason: reason.clone(),
                                                    detail: detail.clone(),
                                                }
                                            };
                                            (AgentState::PARKED, Some(parked_reason), None)
                                        }
                                    },
                                    _ => unreachable!(
                                        "is_terminal() variant missing in terminal triage"
                                    ),
                                };
                                let notice = mark_and_snapshot(
                                    &shared_state, &agent_id, &state, &activity,
                                    &parked, &retrying, new_state, parked_reason, retry_info,
                                ).await;
                                deferred = match ev {
                                    AgentEvent::Error(msg) => Some((
                                        "Subagent Error",
                                        format!("hit a fatal error and parked: {msg}"),
                                        notice,
                                    )),
                                    // A retrying FCE is not operator-actionable:
                                    // no notice, by design (the terminal-signal
                                    // asymmetry is deliberate — see design §5).
                                    AgentEvent::FailoverChainExhausted {
                                        transient_retry: Some(_),
                                        ..
                                    } => None,
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
                                    AgentEvent::Waiting { timeout_secs } => Some((
                                        "Subagent Waiting",
                                        format!("is waiting (timer {timeout_secs}s)."),
                                        notice,
                                    )),
                                    AgentEvent::MaxRoundsExceeded => Some((
                                        "Subagent Error",
                                        "hit the per-request tool-round limit and parked.".to_string(),
                                        notice,
                                    )),
                                    // Cancelled / Interrupted are operator-initiated;
                                    // TokenBudgetExceeded re-arms a wait the operator
                                    // already observes: no superior notification, but
                                    // the state mark above is still linearized with
                                    // everyone else's snapshot.
                                    _ => None,
                                };
                            }
                            _ => {
                                // The first non-retry event ends the layer-1
                                // overlay: the turn is progressing again. A
                                // streaming round likewise invalidates any
                                // parked payload (the retrying cell stays —
                                // see the Busy arm).
                                if state.load(Ordering::Relaxed) == AgentState::RETRYING {
                                    state.store(AgentState::BUSY, Ordering::Relaxed);
                                }
                                *parked.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            }
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
                    mark_idle(&state, &activity, &parked, &retrying);
                    info!("bridge task: agent channel closed, exiting");
                    break;
                }
            },

            // Forced shutdown (tagma-wide only): best-effort drain of anything
            // still queued before exiting. Per-agent cancellation reaches the
            // bridge via the channel-closed path above — see the lifecycle note.
            _ = cancel.cancelled() => {
                mark_idle(&state, &activity, &parked, &retrying);
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

/// Mark the agent gone: drop state to [`AgentState::IDLE`], clear the ephemeral
/// activity string so a stale "reading docs" doesn't persist, and drop any
/// parked/retrying payloads — the bridge only takes this path when the agent
/// task itself is gone (channel closed or tagma shutdown), and a dead agent
/// cannot be parked or retrying. Shared by the shutdown paths in
/// [`bridge_task`]; live turn-ends go through [`mark_and_snapshot`].
fn mark_idle(
    state: &std::sync::atomic::AtomicU8,
    activity: &std::sync::Mutex<String>,
    parked: &std::sync::Mutex<Option<ParkedSnapshot>>,
    retrying: &std::sync::Mutex<Option<TransientRetryInfo>>,
) {
    state.store(AgentState::IDLE, Ordering::Relaxed);
    activity.lock().unwrap_or_else(|e| e.into_inner()).clear();
    *parked.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *retrying.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

/// Apply a terminal state mark and snapshot the sibling states under one
/// registry write lock. The lock is the linearization point: two subagents
/// ending turns concurrently cannot both (or neither) observe "all
/// subordinates idle" — exactly one notification carries the last-idle
/// annotation. The parked/retrying cells are written under the same lock so
/// a status read can never observe a PARKED mark with a stale retrying
/// payload (or vice versa).
async fn mark_and_snapshot(
    shared_state: &SharedState,
    agent_id: &AgentId,
    state: &std::sync::atomic::AtomicU8,
    activity: &std::sync::Mutex<String>,
    parked: &std::sync::Mutex<Option<ParkedSnapshot>>,
    retrying: &std::sync::Mutex<Option<TransientRetryInfo>>,
    new_state: u8,
    parked_reason: Option<ParkedReason>,
    retry_info: Option<TransientRetryInfo>,
) -> IdleNotice {
    let registry = shared_state.registry.write().await;
    state.store(new_state, Ordering::Relaxed);
    activity.lock().unwrap_or_else(|e| e.into_inner()).clear();
    *parked.lock().unwrap_or_else(|e| e.into_inner()) =
        parked_reason.map(|reason| ParkedSnapshot {
            reason,
            at: Instant::now(),
        });
    *retrying.lock().unwrap_or_else(|e| e.into_inner()) = retry_info;
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
