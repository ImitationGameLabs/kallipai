//! The lesche tunnel: the SSE stream reader + reconnect loop, and the inbound
//! dispatch fan-out.
//!
//! Extracted from `mod.rs`. A child module of `relay`, so `use super::*` reuses
//! the parent's private imports and grants access to [`RelayHandle`]'s private
//! fields/methods. The pump lifecycle methods it drives (`start/stop_pump`,
//! `start/stop_status_pump`, `start/stop_room_pump`) live in sibling child
//! modules and are already `pub(super)`.

use super::*;

impl RelayHandle {
    /// Hold the lesche tunnel open, reconnecting with a small backoff on any
    /// disconnect or error. Selects on `shutdown` (the tagma-wide parent token)
    /// so SIGINT/SIGTERM cancels the relay alongside axum and the agents. On
    /// shutdown the pump is drained (`stop_pump`) so in-flight emits complete.
    ///
    /// Cancel-safety: `connect_and_drain` and the per-op `tokio::spawn(dispatch)`
    /// are cancel-safe at every `.await` — a cancel mid-op loses the partial op,
    /// which the app retries via host-history re-pull on reconnect.
    pub async fn run(self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    self.stop_workers().await;
                    return;
                }
                r = self.clone().connect_and_drain() => match r {
                    Ok(()) => info!("relay tunnel stream ended; reconnecting"),
                    Err(e) => warn!("relay tunnel error: {e:#}; reconnecting"),
                }
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    self.stop_workers().await;
                    return;
                }
                _ = tokio::time::sleep(ops::TUNNEL_RECONNECT_BACKOFF) => {}
            }
        }
    }

    /// Drain both the pump and the in-flight op-dispatch tasks. Called from the
    /// shutdown branches of [`RelayHandle::run`].
    async fn stop_workers(&self) {
        self.stop_pump().await;
        self.stop_status_pump().await;
        self.stop_room_pump().await;
        self.stop_dispatch().await;
    }

    /// Abort and reap all in-flight op-dispatch tasks. Only safe on a process-
    /// tearing-down path: `deliver_message`'s spawn step (spawn-agent → install)
    /// is not abort-safe mid-flight, so aborting a dispatch there can leak
    /// spawned tasks or leave a disarmed workspace lock. Both `run` shutdown
    /// branches are process-exit paths, so this is acceptable; a future
    /// non-shutdown caller must first make `deliver_message` abort-safe.
    async fn stop_dispatch(&self) {
        let mut set = self.inner.dispatch.lock().await;
        set.abort_all();
        while set.join_next().await.is_some() {}
    }

    /// Open the tunnel SSE and dispatch each inbound message (each on its own
    /// task so a long-running op does not stall the stream reader). The status
    /// pump is bounded to this tunnel session: started once the tunnel is up,
    /// stopped when the stream ends so a reconnect installs a fresh pump.
    async fn connect_and_drain(self) -> Result<()> {
        let stream = self
            .inner
            .client
            .open_tunnel(&self.inner.device, &self.inner.tagma_id)
            .await?;
        self.start_status_pump().await;
        self.start_room_pump().await;
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(inbound) => {
                    self.inner
                        .dispatch
                        .lock()
                        .await
                        .spawn(self.clone().dispatch(inbound));
                }
                Err(e) => warn!("relay tunnel stream error: {e}"),
            }
        }
        self.stop_status_pump().await;
        self.stop_room_pump().await;
        Ok(())
    }

    async fn dispatch(self, inbound: TunnelInbound) {
        match inbound {
            TunnelInbound::KeyExchange {
                conversation_id,
                init,
            } => self.handle_kex(conversation_id, init).await,
            TunnelInbound::Envelope { envelope } => {
                // Outer last-resort: a panic that escapes the inner req_id-aware
                // boundaries in `handle_agent_op` / `handle_history`. Those cover
                // the common case (a panic yields a correlated reply/marker); a
                // panic reaching here means req_id was never parsed (or an
                // invariant broke after it), so we can only log.
                if AssertUnwindSafe(self.handle_user_op(envelope))
                    .catch_unwind()
                    .await
                    .is_err()
                {
                    error!("relay op dispatch panicked past the inner boundary");
                }
            }
            TunnelInbound::Wake => {
                // A best-effort hint that membership changed: re-poll
                // `list_my_rooms` immediately so the joined-rooms cache warms
                // before the next room envelope arrives (a just-added tagma is
                // blind until this poll lands). The Wake carries no payload, so
                // the sweep re-fetches `list_my_rooms` -- one batched GET,
                // acceptable at the expected volume.
                self.poll_rooms().await;
            }
        }
    }
}
