//! The direct (local, non-relay) serving path: a pure forwarder from the
//! external projector's bus to a plain SSE consumed by a local frontend client.
//!
//! The projector (see [`crate::external`]) is the sole writer of chat content:
//! it subscribes to the root agent's broadcast, persists each authored/inbound
//! row once, and publishes stamped frames onto a bus. `DirectServing` subscribes
//! to that bus and re-publishes the authored + signal frames as
//! [`DirectFrame`]s on its own broadcast, which the external SSE handler
//! drains. It owns no `chat_history` store and stamps nothing. A periodic
//! status snapshot is pushed on the same channel from a separate pump (status
//! is ephemeral and orthogonal to the projector).

use std::sync::{Arc, Weak};
use std::time::Duration;

use kallip_common::protocol::SignalEvent;
use kallip_lesche_common::event::TagmaStatusPayload;
use kallip_lesche_common::message::{Participant, TagmaReply};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::external::ExternalFrame;
use crate::external::ExternalProjector;
use crate::relay::status_pump::snapshot_status;
use crate::state::AppState;

/// The SSE cadence for status snapshots, aligned with the relay status pump.
const STATUS_INTERVAL: Duration = Duration::from_secs(2);

/// One frame on the direct SSE stream. The variant is the SSE event-name
/// discriminator; the inner value is the `data:` payload. Serialization
/// (variant name + inner JSON) lives in `crate::sse::serialize_direct_frame`.
#[derive(Clone, Debug)]
pub(crate) enum DirectFrame {
    /// An authored message forwarded from the projector: an `assistant_content`
    /// event, a `user_message` echo, etc. Carries the sender alongside the
    /// content reply (mirrors the online envelope's `{sender, body}`); already
    /// persisted by the projector.
    Authored {
        sender: Participant,
        reply: TagmaReply,
    },
    /// A runtime signal (busy/idle presence, turn terminals, errors). Ephemeral.
    Signal(SignalEvent),
    /// An aggregate runtime snapshot. Ephemeral.
    Status(TagmaStatusPayload),
}

/// The JSON payload serialized for an `authored` direct-SSE event: the sender
/// paired with the content reply, so the offline frontend (which has no relay
/// envelope) renders the author from one uniform shape.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct DirectAuthoredPayload {
    pub sender: Participant,
    pub reply: TagmaReply,
}

/// The direct serving handle. Always present on the tagma (it serves any local
/// frontend client), independent of whether the relay is also active.
#[derive(Clone)]
pub(crate) struct DirectServing {
    inner: Arc<Inner>,
}

struct Inner {
    frames_tx: broadcast::Sender<DirectFrame>,
    state: Weak<AppState>,
}

impl DirectServing {
    /// Construct the handle and spawn the projector-forwarding + status pumps.
    /// The pumps are cancelled by a child of `state.shutdown`, so SIGINT/SIGTERM
    /// drains them alongside the rest of the tagma.
    pub(crate) fn new(state: Weak<AppState>, projector: ExternalProjector) -> Self {
        let (frames_tx, _) = broadcast::channel(256);
        let cancel = state
            .upgrade()
            .map(|s| s.shutdown.child_token())
            .unwrap_or_default();
        let inner = Arc::new(Inner { frames_tx, state });
        let serving = Self {
            inner: inner.clone(),
        };
        tokio::spawn(serving.clone().run_forward_pump(projector, cancel.clone()));
        tokio::spawn(serving.clone().run_status_pump(cancel));
        serving
    }

    /// Subscribe to the direct SSE stream.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<DirectFrame> {
        self.inner.frames_tx.subscribe()
    }

    /// The forwarding pump: subscribe to the external projector's bus and
    /// re-publish authored + signal frames as [`DirectFrame`]s for the local
    /// SSE. The projector already persisted/stamped the authored frames; this
    /// pump stamps nothing. Ends when the projector's bus closes (tagma
    /// shutdown) or `cancel` fires.
    async fn run_forward_pump(self, projector: ExternalProjector, cancel: CancellationToken) {
        use broadcast::error::RecvError;
        let mut rx = projector.subscribe();
        info!("direct forward pump started");
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("direct forward pump stopped");
                    return;
                }
                recv = rx.recv() => match recv {
                    Ok(frame) => match frame {
                        ExternalFrame::Authored { sender, reply } => {
                            let _ = self
                                .inner
                                .frames_tx
                                .send(DirectFrame::Authored { sender, reply });
                        }
                        ExternalFrame::Signal(event) => {
                            let _ = self.inner.frames_tx.send(DirectFrame::Signal(event));
                        }
                    },
                    Err(RecvError::Lagged(n)) => {
                        warn!(lagged = n, "direct forward pump lagged frames");
                    }
                    Err(RecvError::Closed) => {
                        // Projector bus closed (tagma shutting down). The frame
                        // stream ends; the SSE handler's `take_until(shutdown)`
                        // ends the response.
                        return;
                    }
                }
            }
        }
    }

    /// The status pump: snapshot aggregate runtime state on a fixed cadence and
    /// push it to subscribers. Plaintext operator metadata; orthogonal to the
    /// projector (status is ephemeral, never persisted).
    async fn run_status_pump(self, cancel: CancellationToken) {
        use tokio::time::MissedTickBehavior;
        info!("direct status pump started");
        let mut ticker = tokio::time::interval(STATUS_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }
            let Some(state) = self.inner.state.upgrade() else {
                return;
            };
            let payload = snapshot_status(&*state.registry.read().await, &state.token_budget);
            let _ = self.inner.frames_tx.send(DirectFrame::Status(payload));
        }
    }
}
