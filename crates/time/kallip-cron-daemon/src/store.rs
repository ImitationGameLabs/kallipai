//! SQLite-backed schedule store (sea-orm + sqlx-sqlite), mirroring
//! `kallip-tagma/src/relay/chat_history`.
//!
//! Schema: one `schedules` table (see `migration/m_20260801_01_init`). The
//! `trigger` and `tags` fields are JSON-in-`TEXT` (serde at the boundary), and
//! all timestamps are i64 unix seconds (UTC) — house style, avoids time-format
//! drift in SQLite. The wire `Schedule` carries `OffsetDateTime`; conversion
//! happens only here, at the store boundary.
//!
//! Three concurrent writers (scheduler, deliverer, HTTP) share the WAL pool, so
//! `open` sets a `busy_timeout` PRAGMA (chat_history omits it because it is
//! single-writer; the daemon is not) to ride out `SQLITE_BUSY`.

use std::path::Path;

use anyhow::{Context, Result};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveValue::Set, Condition, ConnectOptions, Database, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder,
};
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use time::util::is_leap_year;
use time::{Month, UtcOffset};
use tokio::fs;
use tracing::warn;

use kallip_common::agentid::AgentId;
use kallip_cron_common::{
    Period, Schedule, ScheduleError, ScheduleId, ScheduleStatus, TriggerSpec, parse_at_time,
};

pub(crate) mod entities {
    pub(crate) mod schedule {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "schedules")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
            pub name: String,
            /// Serialized `TriggerSpec` (JSON).
            #[sea_orm(column_type = "Text")]
            pub trigger: String,
            pub agent_id: String,
            #[sea_orm(column_type = "Text")]
            pub message: String,
            /// Serialized `Vec<String>` (JSON array).
            #[sea_orm(column_type = "Text")]
            pub tags: String,
            pub priority: String,
            pub status: String,
            /// Unix seconds (UTC).
            pub created_at: i64,
            pub next_fire: Option<i64>,
            pub last_fire: Option<i64>,
            /// Delivery-attempt count for 503 backoff.
            pub attempts: i64,
            /// Earliest next delivery attempt (unix seconds), for backoff.
            pub next_attempt_at: Option<i64>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}
        impl ActiveModelBehavior for ActiveModel {}
    }
}

use entities::schedule::{ActiveModel, Column, Entity};

/// A clonable handle to the schedule store. `DatabaseConnection` is internally
/// `Arc`'d, so cloning is cheap and shares the pool.
#[derive(Clone)]
pub struct ScheduleStore {
    db: DatabaseConnection,
}

