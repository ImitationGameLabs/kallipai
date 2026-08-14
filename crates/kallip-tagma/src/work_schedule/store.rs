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

use crate::state::AgentId;

use super::{WorkSchedule, WorkScheduleStatus};

pub(crate) mod entities {
    pub(crate) mod work_schedule {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "work_schedules")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
            pub name: String,
            pub agent_id: String,
            pub start_cron: String,
            pub end_cron: String,
            pub pre_warn_minutes: i64,
            pub final_warn_minutes: i64,
            #[sea_orm(column_type = "Text")]
            pub wake_prompt: String,
            pub status: String,
            pub timezone: Option<String>,
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
}

impl WorkScheduleStore {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await
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
        super::migration::Migrator::up(&db, None).await.context("apply ws migrations")?;
        Ok(Self { db })
    }

    /// Open an in-memory store (for tests).
    #[cfg(test)]
    pub async fn open_in_memory() -> Self {
        let mut opts = ConnectOptions::new("sqlite::memory:".to_owned());
        opts.max_connections(1);
        let db = Database::connect(opts).await.expect("in-memory db");
        super::migration::Migrator::up(&db, None).await.expect("migrations");
        Self { db }
    }

    pub async fn create(&self, schedule: &WorkSchedule) -> Result<()> {
        let model = ActiveModel {
            id: Set(schedule.id.clone()),
            name: Set(schedule.name.clone()),
            agent_id: Set(schedule.agent_id.as_ref().to_string()),
            start_cron: Set(schedule.start_cron.clone()),
            end_cron: Set(schedule.end_cron.clone()),
            pre_warn_minutes: Set(schedule.pre_warn_minutes as i64),
            final_warn_minutes: Set(schedule.final_warn_minutes as i64),
            wake_prompt: Set(schedule.wake_prompt.clone()),
            status: Set(schedule.status.to_string()),
            timezone: Set(schedule.timezone.clone()),
            created_at: Set(to_unix(schedule.created_at)),
        };
        Entity::insert(model).exec(&self.db).await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<WorkSchedule>> {
        Ok(Entity::find_by_id(id).one(&self.db).await?.map(decode))
    }

    pub async fn list(
        &self, agent_id: Option<&AgentId>, status: Option<WorkScheduleStatus>,
    ) -> Result<Vec<WorkSchedule>> {
        let mut q = Entity::find();
        if let Some(aid) = agent_id { q = q.filter(Column::AgentId.eq(aid.as_ref())); }
        if let Some(s) = status { q = q.filter(Column::Status.eq(s.to_string())); }
        Ok(q.order_by_desc(Column::CreatedAt).all(&self.db).await?
            .into_iter().map(decode).collect())
    }

    pub async fn update(&self, schedule: &WorkSchedule) -> Result<bool> {
        let result = Entity::update_many()
            .col_expr(Column::Name, schedule.name.clone().into())
            .col_expr(Column::StartCron, schedule.start_cron.clone().into())
            .col_expr(Column::EndCron, schedule.end_cron.clone().into())
            .col_expr(Column::PreWarnMinutes, (schedule.pre_warn_minutes as i64).into())
            .col_expr(Column::FinalWarnMinutes, (schedule.final_warn_minutes as i64).into())
            .col_expr(Column::WakePrompt, schedule.wake_prompt.clone().into())
            .col_expr(Column::Status, schedule.status.to_string().into())
            .col_expr(Column::Timezone, schedule.timezone.clone().into())
            .filter(Column::Id.eq(schedule.id.clone()))
            .exec(&self.db).await?;
        Ok(result.rows_affected > 0)
    }

    #[cfg(test)]
    pub async fn update_status(&self, id: &str, status: WorkScheduleStatus) -> Result<bool> {
        let result = Entity::update_many()
            .col_expr(Column::Status, status.to_string().into())
            .filter(Column::Id.eq(id))
            .exec(&self.db).await?;
        Ok(result.rows_affected > 0)
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        let result = Entity::delete_many()
            .filter(Column::Id.eq(id))
            .exec(&self.db).await?;
        Ok(result.rows_affected > 0)
    }
}

fn decode(m: entities::work_schedule::Model) -> WorkSchedule {
    let status = m.status.parse().unwrap_or_else(|_| {
        tracing::warn!(schedule_id = %m.id, raw = %m.status, "corrupt status; defaulting");
        Default::default()
    });
    WorkSchedule {
        id: m.id, name: m.name, agent_id: m.agent_id.into(),
        start_cron: m.start_cron, end_cron: m.end_cron,
        pre_warn_minutes: m.pre_warn_minutes as u32,
        final_warn_minutes: m.final_warn_minutes as u32,
        wake_prompt: m.wake_prompt, status, timezone: m.timezone,
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
        let store = WorkScheduleStore::open(&dir.path().join("ws.sqlite")).await.unwrap();
        (store, dir)
    }

    fn sample(id: &str) -> WorkSchedule {
        WorkSchedule {
            id: id.into(), name: "Day shift".into(),
            agent_id: "agent-1".parse().unwrap(),
            start_cron: "0 9 * * 1-5".into(), end_cron: "0 17 * * 1-5".into(),
            pre_warn_minutes: 10, final_warn_minutes: 5,
            wake_prompt: "Good morning.".into(),
            status: WorkScheduleStatus::Active, timezone: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn create_then_get_round_trips() {
        let (store, _d) = open_tmp().await;
        store.create(&sample("ws1")).await.unwrap();
        let got = store.get("ws1").await.unwrap().unwrap();
        assert_eq!(got.name, "Day shift");
        assert_eq!(got.start_cron, "0 9 * * 1-5");
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let (store, _d) = open_tmp().await;
        assert!(store.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_filters_by_agent_and_status() {
        let (store, _d) = open_tmp().await;
        let mut a = sample("ws1"); a.agent_id = "agent-a".parse().unwrap();
        let mut b = sample("ws2"); b.agent_id = "agent-b".parse().unwrap();
        b.status = WorkScheduleStatus::Paused;
        store.create(&a).await.unwrap();
        store.create(&b).await.unwrap();
        let aid: AgentId = "agent-a".parse().unwrap();
        assert_eq!(store.list(Some(&aid), None).await.unwrap().len(), 1);
        assert_eq!(store.list(None, Some(WorkScheduleStatus::Active)).await.unwrap().len(), 1);
        assert_eq!(store.list(None, None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn update_changes_fields() {
        let (store, _d) = open_tmp().await;
        let mut s = sample("ws1");
        store.create(&s).await.unwrap();
        s.name = "Night shift".into();
        s.pre_warn_minutes = 15;
        assert!(store.update(&s).await.unwrap());
        let got = store.get("ws1").await.unwrap().unwrap();
        assert_eq!(got.name, "Night shift");
        assert_eq!(got.pre_warn_minutes, 15);
    }

    #[tokio::test]
    async fn update_status_toggles() {
        let (store, _d) = open_tmp().await;
        store.create(&sample("ws1")).await.unwrap();
        assert!(store.update_status("ws1", WorkScheduleStatus::Paused).await.unwrap());
        assert_eq!(store.get("ws1").await.unwrap().unwrap().status, WorkScheduleStatus::Paused);
    }

    #[tokio::test]
    async fn delete_removes_schedule() {
        let (store, _d) = open_tmp().await;
        store.create(&sample("ws1")).await.unwrap();
        assert!(store.delete("ws1").await.unwrap());
        assert!(store.get("ws1").await.unwrap().is_none());
    }
}
