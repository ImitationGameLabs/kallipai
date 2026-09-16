//! Room-membership poll: refresh the tagma's joined-rooms cache from the lesche
//! `list_my_rooms` snapshot so the relay inbound fork and the agent's room
//! send/read/list routes can distinguish a room conversation from the bilateral
//! 1:1 one.
//!
//! This is a child module of `relay`, so `use
//! super::*` reuses the parent's private imports and grants access to
//! [`RelayHandle`]'s private fields and methods (Rust's descendant-privacy
//! rule). The state it touches -- the room-pump slot -- lives in the relay module root; only
//! the methods move. `poll_rooms` and the pump lifecycle (`start/stop_room_pump`)
//! are `pub(super)`: the first two driven by `tunnel`, the pump exercised
//! directly by the descendant test module.

use super::*;

impl RelayHandle {
    /// Poll the lesche for this tagma's rooms and refresh the joined-rooms
    /// cache. The cache is a full replace (a removed membership self-heals on
    /// the next tick); the lesche member-gated routes (404 for non-members)
    /// backstop any stale entry. Best-effort: a poll failure logs and retries
    /// on the next tick -- a stale cache only transiently misroutes an inbound
    /// envelope, never leaks a room (the lesche enforces membership).
    pub(super) async fn poll_rooms(&self) {
        let rooms = match self.inner.client.list_my_rooms(&self.inner.tagma_id).await {
            Ok(rooms) => rooms,
            Err(e) => {
                warn!(
                    tagma = %self.inner.tagma_id, error = %e,
                    "room poll: list_my_rooms failed; retry next tick"
                );
                return;
            }
        };
        if let Some(state) = self.inner.state.upgrade() {
            state
                .joined_rooms
                .set_joined_rooms(&self.inner.name, rooms.iter().map(|v| v.room_id.clone()))
                .await;
        }
    }

    /// Poll the lesche for this tagma's direct sessions and refresh the
    /// direct-session cache (the direct counterpart of [`Self::poll_rooms`];
    /// the two calls share one pump tick so both routing caches warm
    /// together). Best-effort on the same terms: a failure logs and retries
    /// on the next tick -- the lesche member-gates every direct route, so a
    /// stale entry never leaks a session.
    pub(super) async fn poll_direct_sessions(&self) {
        let sessions = match self.inner.client.list_direct_sessions().await {
            Ok(sessions) => sessions,
            Err(e) => {
                warn!(
                    tagma = %self.inner.tagma_id, error = %e,
                    "direct-session poll: list_direct_sessions failed; retry next tick"
                );
                return;
            }
        };
        if let Some(state) = self.inner.state.upgrade() {
            state
                .direct_sessions
                .set_direct_sessions(&self.inner.name, sessions.into_iter().map(|v| v.session_id))
                .await;
        }
    }

    /// One pump tick: both routing-cache sweeps (rooms + direct sessions),
    /// cancel-selected as a unit so a tunnel-down aborts an in-flight sweep
    /// instead of waiting out its HTTP timeout.
    pub(super) async fn poll_sweep(&self) {
        self.poll_rooms().await;
        self.poll_direct_sessions().await;
    }

    /// Ensure the room-membership pump is running. Idempotent. Bounded to the
    /// tunnel session (started on tunnel-up, stopped on tunnel-down) like the
    /// status pump. The interval's first tick is immediate, so a reconnect
    /// warms the joined-rooms cache right away.
    pub(super) async fn start_room_pump(&self) {
        self.inner
            .room_pump
            .get_or_spawn(|cancel| self.clone().run_room_pump(cancel))
            .await;
    }

    /// Stop and await the room-membership pump if running, clearing the slot.
    pub(super) async fn stop_room_pump(&self) {
        self.inner.room_pump.stop_and_await().await;
    }

    /// Poll `list_my_rooms` + refresh the joined-rooms cache on a slow cadence
    /// until `cancel` fires (tunnel-down / shutdown). Each tick's sweep is
    /// cancel-select'd so a tunnel-down aborts an in-flight poll instead of
    /// waiting out its HTTP timeout -- mirroring the status pump.
    async fn run_room_pump(self, cancel: CancellationToken) {
        info!(tagma = %self.inner.tagma_id, "relay room-membership poll pump started");
        let mut ticker = tokio::time::interval(ROOM_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!(tagma = %self.inner.tagma_id, "relay room-membership poll pump stopped");
                    return;
                }
                _ = ticker.tick() => {}
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = self.poll_sweep() => {}
            }
        }
    }
}
