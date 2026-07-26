//! The event pump: subscribes to the root agent's `SseEvent` broadcast channel
//! in-process, maps each event onto the agent-free `TagmaEvent` vocabulary, and
//! posts the encrypted envelope. Replaces the former standalone connector's HTTP-SSE
//! subscription — no network hop, no tagma-SSE no-timeout reqwest client.

use kallip_common::protocol::SseEvent;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{RelayHandle, ops::PUMP_TRACE};
use crate::state::RegistryEntry;

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

    /// Subscribe to the root agent's `events_tx` and pump events until `cancel`
    /// fires. The root is the non-removable singleton and its `events_tx` Arc
    /// survives reactivation, so a single subscription is stable for the tagma's
    /// lifetime. If the root is transiently absent (e.g. faulted by a restore
    /// path before reactivation), the initial subscribe retries until the root
    /// is live again or `cancel` fires — mirroring the in-loop `Closed` arm.
    async fn run_pump(self, cancel: CancellationToken) {
        let mut rx = loop {
            if let Some(rx) = self.subscribe_root().await {
                break rx;
            }
            warn!("relay pump: root agent not live; retrying");
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            }
        };
        info!("relay event pump started");
        let trace = kallip_agora_common::ids::TraceId::from(PUMP_TRACE.to_owned());
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("relay event pump stopped");
                    return;
                }
                recv = rx.recv() => match recv {
                    Ok(sse) => {
                        if let Some(tagma_ev) = super::ops::map_sse_event(&sse) {
                            // Keep the recv→emit path tight: no extra `.await`
                            // between recv and emit, to bound the broadcast
                            // `Lagged` window (a slow post_envelope already
                            // represents the full backpressure, same as the old
                            // HTTP-SSE pump).
                            // emit_event appends to chat_history first (so a
                            // 503 while the app is offline no longer loses the
                            // frame), then live-emits under the cancel token.
                            if let Err(e) = self.emit_event(
                                &trace,
                                kallip_lesche_common::message::TagmaReply::Event {
                                    event: tagma_ev,
                                    history_id: 0,
                                },
                                Some(&cancel),
                            )
                            .await
                            {
                                warn!("relay pump emit: {e:#}");
                            }
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // Same loss-on-overflow semantics as the old 256-cap
                        // SSE stream; the app recovers via host-history re-pull.
                        warn!(lagged = n, "relay pump lagged events");
                    }
                    Err(RecvError::Closed) => {
                        // Root sender dropped (root removed). Re-resolve + retry
                        // with a backoff; defense-in-depth for a future fault
                        // path that replaces the channel.
                        warn!("relay pump: root event stream closed; attempting re-subscribe");
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                        }
                        rx = match self.subscribe_root().await {
                            Some(r) => r,
                            None => return,
                        };
                    }
                }
            }
        }
    }

    /// Resolve the root agent's `events_tx` under the registry read-lock and
    /// subscribe. Returns `None` if the tagma is shutting down (state dropped)
    /// or the root is not live.
    async fn subscribe_root(&self) -> Option<tokio::sync::broadcast::Receiver<SseEvent>> {
        let state = self.inner.state.upgrade()?;
        let registry = state.registry.read().await;
        // The root is normally live (created by ensure_root_agent before the
        // relay starts and non-removable while live); a transient faulted-root
        // window is handled by retrying from run_pump until it goes live.
        let (_id, entry) = registry.root_agent()?;
        if let RegistryEntry::Live(live) = entry {
            Some(live.agent.events_tx.subscribe())
        } else {
            None
        }
    }
}
