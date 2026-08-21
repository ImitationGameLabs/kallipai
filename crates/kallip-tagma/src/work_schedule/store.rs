//! SQLite-backed work-schedule store (sea-orm + sqlx-sqlite).

use std::path::Path;

use anyhow::{Context, Result};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveValue::Set, ConnectOptions, Database, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder,
};
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use time::UtcOffset;
use tokio::fs;
use tokio::sync::Notify;


use super::spec::{Spec, DAY_MINUTES};
use super::WorkSchedule;
#[cfg(test)]
use super::WorkScheduleStatus;

pub(crate) mod entities {
    pub(crate) mod work_schedule {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "work_schedules")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
            pub spec: Option<String>,
            pub pre_warn_minutes: i64,
            pub final_warn_minutes: i64,
            #[sea_orm(column_type = "Text")]
            pub wake_prompt: String,
            pub status: String,
            pub created_at: i64,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}
        impl ActiveModelBehavior for ActiveModel {}
    }
}

use entities::work_schedule::{ActiveModel, Column, Entity};

#[derive(Clone)]
pub struct WorkScheduleStore {
    db: DatabaseConnection,
    /// Wake signal for the scheduling engine: fired by every mutating method
    /// (create/update/delete) after the DB write succeeds, so the engine
    /// reloads and recomputes its deadlines without polling. The engine
    /// fetches this via [`Self::engine_notify`] at spawn time. `Notify`
    /// coalesces permits, so N mutations between engine wakes collapse to
    /// one reload — the engine's recompute is drain-all (reads the full
    /// table), so no mutation is ever missed.
    engine_notify: std::sync::Arc<Notify>,
}

impl WorkScheduleStore {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create ws db dir {}", parent.display()))?;
        }
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let mut opts = ConnectOptions::new(url);
        opts.max_connections(4);
        opts.map_sqlx_sqlite_opts(|o| {
            o.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
                .busy_timeout(std::time::Duration::from_secs(5))
        });
        let db = Database::connect(opts).await.context("connect ws db")?;
        super::migration::Migrator::up(&db, None)
            .await
            .context("apply ws migrations")?;
        Ok(Self {
            db,
            engine_notify: std::sync::Arc::new(Notify::new()),
        })
    }

    /// Open an in-memory store (for tests).
    #[cfg(test)]
    pub async fn open_in_memory() -> Self {
        let mut opts = ConnectOptions::new("sqlite::memory:".to_owned());
        opts.max_connections(1);
        let db = Database::connect(opts).await.expect("in-memory db");
        super::migration::Migrator::up(&db, None)
            .await
            .expect("migrations");
        Self {
            db,
            engine_notify: std::sync::Arc::new(Notify::new()),
        }
    }

    /// The engine's wake signal. The engine task holds a clone for its
    /// `select!` loop; mutations on this store fire `notify_one()` on it.
    pub fn engine_notify(&self) -> &std::sync::Arc<Notify> {
        &self.engine_notify
    }

    pub async fn create(&self, schedule: &WorkSchedule) -> Result<()> {
        let model = ActiveModel {
            id: Set(schedule.id.clone()),
            spec: Set(Some(serde_json::to_string(&schedule.spec).expect("spec serializes"))),
            pre_warn_minutes: Set(schedule.pre_warn_minutes as i64),
            final_warn_minutes: Set(schedule.final_warn_minutes as i64),
            wake_prompt: Set(schedule.wake_prompt.clone()),
            status: Set(schedule.status.to_string()),
            created_at: Set(to_unix(schedule.created_at)),
        };
        Entity::insert(model).exec(&self.db).await?;
        self.engine_notify.notify_one();
        Ok(())
    }

    /// The tagma's single schedule, ordered oldest-first (first PUT wins
    /// the singleton slot; later rows are unreachable in practice).
    pub async fn get_singleton(&self) -> Result<Option<WorkSchedule>> {
        Ok(Entity::find()
            .order_by_asc(Column::CreatedAt)
            .one(&self.db)
            .await?
            .map(decode))
    }


    pub async fn update(&self, schedule: &WorkSchedule) -> Result<bool> {
        let result = Entity::update_many()
            .col_expr(
                Column::Spec,
                serde_json::to_string(&schedule.spec)
                    .expect("spec serializes")
                    .into(),
            )
            .col_expr(
                Column::PreWarnMinutes,
                (schedule.pre_warn_minutes as i64).into(),
            )
            .col_expr(
                Column::FinalWarnMinutes,
                (schedule.final_warn_minutes as i64).into(),
            )
            .col_expr(Column::WakePrompt, schedule.wake_prompt.clone().into())
            .col_expr(Column::Status, schedule.status.to_string().into())
            .filter(Column::Id.eq(schedule.id.clone()))
            .exec(&self.db)
            .await?;
        let updated = result.rows_affected > 0;
        if updated {
            // Wake the engine only on a real change: a no-op PUT (unknown
            // id) would otherwise cost a redundant recompute pass.
            self.engine_notify.notify_one();
        }
        Ok(updated)
    }

    /// Test-only status toggle. NOTE: deliberately does NOT notify — it is
    /// unreachable in production. If ever un-gated, it MUST fire
    /// `self.engine_notify.notify_one()` like the other mutators, or the
    /// engine will not see the change.
    #[cfg(test)]
    pub async fn update_status(&self, id: &str, status: WorkScheduleStatus) -> Result<bool> {
        let result = Entity::update_many()
            .col_expr(Column::Status, status.to_string().into())
            .filter(Column::Id.eq(id))
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected > 0)
    }

}

