//! The event pump: subscribes to the external projector's bus in-process and
//! forwards each frame onto the relay — authored content as an encrypted
//! envelope, signals as plaintext. The projector (see [`crate::external`]) is
//! the sole writer: it has already projected the root agent's `SseEvent`
//! stream, persisted the authored half once, and stamped its `history_id`. The
//! pump stamps nothing and touches no `chat_history`; it only encrypts + posts.

use super::RelayHandle;
use crate::external::ExternalFrame;
use crate::relay::ops::PUMP_TRACE;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

impl RelayHandle {
    /// Ensure the pump task is running. Idempotent: a no-op if one is already
    /// live. The pump reads the current session key per-emit, so a later
    /// re-KEX's rotated key is picked up by the *next* pump incarnation.
    pub(super) async fn start_pump(&self) {
        let mut slot = self.inner.pump.lock().await;
        if slot.is_some() {
            return;
        }
        let cancel = CancellationToken::new();
        let task = tokio::spawn(self.clone().run_pump(cancel.clone()));
        *slot = Some(super::PumpHandle { task, cancel });
    }

    /// Stop and await the pump if it is running, clearing the slot so a later
    /// `start_pump` can install a fresh one.
    pub(super) async fn stop_pump(&self) {
        let handle = { self.inner.pump.lock().await.take() };
        if let Some(handle) = handle {
            handle.cancel.cancel();
            // Await so any in-flight emit completes (or errors) before we touch
            // the crypto state — this is what makes the re-KEX reset race-free.
            let _ = handle.task.await;
        }
    }

    /// Subscribe to the external projector's bus and forward frames onto the
    /// relay until `cancel` fires. The projector bus is process-wide and lives
    /// for the tagma's lifetime; a `Closed` is shutdown. If the projector is
    /// transiently absent (during boot ordering), retries until it appears or
    /// `cancel` fires.
    async fn run_pump(self, cancel: CancellationToken) {
        let mut rx = loop {
            if let Some(rx) = self.subscribe_projector().await {
                break rx;
            }
            warn!("relay pump: external projector not yet installed; retrying");
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        };
        debug!("relay event pump started");
        let trace = kallip_agora_common::ids::TraceId::from(PUMP_TRACE.to_owned());
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!("relay event pump stopped");
                    return;
                }
                recv = rx.recv() => match recv {
                    Ok(ExternalFrame::Authored { sender, reply }) => {
                        // Authored content is already persisted + stamped by the
                        // projector, which also paired it with the sender (agent
                        // for outbound, user for the inbound echo). The pump
                        // encrypts + posts under the cancel token (so a slow emit
                        // cannot stall a re-KEX), stamping the frame's sender
                        // onto the envelope.
                        if let Err(e) = self.emit(&trace, sender, reply, Some(&cancel)).await {
                            warn!("relay pump emit: {e:#}");
                        }
                    }
                    Ok(ExternalFrame::Signal(signal)) => {
                        // Operator metadata: plaintext signal channel, not
                        // persisted, not replayed.
                        if let Err(e) = self.emit_signal(signal, Some(&cancel)).await {
                            warn!("relay pump signal: {e:#}");
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // Same loss-on-overflow semantics as the old 256-cap
                        // SSE stream; the app recovers via host-history re-pull.
                        warn!(lagged = n, "relay pump lagged frames");
                    }
                    Err(RecvError::Closed) => {
                        // Projector bus closed. The projector is process-lifetime,
                        // so this only happens at tagma shutdown (already logged
                        // in `run`'s shutdown branch) — debug, not info. Nothing
                        // to re-subscribe to; end the pump.
                        debug!("relay event pump: projector bus closed");
                        return;
                    }
                }
            }
        }
    }

    /// Resolve the external projector off `AppState` and subscribe to its bus.
    /// Returns `None` while shutting down (state dropped) or before the
    /// projector is installed at boot.
    async fn subscribe_projector(&self) -> Option<tokio::sync::broadcast::Receiver<ExternalFrame>> {
        let state = self.inner.state.upgrade()?;
        let projector = state.external.get()?;
        Some(projector.subscribe())
    }
}