impl ScheduleStore {
    /// Open (or create) the SQLite database at `path`, apply migrations, and
    /// install WAL + `synchronous=NORMAL` + `busy_timeout` on every pooled
    /// connection. The parent directory is created if missing.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create schedule db dir {}", parent.display()))?;
        }
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let mut opts = ConnectOptions::new(url);
        // Three concurrent writers (scheduler, deliverer, HTTP) + the
        // occasional read; WAL lets reads overlap writes.
        opts.max_connections(8);
        opts.map_sqlx_sqlite_opts(|o| {
            o.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
                // Ride out SQLITE_BUSY when writers contend; chat_history omits
                // this because the pump is the sole writer, but the daemon is not.
                .busy_timeout(std::time::Duration::from_secs(5))
        });
        let db = Database::connect(opts)
            .await
            .context("connect schedule db")?;
        crate::migration::Migrator::up(&db, None)
            .await
            .context("apply schedule migrations")?;
        Ok(Self { db })
    }

    /// Insert a new schedule. `schedule.id`, `created_at`, and `next_fire` must
    /// already be set by the caller (the create route mints the id and computes
    /// the initial `next_fire`).
    pub async fn create(&self, schedule: &Schedule) -> Result<()> {
        let model = ActiveModel {
            id: Set(schedule.id.clone()),
            name: Set(schedule.name.clone()),
            trigger: Set(serde_json::to_string(&schedule.trigger)?),
            agent_id: Set(schedule.agent_id.as_ref().to_string()),
            message: Set(schedule.message.clone()),
            tags: Set(serde_json::to_string(&schedule.tags)?),
            priority: Set(schedule.priority.to_string()),
            status: Set(schedule.status.to_string()),
            created_at: Set(to_unix(schedule.created_at)),
            next_fire: Set(schedule.next_fire.map(to_unix)),
            last_fire: Set(schedule.last_fire.map(to_unix)),
            attempts: Set(0),
            next_attempt_at: Set(None),
        };
        Entity::insert(model).exec(&self.db).await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Schedule>> {
        Ok(Entity::find_by_id(id).one(&self.db).await?.map(decode))
    }

    /// List `agent`'s schedules with optional status/tag filters. (Management
    /// surface is self-scoped; the scheduler/deliverer use the unscoped
    /// `list_due`/`get_triggered`.)
    pub async fn list(
        &self,
        agent: &AgentId,
        status: Option<ScheduleStatus>,
        tag: Option<&str>,
    ) -> Result<Vec<Schedule>> {
        let mut q = Entity::find().filter(Column::AgentId.eq(agent.as_ref()));
        if let Some(s) = status {
            q = q.filter(Column::Status.eq(s.to_string()));
        }
        if let Some(t) = tag {
            // JSON array contains "<tag>"; LIKE is a full scan but the daemon's
            // scale (host-side reminders) makes that irrelevant.
            q = q.filter(Column::Tags.like(format!("%\"{t}\"%")));
        }
        Ok(q.order_by_asc(Column::NextFire)
            .all(&self.db)
            .await?
            .into_iter()
            .map(decode)
            .collect())
    }

    /// Active schedules due at or before `now`. The `next_fire IS NOT NULL`
    /// filter is also the no-rearm guard: a fired one-timer has `next_fire =
    /// NULL` (cleared at trigger time), so even if an operator manually resets
    /// its status to Active it can never re-enter this set.
    pub async fn list_due(&self, now: OffsetDateTime) -> Result<Vec<Schedule>> {
        let now_ts = to_unix(now);
        Ok(Entity::find()
            .filter(Column::Status.eq(ScheduleStatus::Active.to_string()))
            .filter(Column::NextFire.is_not_null())
            .filter(Column::NextFire.lte(now_ts))
            .order_by_asc(Column::NextFire)
            .all(&self.db)
            .await?
            .into_iter()
            .map(decode)
            .collect())
    }

    /// `agent`'s earliest-fire active schedule, regardless of whether it is due
    /// (for the self-scoped `/next` endpoint).
    pub async fn get_next(&self, agent: &AgentId) -> Result<Option<Schedule>> {
        Ok(Entity::find()
            .filter(Column::AgentId.eq(agent.as_ref()))
            .filter(Column::Status.eq(ScheduleStatus::Active.to_string()))
            .filter(Column::NextFire.is_not_null())
            .order_by_asc(Column::NextFire)
            .one(&self.db)
            .await?
            .map(decode))
    }

    /// Triggered schedules whose backoff window has elapsed (or has never been
    /// set), oldest-fire-first — the deliverer's work queue. Unscoped (the
    /// deliverer fires across all agents).
    pub async fn get_triggered(&self, now: OffsetDateTime) -> Result<Vec<Schedule>> {
        let now_ts = to_unix(now);
        let rows = Entity::find()
            .filter(Column::Status.eq(ScheduleStatus::Triggered.to_string()))
            .filter(
                Condition::any()
                    .add(Column::NextAttemptAt.is_null())
                    .add(Column::NextAttemptAt.lte(now_ts)),
            )
            .order_by_asc(Column::LastFire)
            .all(&self.db)
            .await?;
        Ok(rows.into_iter().map(decode).collect())
    }

    /// Update `agent`'s schedule status (pause/resume). Scoped at the query
    /// level so a cross-owner id is a no-op (the route returns 404).
    pub async fn update_status(
        &self,
        id: &str,
        agent: &AgentId,
        status: ScheduleStatus,
    ) -> Result<()> {
        Entity::update_many()
            .col_expr(Column::Status, status.to_string().into())
            .filter(Column::Id.eq(id))
            .filter(Column::AgentId.eq(agent.as_ref()))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Advance fire times + flip status. Used by the scheduler at trigger time
    /// to pre-advance `next_fire` and stamp `last_fire` atomically.
    pub async fn update_fire_times(
        &self,
        id: &str,
        next_fire: Option<OffsetDateTime>,
        last_fire: Option<OffsetDateTime>,
        status: ScheduleStatus,
    ) -> Result<()> {
        Entity::update_many()
            .col_expr(Column::NextFire, next_fire.map(to_unix).into())
            .col_expr(Column::LastFire, last_fire.map(to_unix).into())
            .col_expr(Column::Status, status.to_string().into())
            .filter(Column::Id.eq(id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Record a failed delivery (queue-full 503): bump `attempts` and set the
    /// earliest next attempt to `now + backoff(attempts)`. The write is guarded
    /// on `status='triggered'`, so a concurrent manual ack (which flips status
    /// away from Triggered first) makes a stale read-then-write a no-op instead
    /// of stamping a backoff onto a row that is no longer awaiting delivery.
    pub async fn record_delivery_failure(&self, id: &str, now: OffsetDateTime) -> Result<()> {
        let row = Entity::find_by_id(id).one(&self.db).await?;
        let Some(row) = row else {
            return Ok(());
        };
        let attempts = row.attempts + 1;
        let next_attempt = to_unix(now) + backoff_seconds(attempts);
        Entity::update_many()
            .col_expr(Column::Attempts, attempts.into())
            .col_expr(Column::NextAttemptAt, next_attempt.into())
            .filter(Column::Id.eq(id))
            .filter(Column::Status.eq(ScheduleStatus::Triggered.to_string()))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Acknowledge delivered schedules: recurring → reactivate (next_fire was
    /// already advanced at trigger time; just flip status + reset backoff),
    /// one-time → Completed. Idempotent: a non-Triggered row is skipped.
    pub async fn ack_triggered_at(&self, ids: &[ScheduleId]) -> Result<usize> {
        let mut count = 0usize;
        for id in ids {
            let Some(row) = Entity::find_by_id(id).one(&self.db).await? else {
                continue;
            };
            if row.status != ScheduleStatus::Triggered.to_string() {
                continue;
            }
            let trigger: TriggerSpec = serde_json::from_str(&row.trigger)?;
            let new_status = if trigger.is_recurring() {
                ScheduleStatus::Active
            } else {
                ScheduleStatus::Completed
            };
            // Reset backoff so the next trigger cycle starts clean.
            Entity::update_many()
                .col_expr(Column::Status, new_status.to_string().into())
                .col_expr(Column::Attempts, 0i64.into())
                .col_expr(Column::NextAttemptAt, Option::<i64>::None.into())
                .filter(Column::Id.eq(id))
                .exec(&self.db)
                .await?;
            count += 1;
        }
        Ok(count)
    }

    /// Delete `agent`'s schedule; `false` if not found or owned by another
    /// agent (scoped at the query level).
    pub async fn delete(&self, id: &str, agent: &AgentId) -> Result<bool> {
        let r = Entity::delete_many()
            .filter(Column::Id.eq(id))
            .filter(Column::AgentId.eq(agent.as_ref()))
            .exec(&self.db)
            .await?;
        Ok(r.rows_affected > 0)
    }

    /// `(active, pending_triggered, next_fire)` scoped to `agent`, for the
    /// self-scoped `/status` endpoint.
    pub async fn stats(&self, agent: &AgentId) -> Result<(usize, usize, Option<OffsetDateTime>)> {
        let active = Entity::find()
            .filter(Column::AgentId.eq(agent.as_ref()))
            .filter(Column::Status.eq(ScheduleStatus::Active.to_string()))
            .count(&self.db)
            .await? as usize;
        let pending = Entity::find()
            .filter(Column::AgentId.eq(agent.as_ref()))
            .filter(Column::Status.eq(ScheduleStatus::Triggered.to_string()))
            .count(&self.db)
            .await? as usize;
        let next = Entity::find()
            .filter(Column::AgentId.eq(agent.as_ref()))
            .filter(Column::Status.eq(ScheduleStatus::Active.to_string()))
            .filter(Column::NextFire.is_not_null())
            .order_by_asc(Column::NextFire)
            .one(&self.db)
            .await?
            .and_then(|m| m.next_fire.map(from_unix));
        Ok((active, pending, next))
    }
}

// ---------------------------------------------------------------------------
// Model <-> wire conversion + timestamp helpers
// ---------------------------------------------------------------------------

fn decode(m: entities::schedule::Model) -> Schedule {
    // Best-effort decode: a corrupt field degrades to a placeholder (and logs)
    // rather than bricking the read path.
    let trigger = serde_json::from_str(&m.trigger).unwrap_or_else(|e| {
        warn!(
            schedule_id = %m.id,
            error = %e,
            "corrupt trigger JSON; degrading to In{{1}}"
        );
        TriggerSpec::In {
            duration_seconds: 1,
        }
    });
    let tags: Vec<String> = serde_json::from_str(&m.tags).unwrap_or_else(|e| {
        warn!(
            schedule_id = %m.id,
            error = %e,
            "corrupt tags JSON; defaulting to empty"
        );
        Vec::new()
    });
    let priority = m.priority.parse().unwrap_or_else(|_| {
        warn!(schedule_id = %m.id, raw = %m.priority, "corrupt priority; defaulting");
        Default::default()
    });
    let status = m.status.parse().unwrap_or_else(|_| {
        warn!(schedule_id = %m.id, raw = %m.status, "corrupt status; defaulting");
        Default::default()
    });
    Schedule {
        id: m.id,
        name: m.name,
        trigger,
        agent_id: m.agent_id.into(),
        message: m.message,
        tags,
        priority,
        status,
        created_at: from_unix(m.created_at),
        next_fire: m.next_fire.map(from_unix),
        last_fire: m.last_fire.map(from_unix),
    }
}

/// `OffsetDateTime` -> unix seconds, normalized to UTC. Stored i64 drops
/// sub-second precision (the daemon is second-precision by contract).
fn to_unix(t: OffsetDateTime) -> i64 {
    t.to_offset(UtcOffset::UTC).unix_timestamp()
}

fn from_unix(secs: i64) -> OffsetDateTime {
    // A corrupt row should not brick reads; clamp to the epoch (and log).
    OffsetDateTime::from_unix_timestamp(secs).unwrap_or_else(|_| {
        warn!(secs, "corrupt unix timestamp; clamping to epoch");
        OffsetDateTime::UNIX_EPOCH
    })
}

/// Exponential backoff (seconds) for a 503 (queue-full) retry: 1, 2, 4, ...
/// capped at 60s. `attempt` is 1-based (the count after increment).
fn backoff_seconds(attempt: i64) -> i64 {
    let shift = (attempt - 1).clamp(0, 6) as u32;
    (1i64 << shift).min(60)
}

// ---------------------------------------------------------------------------
// Time math (ported from kairos, adapted)
// ---------------------------------------------------------------------------

/// Calculate the next fire time for a recurring schedule, relative to `from`.
/// `at_time` ("HH:MM" UTC) is consumed only by Daily/Monthly/Yearly;
/// Minutely/Hourly/Weekly are rolling cadences (the create-time validator
/// rejects `at_time` for those, so it is always `None` here).
pub fn calculate_next_fire(
    period: &Period,
    at_time: &Option<String>,
    from: OffsetDateTime,
) -> Result<OffsetDateTime, ScheduleError> {
    let from = from.to_offset(UtcOffset::UTC);
    let (hour, minute, second) = if let Some(time_str) = at_time {
        let (h, m) = parse_at_time(time_str)?;
        (h, m, 0u8)
    } else {
        (from.hour(), from.minute(), from.second())
    };

    let next = match period {
        // Rolling cadences — `at_time` is meaningless and never present.
        Period::Minutely => from + time::Duration::minutes(1),
        Period::Hourly => from + time::Duration::hours(1),
        Period::Weekly => from + time::Duration::weeks(1),
        // `at_time`-anchored — clamp to the next occurrence at HH:MM:SS.
        Period::Daily => {
            let candidate = at(from, hour, minute, second)?;
            if candidate > from {
                candidate
            } else {
                candidate + time::Duration::days(1)
            }
        }
        Period::Monthly => {
            let candidate = at(from, hour, minute, second)?;
            if candidate > from {
                candidate
            } else {
                add_one_month(candidate)
            }
        }
        Period::Yearly => {
            let candidate = at(from, hour, minute, second)?;
            if candidate > from {
                candidate
            } else {
                add_one_year(candidate)
            }
        }
    };
    Ok(next)
}

fn at(
    from: OffsetDateTime,
    hour: u8,
    minute: u8,
    second: u8,
) -> Result<OffsetDateTime, ScheduleError> {
    from.replace_hour(hour)
        .and_then(|d| d.replace_minute(minute))
        .and_then(|d| d.replace_second(second))
        .map_err(|e| ScheduleError::InvalidAtTime(e.to_string()))
}

/// Initial `next_fire` for a brand-new schedule. Always returns `Some` for our
/// trigger set, so an `Active` row always has a concrete `next_fire` (the
/// no-rearm invariant relies on this).
pub fn calculate_initial_next_fire(
    trigger: &TriggerSpec,
    now: OffsetDateTime,
) -> Result<OffsetDateTime, ScheduleError> {
    let now = now.to_offset(UtcOffset::UTC);
    match trigger {
        TriggerSpec::Once { at } => Ok(at.to_offset(UtcOffset::UTC)),
        TriggerSpec::In { duration_seconds } => {
            Ok(now + time::Duration::seconds(*duration_seconds as i64))
        }
        TriggerSpec::Every { period, at_time } => calculate_next_fire(period, at_time, now),
    }
}

/// Add one month, clamping the day if the next month is shorter (Jan 31 -> Feb 28/29).
fn add_one_month(dt: OffsetDateTime) -> OffsetDateTime {
    let next_month = dt.month().next();
    let next_year = if dt.month() == Month::December {
        dt.year() + 1
    } else {
        dt.year()
    };
    let max_day = next_month.length(next_year);
    let day = dt.day().min(max_day);
    dt.replace_year(next_year)
        .and_then(|d| d.replace_day(day))
        .and_then(|d| d.replace_month(next_month))
        .expect("valid date components")
}

/// Add one year, handling Feb 29 on non-leap years.
fn add_one_year(dt: OffsetDateTime) -> OffsetDateTime {
    let next_year = dt.year() + 1;
    if dt.month() == Month::February && dt.day() == 29 && !is_leap_year(next_year) {
        dt.replace_day(28)
            .and_then(|d| d.replace_year(next_year))
            .expect("valid date components")
    } else {
        dt.replace_year(next_year).expect("valid year")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_common::agentid::AgentId;
    use kallip_cron_common::{Priority, Schedule};
    use tempfile::TempDir;
    use time::macros::datetime;

    async fn open_tmp() -> (ScheduleStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = ScheduleStore::open(&dir.path().join("cron.sqlite"))
            .await
            .unwrap();
        (store, dir)
    }

    fn once(id: &str, at: OffsetDateTime) -> Schedule {
        Schedule {
            id: id.into(),
            name: id.into(),
            trigger: TriggerSpec::Once { at },
            agent_id: AgentId::random(),
            message: "hi".into(),
            tags: vec![],
            priority: Priority::Normal,
            status: ScheduleStatus::Active,
            created_at: datetime!(2025-03-12 08:00 UTC),
            next_fire: Some(at),
            last_fire: None,
        }
    }

    #[tokio::test]
    async fn management_methods_are_agent_scoped() {
        let (store, _d) = open_tmp().await;
        let a: AgentId = "agent-a".parse().unwrap();
        let b: AgentId = "agent-b".parse().unwrap();
        // Two schedules, same id space, different owners.
        let mut sa = once("a1", datetime!(2025-03-12 09:00 UTC));
        sa.agent_id = a.clone();
        let mut sb = once("b1", datetime!(2025-03-12 09:00 UTC));
        sb.agent_id = b.clone();
        store.create(&sa).await.unwrap();
        store.create(&sb).await.unwrap();

        // list is per-agent.
        assert_eq!(store.list(&a, None, None).await.unwrap().len(), 1);
        assert_eq!(store.list(&b, None, None).await.unwrap().len(), 1);

        // get is unscoped (a pure read) — both resolve — but the route enforces
        // ownership; here we assert get itself returns the row regardless.
        assert!(store.get("a1").await.unwrap().is_some());
        assert!(store.get("b1").await.unwrap().is_some());

        // delete + update_status are scoped: cross-owner is a no-op.
        assert!(!store.delete("a1", &b).await.unwrap()); // b can't delete a's row
        assert!(store.get("a1").await.unwrap().is_some()); // still there
        store
            .update_status("a1", &b, ScheduleStatus::Paused)
            .await
            .unwrap();
        assert_eq!(
            store.get("a1").await.unwrap().unwrap().status,
            ScheduleStatus::Active
        ); // unchanged: b can't pause a's row
        assert!(store.delete("a1", &a).await.unwrap()); // a deletes own row

        // stats + get_next are per-agent.
        let (active_a, _, _) = store.stats(&a).await.unwrap();
        assert_eq!(active_a, 0);
        let (active_b, _, next_b) = store.stats(&b).await.unwrap();
        assert_eq!(active_b, 1);
        assert!(next_b.is_some());
        assert!(store.get_next(&a).await.unwrap().is_none());
        assert!(store.get_next(&b).await.unwrap().is_some());
    }

    // --- time math (ported from kairos) ---

    #[test]
    fn daily_next_day_at_time() {
        let now = OffsetDateTime::now_utc();
        let next = calculate_next_fire(&Period::Daily, &Some("09:00".into()), now).unwrap();
        assert!(next > now);
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn hourly_rolls_forward_ignoring_at_time() {
        let now = datetime!(2025-03-12 09:30 UTC);
        let next = calculate_next_fire(&Period::Hourly, &None, now).unwrap();
        assert_eq!(next, datetime!(2025-03-12 10:30 UTC));
    }

    #[test]
    fn weekly_rolls_forward_one_week_same_weekday() {
        // 2025-03-12 is a Wednesday; +7 days lands on the next Wednesday, same
        // time-of-day — the rolling-cadence semantics the validator guarantees
        // (Weekly with at_time is rejected at create).
        let now = datetime!(2025-03-12 09:30 UTC);
        let next = calculate_next_fire(&Period::Weekly, &None, now).unwrap();
        assert_eq!(next, datetime!(2025-03-19 09:30 UTC));
        assert_eq!(next.weekday(), time::Weekday::Wednesday);
    }

    #[test]
    fn monthly_31_to_28() {
        let next =
            calculate_next_fire(&Period::Monthly, &None, datetime!(2025-01-31 10:00 UTC)).unwrap();
        assert_eq!(next.month(), Month::February);
        assert_eq!(next.day(), 28);
    }

    #[test]
    fn monthly_31_to_29_leap() {
        let next =
            calculate_next_fire(&Period::Monthly, &None, datetime!(2024-01-31 10:00 UTC)).unwrap();
        assert_eq!(next.month(), Month::February);
        assert_eq!(next.day(), 29);
    }

    #[test]
    fn yearly_leap_to_non_leap_downgrades_and_stays() {
        let now = datetime!(2024-02-29 10:00 UTC);
        let n1 = calculate_next_fire(&Period::Yearly, &None, now).unwrap();
        assert_eq!(n1.year(), 2025);
        assert_eq!(n1.day(), 28);
        let n2 = calculate_next_fire(&Period::Yearly, &None, n1).unwrap();
        assert_eq!(n2.day(), 28); // does not upgrade back to 29
    }

    #[test]
    fn yearly_dec_31_wraps_year() {
        let next = calculate_next_fire(&Period::Yearly, &None, datetime!(2025-12-31 23:59:59 UTC))
            .unwrap();
        assert_eq!(next.year(), 2026);
    }

    // --- calculate_initial_next_fire (ported from kairos) ---

    #[test]
    fn initial_next_fire_once_is_at() {
        let at = datetime!(2025-03-12 09:00 UTC);
        let next =
            calculate_initial_next_fire(&TriggerSpec::Once { at }, datetime!(2025-03-12 08:00 UTC))
                .unwrap();
        assert_eq!(next, at);
    }

    #[test]
    fn initial_next_fire_in_is_now_plus_duration() {
        let now = datetime!(2025-03-12 08:00 UTC);
        let next = calculate_initial_next_fire(
            &TriggerSpec::In {
                duration_seconds: 3600,
            },
            now,
        )
        .unwrap();
        assert_eq!(next, datetime!(2025-03-12 09:00 UTC));
    }

    // --- monthly gaps (ported from kairos) ---

    #[test]
    fn monthly_normal() {
        let next =
            calculate_next_fire(&Period::Monthly, &None, datetime!(2025-03-15 10:00 UTC)).unwrap();
        assert_eq!(next.month(), Month::April);
        assert_eq!(next.day(), 15);
    }

    #[test]
    fn monthly_december_wrap() {
        let next =
            calculate_next_fire(&Period::Monthly, &None, datetime!(2025-12-15 10:00 UTC)).unwrap();
        assert_eq!(next.month(), Month::January);
        assert_eq!(next.day(), 15);
        assert_eq!(next.year(), 2026);
    }

    #[test]
    fn monthly_31_to_30_day_month() {
        // Mar 31 -> Apr 30 (April has only 30 days).
        let next =
            calculate_next_fire(&Period::Monthly, &None, datetime!(2025-03-31 10:00 UTC)).unwrap();
        assert_eq!(next.month(), Month::April);
        assert_eq!(next.day(), 30);
    }

    #[test]
    fn monthly_feb_28_to_march() {
        let next =
            calculate_next_fire(&Period::Monthly, &None, datetime!(2025-02-28 10:00 UTC)).unwrap();
        assert_eq!(next.month(), Month::March);
        assert_eq!(next.day(), 28);
    }

    #[test]
    fn monthly_preserves_time() {
        let next = calculate_next_fire(&Period::Monthly, &None, datetime!(2025-03-15 14:30:45 UTC))
            .unwrap();
        assert_eq!(next.hour(), 14);
        assert_eq!(next.minute(), 30);
        assert_eq!(next.second(), 45);
    }

    // --- yearly gaps (ported from kairos) ---

    #[test]
    fn yearly_normal() {
        let next =
            calculate_next_fire(&Period::Yearly, &None, datetime!(2025-03-15 10:00 UTC)).unwrap();
        assert_eq!(next.year(), 2026);
        assert_eq!(next.month(), Month::March);
        assert_eq!(next.day(), 15);
    }

    #[test]
    fn yearly_explicit_feb_28_never_upgrades() {
        // Symmetric to the leap-downgrade test: an explicit Feb 28 schedule
        // stays Feb 28 across leap years (we never auto-upgrade to Feb 29).
        let now = datetime!(2025-02-28 10:00 UTC);
        let n1 = calculate_next_fire(&Period::Yearly, &None, now).unwrap();
        assert_eq!(n1.year(), 2026);
        assert_eq!(n1.day(), 28);
        let n2 = calculate_next_fire(&Period::Yearly, &None, n1).unwrap();
        assert_eq!(n2.year(), 2027);
        assert_eq!(n2.day(), 28);
        let n3 = calculate_next_fire(&Period::Yearly, &None, n2).unwrap();
        assert_eq!(n3.year(), 2028); // leap year
        assert_eq!(n3.day(), 28); // still Feb 28, not upgraded to 29
    }

    #[test]
    fn yearly_preserves_time() {
        let next = calculate_next_fire(&Period::Yearly, &None, datetime!(2025-06-15 08:45:30 UTC))
            .unwrap();
        assert_eq!(next.hour(), 8);
        assert_eq!(next.minute(), 45);
        assert_eq!(next.second(), 30);
    }

    #[test]
    fn backoff_caps_at_60s() {
        assert_eq!(backoff_seconds(1), 1);
        assert_eq!(backoff_seconds(2), 2);
        assert_eq!(backoff_seconds(3), 4);
        assert_eq!(backoff_seconds(7), 60);
        assert_eq!(backoff_seconds(99), 60);
    }

    // --- store round-trips ---

    #[tokio::test]
    async fn create_then_get_round_trips() {
        let (store, _d) = open_tmp().await;
        let s = once("a", datetime!(2025-03-12 09:00 UTC));
        store.create(&s).await.unwrap();
        let got = store.get("a").await.unwrap().unwrap();
        assert_eq!(got.name, "a");
        assert!(matches!(got.trigger, TriggerSpec::Once { .. }));
        assert_eq!(got.next_fire, Some(datetime!(2025-03-12 09:00 UTC)));
    }

    #[tokio::test]
    async fn list_due_filters_by_next_fire_and_status() {
        let (store, _d) = open_tmp().await;
        store
            .create(&once("due", datetime!(2025-03-12 08:00 UTC)))
            .await
            .unwrap();
        store
            .create(&once("future", datetime!(2025-03-12 09:30 UTC)))
            .await
            .unwrap();
        let due = store
            .list_due(datetime!(2025-03-12 08:55 UTC))
            .await
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "due");
    }

    #[tokio::test]
    async fn get_next_returns_earliest_active_regardless_of_due() {
        // `/next` is "earliest absolute next_fire among active", not "next due":
        // an overdue row sorts before a future one.
        let (store, _d) = open_tmp().await;
        let agent: AgentId = "agent-x".parse().unwrap();
        let mut past = once("past", datetime!(2025-03-12 08:55 UTC));
        past.agent_id = agent.clone();
        let mut future = once("future", datetime!(2025-03-12 09:30 UTC));
        future.agent_id = agent.clone();
        store.create(&past).await.unwrap();
        store.create(&future).await.unwrap();
        let next = store.get_next(&agent).await.unwrap().unwrap();
        assert_eq!(next.id, "past");
    }

    #[tokio::test]
    async fn fired_one_timer_with_null_next_fire_not_rearmed() {
        let (store, _d) = open_tmp().await;
        // Simulate a fired one-timer (next_fire cleared) that an operator
        // manually reset to Active.
        let mut s = once("done", datetime!(2025-03-12 08:00 UTC));
        s.status = ScheduleStatus::Active;
        s.next_fire = None;
        s.last_fire = Some(datetime!(2025-03-12 08:00 UTC));
        store.create(&s).await.unwrap();
        // list_due must never return it (the no-rearm invariant).
        let due = store
            .list_due(datetime!(2025-03-12 08:00 UTC))
            .await
            .unwrap();
        assert!(due.is_empty());
    }

    #[tokio::test]
    async fn ack_reactivates_recurring_completes_onetime() {
        let (store, _d) = open_tmp().await;
        // Recurring, triggered.
        let mut rec = once("rec", datetime!(2025-03-12 09:00 UTC));
        rec.trigger = TriggerSpec::Every {
            period: Period::Hourly,
            at_time: None,
        };
        rec.status = ScheduleStatus::Triggered;
        rec.next_fire = Some(datetime!(2025-03-12 10:00 UTC)); // already advanced
        rec.last_fire = Some(datetime!(2025-03-12 09:00 UTC));
        store.create(&rec).await.unwrap();
        // One-time, triggered.
        let mut one = once("one", datetime!(2025-03-12 09:00 UTC));
        one.status = ScheduleStatus::Triggered;
        one.next_fire = None;
        one.last_fire = Some(datetime!(2025-03-12 09:00 UTC));
        store.create(&one).await.unwrap();

        let n = store
            .ack_triggered_at(&["rec".into(), "one".into()])
            .await
            .unwrap();
        assert_eq!(n, 2);
        let rec = store.get("rec").await.unwrap().unwrap();
        assert_eq!(rec.status, ScheduleStatus::Active);
        assert_eq!(rec.next_fire, Some(datetime!(2025-03-12 10:00 UTC)));
        let one = store.get("one").await.unwrap().unwrap();
        assert_eq!(one.status, ScheduleStatus::Completed);
    }

    #[tokio::test]
    async fn record_delivery_failure_sets_backoff() {
        let (store, _d) = open_tmp().await;
        let mut s = once("rec", datetime!(2025-03-12 09:00 UTC));
        s.trigger = TriggerSpec::Every {
            period: Period::Hourly,
            at_time: None,
        };
        s.status = ScheduleStatus::Triggered;
        store.create(&s).await.unwrap();
        store
            .record_delivery_failure("rec", datetime!(2025-03-12 09:00 UTC))
            .await
            .unwrap();
        let now = datetime!(2025-03-12 09:00 UTC);
        // next_attempt_at = now + 1s; get_triggered before that excludes it.
        let pre = store.get_triggered(now).await.unwrap();
        assert!(pre.is_empty());
        let post = store
            .get_triggered(datetime!(2025-03-12 09:00:02 UTC))
            .await
            .unwrap();
        assert_eq!(post.len(), 1);
    }
}