fn decode(m: entities::work_schedule::Model) -> WorkSchedule {
    let status = m.status.parse().unwrap_or_else(|_| {
        tracing::warn!(schedule_id = %m.id, raw = %m.status, "corrupt status; defaulting");
        Default::default()
    });
    let id = m.id.clone();
    WorkSchedule {
        id: id.clone(),
        spec: m
            .spec
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| {
                tracing::warn!(schedule_id = %id, "corrupt spec; defaulting");
                Spec::Weekly { days: 1, start_minute: 0, end_minute: DAY_MINUTES }
            }),
        pre_warn_minutes: m.pre_warn_minutes as u32,
        final_warn_minutes: m.final_warn_minutes as u32,
        wake_prompt: m.wake_prompt,
        status,
        created_at: from_unix(m.created_at),
    }
}

fn to_unix(t: OffsetDateTime) -> i64 {
    t.to_offset(UtcOffset::UTC).unix_timestamp()
}

fn from_unix(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap_or_else(|_| {
        tracing::warn!(secs, "corrupt unix timestamp; clamping to epoch");
        OffsetDateTime::UNIX_EPOCH
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn open_tmp() -> (WorkScheduleStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = WorkScheduleStore::open(&dir.path().join("ws.sqlite"))
            .await
            .unwrap();
        (store, dir)
    }

    fn sample(id: &str) -> WorkSchedule {
        WorkSchedule {
            id: id.into(),
            spec: Spec::Weekly { days: 0b0001_1111, start_minute: 540, end_minute: 1020 },
            pre_warn_minutes: 10,
            final_warn_minutes: 5,
            wake_prompt: "Good morning.".into(),
            status: WorkScheduleStatus::Active,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn create_then_get_round_trips() {
        let (store, _d) = open_tmp().await;
        store.create(&sample("ws1")).await.unwrap();
        let got = store.get_singleton().await.unwrap().unwrap();
        assert_eq!(got.spec, sample("x").spec);
    }


    #[tokio::test]
    async fn empty_store_returns_none() {
        let (store, _d) = open_tmp().await;
        assert!(store.get_singleton().await.unwrap().is_none());
    }


    #[tokio::test]
    async fn update_changes_fields() {
        let (store, _d) = open_tmp().await;
        let mut s = sample("ws1");
        store.create(&s).await.unwrap();
        s.pre_warn_minutes = 15;
        assert!(store.update(&s).await.unwrap());
        let got = store.get_singleton().await.unwrap().unwrap();
        assert_eq!(got.pre_warn_minutes, 15);
    }

    #[tokio::test]
    async fn update_status_toggles() {
        let (store, _d) = open_tmp().await;
        store.create(&sample("ws1")).await.unwrap();
        assert!(
            store
                .update_status("ws1", WorkScheduleStatus::Paused)
                .await
                .unwrap()
        );
        assert_eq!(
            store.get_singleton().await.unwrap().unwrap().status,
            WorkScheduleStatus::Paused
        );
    }

}
