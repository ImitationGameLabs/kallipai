//! The event pump: subscribes to the tagma's typed topic bus in-process and
//! forwards each authored frame onto the relay as an encrypted envelope
//! (signals ride the upstream flusher). The projector (see
//! [`crate::external`]) is the sole writer: it has already projected the
//! root agent's `SseEvent` stream, persisted the authored half once, and
//! pump stamps nothing and touches no `chat_history`; it only encrypts + posts.

use super::RelayHandle;
use crate::bus::{AuthoredFrame, TopicReceiver};
use crate::relay::ops::PUMP_TRACE;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

impl RelayHandle {
    /// Ensure the pump task is running. Idempotent: a no-op if one is already
    /// live. The pump reads the current session key per-emit, so a later
    /// re-KEX's rotated key is picked up by the *next* pump incarnation.
    pub(super) async fn start_pump(&self) {
        self.inner
            .pump
            .get_or_spawn(|cancel| self.clone().run_pump(cancel))
            .await;
    }

    /// Stop and await the pump if it is running, clearing the slot so a later
    /// `start_pump` can install a fresh one. The await is what makes the
    /// re-KEX reset race-free: any in-flight emit completes (or errors)
    /// before the crypto state is touched.
    pub(super) async fn stop_pump(&self) {
        self.inner.pump.stop_and_await().await;
    }

    /// Subscribe to both chat topics on the typed bus and forward frames
    /// onto the relay until `cancel` fires. The bus lives on `AppState` for
    /// the tagma's lifetime, so a `Closed` on either topic is shutdown:
    /// the bus exists as soon as the state does.
    async fn run_pump(self, cancel: CancellationToken) {
        let Some(mut authored_rx) = self.subscribe_topics().await else {
            return; // shutting down (the AppState is dropped)
        };
        debug!("relay event pump started");
        let trace = kallipai_archeion_common::ids::TraceId::from(PUMP_TRACE.to_owned());
        loop {
            tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("relay event pump stopped");
                return;
            }
            recv = authored_rx.recv() => match recv {
                Ok(AuthoredFrame { sender, reply }) => {
                    // Authored content is already persisted + stamped by the
                    // projector, which also paired it with the sender (agent
                    // for outbound, user for the inbound echo). The pump
                    // encrypts + posts under the cancel token (so a slow emit
                    // cannot stall a re-KEX). The agent sender is re-stamped
                    // per relay: the projector stamps it with the primary
                    // archeion's tagma id (its single-value stamp is a frontend
                    // cache key, not a wire identity), which would not match
                    // this relay's participant on any secondary archeion — so
                    // Agent-kind senders are replaced with this relay's own
                    // agent sender, while Human-kind senders (the inbound
                    // echo's user) are data and pass through untouched.
                    let sender = if sender.kind ==
                        kallipai_archeion_common::ids::ParticipantKind::Agent
                    {
                        self.agent_sender()
                    } else {
                        sender
                    };
                    if let Err(e) = self.emit(&trace, sender, reply, Some(&cancel)).await {
                        warn!("relay pump emit: {e:#}");
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // Same loss-on-overflow semantics as the old shared
                    // channel; the app recovers via host-history re-pull.
                    warn!(lagged = n, "relay pump lagged authored frames");
                }
                Err(RecvError::Closed) => {
                    // The bus is process-lifetime: Closed is shutdown (already
                    // logged in `run`'s shutdown branch) — debug, not info.
                    debug!("relay event pump: authored topic closed");
                    return;
                }
            },
            }
        }
    }

    /// Subscribe to the authored topic. Returns `None` while shutting down
    /// (the `AppState` is dropped). The topic is registered at construction,
    /// so the subscribe cannot fail.
    async fn subscribe_topics(&self) -> Option<TopicReceiver<AuthoredFrame>> {
        let state = self.inner.state.upgrade()?;
        state.bus.subscribe::<AuthoredFrame>().ok()
    }
}
