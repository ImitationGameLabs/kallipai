//! Scheduler engine — the tick loop that advances due schedules to `Triggered`.
//!
//! Ported from kairos's `scheduler`, retaining its core correctness invariant:
//! when a schedule fires, `next_fire` is advanced to the next occurrence
//! (recurring) or cleared to `None` (one-time) and `last_fire` is stamped to
//! the old `next_fire`, in one `update_fire_times` call. Because the row is
//! flipped to `Triggered` and `next_fire` is already future/`None` before the
//! deliverer ever sees it, the schedule cannot double-fire — even if the
//! deliverer is slow or an operator manually resets a status.

use std::time::Duration;

use anyhow::Result;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use kallip_cron_common::{ScheduleStatus, TriggerSpec};

use crate::state::Liveness;
use crate::store::ScheduleStore;

pub struct Scheduler {
    store: ScheduleStore,
    tick_interval: Duration,
    liveness: Liveness,
}

impl Scheduler {
    pub fn new(store: ScheduleStore, tick_interval: Duration, liveness: Liveness) -> Self {
        Self {
            store,
            tick_interval,
            liveness,
        }
    }

    /// Run the tick loop until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(self.tick_interval);
        info!(
            tick_ms = self.tick_interval.as_millis() as u64,
            "scheduler started"
        );
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => { info!("scheduler stopped"); return; }
                _ = interval.tick() => {
                    // Stamp before the work call: a tick that hangs never
                    // returns to re-stamp, so its heartbeat ages out and
                    // `/health` reports the scheduler stale.
                    self.liveness.saw_tick();
                    if let Err(e) = self.tick_at(OffsetDateTime::now_utc()).await {
                        error!(error = %e, "scheduler tick failed");
                    }
                }
            }
        }
    }

    /// One tick at `now`: advance every due schedule to `Triggered` with its
    /// `next_fire` pre-advanced. Parameterized for deterministic tests.
    pub async fn tick_at(&self, now: time::OffsetDateTime) -> Result<()> {
        for mut schedule in self.store.list_due(now).await? {
            let Some(old_next) = schedule.next_fire else {
                // list_due guarantees next_fire IS NOT NULL; defensive only.
                continue;
            };
            let new_next = match &schedule.trigger {
                TriggerSpec::Every { duration_seconds } => {
                    Some(now + time::Duration::seconds(*duration_seconds as i64))
                }
                // One-time triggers fire once and are done.
                TriggerSpec::Once { .. } | TriggerSpec::In { .. } => None,
            };
            self.store
                .update_fire_times(
                    &schedule.id,
                    new_next,
                    Some(old_next),
                    ScheduleStatus::Triggered,
                )
                .await?;
            info!(schedule_id = %schedule.id, name = %schedule.name, "schedule triggered");
            // Keep the wire model consistent for any in-process caller that
            // reads it back this same tick.
            schedule.next_fire = new_next;
            schedule.status = ScheduleStatus::Triggered;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::calculate_initial_next_fire;
    use kallip_common::agentid::AgentId;
    use kallip_cron_common::{Priority, Schedule};
    use tempfile::TempDir;
    use time::macros::datetime;

    async fn open() -> (ScheduleStore, Scheduler, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = ScheduleStore::open(&dir.path().join("c.sqlite"))
            .await
            .unwrap();
        let liveness = Liveness::new(Duration::from_secs(5), Duration::from_secs(30));
        let scheduler = Scheduler::new(store.clone(), Duration::from_millis(1000), liveness);
        (store, scheduler, dir)
    }

    fn hourly(id: &str, next_fire: OffsetDateTime) -> Schedule {
        Schedule {
            id: id.into(),
            name: id.into(),
            trigger: TriggerSpec::Every {
                duration_seconds: 3600,
            },
            agent_id: AgentId::random(),
            message: "hi".into(),
            tags: vec![],
            priority: Priority::Normal,
            status: ScheduleStatus::Active,
            created_at: datetime!(2025-03-12 08:00 UTC),
            next_fire: Some(next_fire),
            last_fire: None,
        }
    }

    #[tokio::test]
    async fn recurring_next_fire_advances_on_trigger() {
        let (store, scheduler, _d) = open().await;
        store
            .create(&hourly("h", datetime!(2025-03-12 09:00 UTC)))
            .await
            .unwrap();

        scheduler
            .tick_at(datetime!(2025-03-12 09:00:05 UTC))
            .await
            .unwrap();

        let s = store.get("h").await.unwrap().unwrap();
        assert_eq!(s.status, ScheduleStatus::Triggered);
        // Advanced to ~10:00:05 (hourly rolls from `now`).
        assert!(s.next_fire.unwrap() > datetime!(2025-03-12 09:00:05 UTC));
    }

    #[tokio::test]
    async fn no_double_trigger_after_manual_status_reset() {
        let (store, scheduler, _d) = open().await;
        store
            .create(&hourly("h", datetime!(2025-03-12 09:00 UTC)))
            .await
            .unwrap();

        scheduler
            .tick_at(datetime!(2025-03-12 09:00:05 UTC))
            .await
            .unwrap();
        // Operator "fix": flip back to Active WITHOUT touching next_fire.
        let agent = store.get("h").await.unwrap().unwrap().agent_id;
        store
            .update_status("h", &agent, ScheduleStatus::Active)
            .await
            .unwrap();
        scheduler
            .tick_at(datetime!(2025-03-12 09:00:10 UTC))
            .await
            .unwrap();

        let s = store.get("h").await.unwrap().unwrap();
        // Stays Active: next_fire is already future, so it must not re-trigger.
        assert_eq!(s.status, ScheduleStatus::Active);
    }

    #[tokio::test]
    async fn once_schedule_clears_next_fire_on_trigger() {
        let (store, scheduler, _d) = open().await;
        let s = Schedule {
            id: "o".into(),
            name: "o".into(),
            trigger: TriggerSpec::Once {
                at: datetime!(2025-03-12 09:00 UTC),
            },
            agent_id: AgentId::random(),
            message: "hi".into(),
            tags: vec![],
            priority: Priority::Normal,
            status: ScheduleStatus::Active,
            created_at: datetime!(2025-03-12 08:00 UTC),
            next_fire: Some(datetime!(2025-03-12 09:00 UTC)),
            last_fire: None,
        };
        store.create(&s).await.unwrap();

        scheduler
            .tick_at(datetime!(2025-03-12 09:00:05 UTC))
            .await
            .unwrap();

        let s = store.get("o").await.unwrap().unwrap();
        assert_eq!(s.status, ScheduleStatus::Triggered);
        assert!(s.next_fire.is_none());
    }

    #[tokio::test]
    async fn full_recurring_lifecycle() {
        let (store, scheduler, _d) = open().await;
        let mut s = hourly("m", datetime!(2025-03-12 09:00 UTC));
        // 3-minute interval (the recurrence floor).
        s.trigger = TriggerSpec::Every {
            duration_seconds: 180,
        };
        store.create(&s).await.unwrap();

        // Fire at :05 -> next_fire advanced by 180 s (to :03:05).
        scheduler
            .tick_at(datetime!(2025-03-12 09:00:05 UTC))
            .await
            .unwrap();
        let after = store.get("m").await.unwrap().unwrap();
        assert_eq!(after.status, ScheduleStatus::Triggered);
        assert_eq!(after.next_fire, Some(datetime!(2025-03-12 09:03:05 UTC)));

        // Ack at :30 -> Active, next_fire unchanged.
        store.ack_triggered_at(&["m".into()]).await.unwrap();
        let after = store.get("m").await.unwrap().unwrap();
        assert_eq!(after.status, ScheduleStatus::Active);
        assert_eq!(after.next_fire, Some(datetime!(2025-03-12 09:03:05 UTC)));

        // :03:00 must NOT trigger (next_fire = :03:05 > :03:00).
        scheduler
            .tick_at(datetime!(2025-03-12 09:03:00 UTC))
            .await
            .unwrap();
        assert_eq!(
            store.get("m").await.unwrap().unwrap().status,
            ScheduleStatus::Active
        );

        // :03:05 second fire -> next advanced another 180 s.
        scheduler
            .tick_at(datetime!(2025-03-12 09:03:05 UTC))
            .await
            .unwrap();
        let after = store.get("m").await.unwrap().unwrap();
        assert_eq!(after.status, ScheduleStatus::Triggered);
        assert_eq!(after.next_fire, Some(datetime!(2025-03-12 09:06:05 UTC)));
    }

    #[tokio::test]
    async fn once_in_the_past_fires_immediately() {
        let (store, scheduler, _d) = open().await;
        let mut s = hourly("p", datetime!(2025-03-12 09:00 UTC));
        s.trigger = TriggerSpec::Once {
            at: datetime!(2020-01-01 00:00 UTC),
        };
        s.next_fire = Some(calculate_initial_next_fire(
            &s.trigger,
            datetime!(2025-03-12 08:00 UTC),
        ));
        // next_fire for Once{past} = the past time, so it is immediately due.
        store.create(&s).await.unwrap();
        scheduler
            .tick_at(datetime!(2025-03-12 08:00 UTC))
            .await
            .unwrap();
        assert_eq!(
            store.get("p").await.unwrap().unwrap().status,
            ScheduleStatus::Triggered
        );
    }
}
