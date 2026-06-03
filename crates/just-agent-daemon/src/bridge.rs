use std::sync::Arc;
use std::sync::atomic::Ordering;

use just_agent_common::agentid::AgentId;
use just_agent_common::approval::ApprovalStatus;
use just_agent_common::command::UserInput;
use just_agent_common::protocol::{AgentState, SseEvent};
use just_agent_runtime::event::AgentEvent;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::state::SharedState;

pub async fn bridge_task(
    agent_id: AgentId,
    mut agent_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    events_tx: broadcast::Sender<SseEvent>,
    cancel: CancellationToken,
    state: Arc<std::sync::atomic::AtomicU8>,
    shared_state: SharedState,
) {
    loop {
        tokio::select! {
            biased;

            Some(event) = agent_rx.recv() => {
                match event {
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
                            AgentEvent::Finished(_)
                            | AgentEvent::MaxRoundsExceeded
                            | AgentEvent::Error(_)
                            | AgentEvent::Cancelled => {
                                state.store(AgentState::IDLE, Ordering::Relaxed)
                            }
                            _ => {}
                        }
                        if let Some(sse) = convert_event(other)
                            && events_tx.send(sse).is_err()
                        {
                            info!("no SSE subscribers, event dropped");
                        }
                    }
                }
            }

            _ = cancel.cancelled() => {
                state.store(AgentState::IDLE, Ordering::Relaxed);
                while let Ok(event) = agent_rx.try_recv() {
                    if let Some(sse) = convert_event(event) {
                        events_tx.send(sse).ok();
                    }
                }
                info!("bridge task: cancelled, exiting");
                break;
            }

            else => {
                state.store(AgentState::IDLE, Ordering::Relaxed);
                info!("bridge task: all channels closed, exiting");
                break;
            }
        }
    }
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
        AgentEvent::Cancelled => Some(SseEvent::Cancelled),
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
    // Clone the sender inside the lock so we don't hold the read lock across the async send.
    let (superior_id, prompt_tx) = {
        let registry = shared_state.registry.read().await;
        let Some(entry) = registry.get(agent_id) else {
            warn!(id = %agent_id, "agent not found in registry during superior routing");
            return;
        };
        let Some(ref superior_id) = entry.agent.config.created_by else {
            return;
        };
        let Some(superior_entry) = registry.get(superior_id) else {
            warn!(id = %superior_id, "superior not found in registry");
            return;
        };
        (superior_id.clone(), superior_entry.agent.prompt_tx.clone())
    };

    let notification = format!(
        "[Approval Request] Subordinate agent {agent_id} requests approval for:\n\
         Tool: {tool_name}\n\
         Arguments: {arguments}\n\
         Reason: {commit_reason}\n\
         Action ID: {approval_id}\n\n\
         Use `just-agent approval approve {approval_id}` to approve \
         or `just-agent approval deny {approval_id} <reason>` to deny."
    );
    if prompt_tx
        .send(UserInput::Prompt(notification))
        .await
        .is_err()
    {
        warn!(id = %superior_id, "superior prompt channel closed, approval notification dropped");
    }
}
