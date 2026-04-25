use just_agent_core::types::{AgentEvent, SseEvent};
use tokio::sync::broadcast;
use tracing::info;

pub async fn bridge_task(
    mut agent_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    events_tx: broadcast::Sender<SseEvent>,
) {
    loop {
        tokio::select! {
            Some(event) = agent_rx.recv() => {
                let sse = SseEvent::from(event);
                if events_tx.send(sse).is_err() {
                    info!("no SSE subscribers, event dropped");
                }
            }
            else => {
                info!("bridge task: all channels closed, exiting");
                break;
            }
        }
    }
}
