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

use super::WorkSchedule;
#[cfg(test)]
use super::WorkScheduleStatus;
use super::spec::Spec;
#[cfg(test)]
use super::spec::Window;

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
            pub final_warn_prompt: String,
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
            spec: Set(Some(
                serde_json::to_string(&schedule.spec).expect("spec serializes"),
            )),
            pre_warn_minutes: Set(schedule.pre_warn_minutes as i64),
            final_warn_minutes: Set(schedule.final_warn_minutes as i64),
            final_warn_prompt: Set(schedule.final_warn_prompt.clone().unwrap_or_default()),
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
            .col_expr(
                Column::FinalWarnPrompt,
                schedule
                    .final_warn_prompt
                    .clone()
                    .unwrap_or_default()
                    .into(),
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

    #[cfg(test)]
    /// Test-only: clear every row (undo the migration-04 seed) so tests
    /// can own the singleton slot.
    pub async fn delete_all(&self) -> Result<()> {
        Entity::delete_many().exec(&self.db).await?;
        Ok(())
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
                Spec::Always
            }),
        pre_warn_minutes: m.pre_warn_minutes as u32,
        final_warn_minutes: m.final_warn_minutes as u32,
        // '' is the stored form of "use the built-in default".
        final_warn_prompt: (!m.final_warn_prompt.is_empty()).then_some(m.final_warn_prompt),
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

    #[tokio::test]
    async fn fresh_migrate_seeds_always_singleton() {
        let (store, _d) = open_tmp().await;
        // Migration 04 clears any rows and seeds the singleton slot with
        // the always-on spec — the unset state no longer exists.
        let got = store.get_singleton().await.unwrap().expect("seeded");
        assert_eq!(got.spec, Spec::Always);
        assert_eq!(got.status, WorkScheduleStatus::Active);
    }

    #[tokio::test]
    async fn update_replaces_seed_spec_in_place() {
        let (store, _d) = open_tmp().await;
        let seed = store.get_singleton().await.unwrap().expect("seeded");
        let mut next = seed.clone();
        next.spec = Spec::Weekly {
            days: 0b0001_1111,
            windows: vec![Window {
                start_minute: 540,
                end_minute: 1020,
            }],
        };
        store.update(&next).await.unwrap();
        let got = store.get_singleton().await.unwrap().unwrap();
        assert_eq!(got.spec, next.spec);
        // The singleton slot is replaced in place, not appended.
        assert_eq!(got.id, seed.id);
    }

    #[tokio::test]
    async fn update_changes_fields() {
        let (store, _d) = open_tmp().await;
        let mut s = store.get_singleton().await.unwrap().expect("seeded");
        s.pre_warn_minutes = 15;
        assert!(store.update(&s).await.unwrap());
        let got = store.get_singleton().await.unwrap().unwrap();
        assert_eq!(got.pre_warn_minutes, 15);
    }

    #[tokio::test]
    async fn final_warn_prompt_round_trips_and_normalizes_empty() {
        let (store, _d) = open_tmp().await;
        let mut s = store.get_singleton().await.unwrap().expect("seeded");
        // The migration default is '' — the stored form of the default.
        assert_eq!(s.final_warn_prompt, None);
        s.final_warn_prompt = Some("wrap up {N}".into());
        assert!(store.update(&s).await.unwrap());
        let got = store.get_singleton().await.unwrap().unwrap();
        assert_eq!(got.final_warn_prompt.as_deref(), Some("wrap up {N}"));
        let mut cleared = got;
        cleared.final_warn_prompt = None;
        assert!(store.update(&cleared).await.unwrap());
        assert_eq!(
            store
                .get_singleton()
                .await
                .unwrap()
                .unwrap()
                .final_warn_prompt,
            None
        );
    }

    #[tokio::test]
    async fn update_status_toggles() {
        let (store, _d) = open_tmp().await;
        let id = store.get_singleton().await.unwrap().expect("seeded").id;
        assert!(
            store
                .update_status(&id, WorkScheduleStatus::Paused)
                .await
                .unwrap()
        );
        assert_eq!(
            store.get_singleton().await.unwrap().unwrap().status,
            WorkScheduleStatus::Paused
        );
    }
}
