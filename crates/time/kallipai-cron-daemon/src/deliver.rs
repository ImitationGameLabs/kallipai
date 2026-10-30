//! Deliverer — consumes `Triggered` schedules and injects them into the target
//! agent conversation via the tagma HTTP API. Replaces kairos's separate
//! `kairos-herald` process: kallipai has no archeion event hub, so the daemon
//! owns delivery directly.
//!
//! Delivery is **serial, per-row fall-through**: one row at a time. A failed
//! post leaves that row `Triggered` and the sweep moves on; a failed ack
//! ends the sweep, and that row and the rows behind it retry on the next
//! tick. On success we ack per-id, which narrows the crash-replay window to
//! one row.

use std::time::Duration;

use anyhow::Result;
use kallipai_client::TagmaClient;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

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
                        error!(error = %e, "delivery sweep failed");
                    }
                }
            }
        }
    }

    /// One delivery sweep: drain all triggered rows, serially. Stops starting
    /// new posts when shutdown is cancelled, so a SIGTERM during a sweep
    /// bounds the drain to the one post already in flight.
    async fn deliver_once(&self) -> Result<()> {
        let rows = self.store.get_triggered().await?;
        for sched in rows {
            if self.shutdown.is_cancelled() {
                break;
            }
            let id = sched.id.clone();
            let agent_id = sched.agent_id.clone();
            let message = sched.message.clone();
            match self.tagma.post_message(&agent_id, &message, false).await {
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
                            delivery_mode = ?resp.delivery_mode,
                            "delivered"
                        );
                    }
                }
                Err(e) => {
                    // Leave Triggered: the next tick retries it.
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
