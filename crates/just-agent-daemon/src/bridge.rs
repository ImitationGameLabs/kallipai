use std::sync::Arc;
use std::sync::atomic::Ordering;

use just_agent_core::types::{AgentEvent, AgentState, SseEvent};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub async fn bridge_task(
    mut agent_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    events_tx: broadcast::Sender<SseEvent>,
    cancel: CancellationToken,
    state: Arc<std::sync::atomic::AtomicU8>,
) {
    loop {
        tokio::select! {
            biased;

            Some(event) = agent_rx.recv() => {
                match &event {
                    AgentEvent::Busy => state.store(AgentState::BUSY, Ordering::Relaxed),
                    AgentEvent::Finished(_)
                    | AgentEvent::MaxRoundsExceeded
                    | AgentEvent::Error(_)
                    | AgentEvent::Cancelled => {
                        state.store(AgentState::IDLE, Ordering::Relaxed)
                    }
                    _ => {}
                }
                let sse = SseEvent::from(event);
                if events_tx.send(sse).is_err() {
                    info!("no SSE subscribers, event dropped");
                }
            }

            _ = cancel.cancelled() => {
                state.store(AgentState::IDLE, Ordering::Relaxed);
                while let Ok(event) = agent_rx.try_recv() {
                    events_tx.send(SseEvent::from(event)).ok();
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
