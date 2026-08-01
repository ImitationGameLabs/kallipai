//! Deliverer — consumes `Triggered` schedules and injects them into the target
//! agent conversation via the tagma HTTP API. Replaces kairos's separate
//! `kairos-herald` process: kallipai has no agora event hub, so the daemon
//! owns delivery directly.
//!
//! Delivery is **serial, per-row fall-through** with **503-aware persisted
//! backoff**. tagma's `post_message` returns 503 when the agent's prompt queue
//! is full (drained ~once per LLM round); on 503 we bump a per-row `attempts`
//! counter + `next_attempt_at = now + backoff` (persisted, so it survives
//! restart) and move to the next row — one stuck agent cannot block others.
//! On success we ack per-id (narrows the crash-replay window to one row). On a
//! transport error we leave the row `Triggered` for the next tick.

use std::time::Duration;

use anyhow::Result;
use kallip_client::TagmaClient;
use kallip_common::protocol::ApiError;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::state::Liveness;
use crate::store::ScheduleStore;

pub struct Deliverer {
    store: ScheduleStore,
    tagma: TagmaClient,
    poll_interval: Duration,
    shutdown: CancellationToken,
    liveness: Liveness,
}

impl Deliverer {
    pub fn new(
        store: ScheduleStore,
        tagma: TagmaClient,
        poll_interval: Duration,
        shutdown: CancellationToken,
        liveness: Liveness,
    ) -> Self {
        Self {
            store,
            tagma,
            poll_interval,
            shutdown,
            liveness,
        }
    }

    /// Run the delivery loop until `shutdown` is cancelled.
    pub async fn run(self) {
        let mut interval = tokio::time::interval(self.poll_interval);
        info!(
            poll_ms = self.poll_interval.as_millis() as u64,
            "deliverer started"
        );
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => { info!("deliverer stopped"); return; }
                _ = interval.tick() => {
                    // Stamp before the sweep: a sweep that hangs (e.g. a stuck
                    // tagma post) never returns to re-stamp, so its heartbeat
                    // ages out and `/health` reports the deliverer stale.
                    self.liveness.saw_deliver();
                    if let Err(e) = self.deliver_once().await {
                        warn!(error = %e, "delivery sweep failed");
                    }
                }
            }
        }
    }

    /// One delivery sweep: drain all backoff-eligible triggered rows, serially.
    /// Stops starting new posts when shutdown is cancelled, so a SIGTERM during
    /// a sweep bounds the drain to the one post already in flight.
    async fn deliver_once(&self) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        let rows = self.store.get_triggered(now).await?;
        for sched in rows {
            if self.shutdown.is_cancelled() {
                break;
            }
            let id = sched.id.clone();
            let agent_id = sched.agent_id.clone();
            let message = sched.message.clone();
            match self.tagma.post_message(&agent_id, &message).await {
                Ok(resp) => {
                    // Ack per-id: recurring -> Active (next_fire already
                    // advanced at trigger time), one-time -> Completed.
                    let count = self
                        .store
                        .ack_triggered_at(std::slice::from_ref(&id))
                        .await?;
                    if count == 1 {
                        info!(
                            schedule_id = %id,
                            agent_id = %agent_id,
                            queue_depth = resp.queue_depth,
                            "delivered"
                        );
                    }
                }
                Err(e) => {
                    if let Some(api) = e.downcast_ref::<ApiError>()
                        && api.status == 503
                    {
                        // Queue full: back off, do not block other rows.
                        self.store.record_delivery_failure(&id, now).await?;
                        warn!(
                            schedule_id = %id,
                            agent_id = %agent_id,
                            "delivery deferred: agent queue full (503)"
                        );
                        continue;
                    }
                    // Transport/other error: leave Triggered, retry next tick.
                    warn!(
                        schedule_id = %id,
                        agent_id = %agent_id,
                        error = %e,
                        "delivery failed; will retry"
                    );
                }
            }
        }
        Ok(())
    }
}
