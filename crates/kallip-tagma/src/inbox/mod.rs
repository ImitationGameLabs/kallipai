//! Per-agent message inbox: a SQLite-backed message store that is the sole
//! durable entry point for all external messages.
//!
//! The inbox stores full message bodies with a category (`direct` or
//! `buffered`) and a lifecycle status (`unread`, `read`, `done`). Direct
//! messages are pulled by the agent task loop on wake (via `pull_undelivered`);
//! buffered messages are consumed by the agent via CLI tools. Messages persist
//! after pull (for reference) and are evicted FIFO at the retention cap.
//!
//! Events are persisted to SQLite (`inboxes.sqlite`) so they survive tagma
//! restarts.

pub mod entities;
pub mod migration;

use std::path::Path;

use anyhow::{Context, Result};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveValue::Set, ConnectOptions, Database, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Statement,
};
use sea_orm_migration::MigratorTrait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::UtcOffset;
use tokio::fs;

use crate::state::AgentId;
use kallip_common::protocol::{InboxEntry, InboxSummary};

use entities::inbox_event::{ActiveModel, Column, Entity};

/// Default maximum number of events retained per inbox before truncation.
pub const DEFAULT_MAX_RETAINED: usize = 500;

/// A single message in an agent's inbox.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BufferedEvent {
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub source: String,
    pub body: String,
}

/// Re-exported from kallip_common::protocol for internal use.

#[derive(Clone, Debug, Default)]
pub struct InboxFilter {
    pub status: Option<String>,
    pub limit: Option<u32>,
}

/// SQLite-backed store of per-agent message inboxes.
#[derive(Clone)]
pub struct InboxStore {
    db: DatabaseConnection,
    max_retained: usize,
}

impl InboxStore {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create inbox db dir {}", parent.display()))?;
        }
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let mut opts = ConnectOptions::new(url);
        opts.max_connections(4);
        opts.map_sqlx_sqlite_opts(|o| {
            o.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
                .busy_timeout(std::time::Duration::from_secs(5))
        });
        let db = Database::connect(opts).await.context("connect inbox db")?;
        migration::Migrator::up(&db, None)
            .await
            .context("apply inbox migrations")?;
        Ok(Self {
            db,
            max_retained: DEFAULT_MAX_RETAINED,
        })
    }

    #[cfg(test)]
    pub async fn open_in_memory() -> Self {
        let mut opts = ConnectOptions::new("sqlite::memory:".to_owned());
        opts.max_connections(1);
        let db = Database::connect(opts).await.expect("in-memory db");
        migration::Migrator::up(&db, None)
            .await
            .expect("migrations");
        Self {
            db,
            max_retained: DEFAULT_MAX_RETAINED,
        }
    }

    /// Push a message into an agent's inbox.
    pub async fn push(&self, agent_id: AgentId, event: BufferedEvent) {
        let model = ActiveModel {
            agent_id: Set(agent_id.as_ref().to_string()),
            timestamp: Set(to_unix(event.timestamp)),
            source: Set(event.source),
            body: Set(event.body),
            status: Set("unread".to_string()),
            delivered: Set(0),
            ..Default::default()
        };
        if let Err(e) = Entity::insert(model).exec(&self.db).await {
            tracing::warn!(error = %e, "inbox push failed");
            return;
        }
        let limit = self.max_retained as i64;
        self.db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "DELETE FROM inbox_events WHERE agent_id = ? AND delivered = 1 AND id NOT IN \
             (SELECT id FROM inbox_events WHERE agent_id = ? AND delivered = 1 ORDER BY id DESC LIMIT ?)",
            [agent_id.as_ref().into(), agent_id.as_ref().into(), limit.into()],
        )).await.ok();
    }

    /// Atomically mark ALL undelivered direct messages as delivered and return
    /// them as a formatted digest. Returns None when no undelivered direct.
    ///
    /// Uses UPDATE...RETURNING (no LIMIT) to drain all in one atomic call,
    /// surviving notify coalescing where multiple notify_one() collapse to one wake.
    pub async fn pull_undelivered(&self, agent_id: &AgentId) -> Option<String> {
        let rows = self
            .db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Sqlite,
                "UPDATE inbox_events SET delivered = 1 \
             WHERE agent_id = ? AND delivered = 0 \
             RETURNING id, timestamp, source, body",
                [agent_id.as_ref().into()],
            ))
            .await
            .ok()?;

        if rows.is_empty() {
            return None;
        }

        let mut events: Vec<(i64, BufferedEvent)> = rows
            .into_iter()
            .map(|r| {
                let id: i64 = r.try_get("", "id").unwrap_or(0);
                let ts: i64 = r.try_get("", "timestamp").unwrap_or(0);
                let source: String = r.try_get("", "source").unwrap_or_default();
                let body: String = r.try_get("", "body").unwrap_or_default();
                (
                    id,
                    BufferedEvent {
                        timestamp: from_unix(ts),
                        source,
                        body,
                    },
                )
            })
            .collect();
        events.sort_by_key(|(id, _)| *id);

        let count = events.len();
        let mut lines = Vec::with_capacity(count);
        lines.push(format!(
            "\u{1F4EC} While you were away ({count} message{}):",
            if count == 1 { "" } else { "s" }
        ));
        for (_, ev) in &events {
            lines.push(format!(
                "  [{}] {}:",
                ev.timestamp
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "?".into()),
                ev.source
            ));
            for line in ev.body.lines() {
                lines.push(format!("    {line}"));
            }
        }

        Some(lines.join("\n"))
    }

    async fn count_unread(&self, agent_id: &AgentId) -> usize {
        Entity::find()
            .filter(Column::AgentId.eq(agent_id.as_ref()))
            .filter(Column::Status.eq("unread"))
            .count(&self.db)
            .await
            .unwrap_or(0) as usize
    }

    pub async fn list(&self, agent_id: &AgentId, filter: &InboxFilter) -> Vec<InboxEntry> {
        let limit = filter.limit.unwrap_or(50) as u64;
        let mut q = Entity::find()
            .filter(Column::AgentId.eq(agent_id.as_ref()))
            .order_by_desc(Column::Id);
        if let Some(ref s) = filter.status {
            q = q.filter(Column::Status.eq(s));
        }
        q.limit(limit)
            .all(&self.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(model_to_entry)
            .collect()
    }

    pub async fn read(&self, agent_id: &AgentId, msg_id: i64) -> Option<InboxEntry> {
        let model = Entity::find_by_id(msg_id)
            .filter(Column::AgentId.eq(agent_id.as_ref()))
            .one(&self.db)
            .await
            .map_err(|e| tracing::warn!(error = %e, "inbox read find failed"))
            .ok()
            .flatten()?;
        let mut a: ActiveModel = model.into();
        a.status = Set("read".to_string());
        let updated = a
            .update(&self.db)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "inbox read status update failed");
                e
            })
            .ok()?;
        Some(model_to_entry(updated))
    }

    pub async fn summary(&self, agent_id: &AgentId) -> InboxSummary {
        let total = Entity::find()
            .filter(Column::AgentId.eq(agent_id.as_ref()))
            .count(&self.db)
            .await
            .unwrap_or(0) as usize;
        let unread = self.count_unread(agent_id).await;
        InboxSummary { total, unread }
    }

    pub async fn mark_done(&self, agent_id: &AgentId, msg_id: i64) -> bool {
        match Entity::find_by_id(msg_id)
            .filter(Column::AgentId.eq(agent_id.as_ref()))
            .one(&self.db)
            .await
        {
            Ok(Some(m)) => {
                let mut a: ActiveModel = m.into();
                a.status = Set("done".to_string());
                match a.update(&self.db).await {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::warn!(error = %e, "inbox mark_done update failed");
                        false
                    }
                }
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "inbox mark_done find failed");
                false
            }
        }
    }

    pub async fn clear(&self, agent_id: &AgentId, all: bool) -> usize {
        let mut q = Entity::delete_many().filter(Column::AgentId.eq(agent_id.as_ref()));
        if !all {
            q = q.filter(Column::Status.eq("done"));
        } else {
            // Guard: never delete undelivered messages.
            q = q.filter(Column::Delivered.eq(1));
        }
        q.exec(&self.db)
            .await
            .map(|r| r.rows_affected as usize)
            .unwrap_or(0)
    }

    pub async fn clear_for(&self, agent_id: &AgentId) {
        Entity::delete_many()
            .filter(Column::AgentId.eq(agent_id.as_ref()))
            .exec(&self.db)
            .await
            .ok();
    }

    #[cfg(test)]
    pub async fn len_for(&self, agent_id: &AgentId) -> usize {
        Entity::find()
            .filter(Column::AgentId.eq(agent_id.as_ref()))
            .count(&self.db)
            .await
            .unwrap_or(0) as usize
    }
}

fn model_to_entry(m: entities::inbox_event::Model) -> InboxEntry {
    InboxEntry {
        id: m.id,
        timestamp: from_unix(m.timestamp),
        source: m.source,
        body: m.body,
        status: m.status,
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
    use time::macros::datetime;

    fn event(source: &str, body: &str) -> BufferedEvent {
        BufferedEvent {
            timestamp: OffsetDateTime::now_utc(),
            source: source.to_string(),
            body: body.to_string(),
        }
    }

    #[tokio::test]
    async fn push_and_pull_single() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store
            .push(id.clone(), event("operator", "hello world"))
            .await;
        let msg = store.pull_undelivered(&id).await.unwrap();
        assert!(msg.contains("1 message"));
        assert!(msg.contains("operator"));
        assert!(msg.contains("hello world"));
    }

    #[tokio::test]
    async fn pull_empty_returns_none() {
        let store = InboxStore::open_in_memory().await;
        assert!(store.pull_undelivered(&AgentId::random()).await.is_none());
    }

    #[tokio::test]
    async fn pull_drains_all_undelivered() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("operator", "first")).await;
        store.push(id.clone(), event("cron", "second")).await;
        store.push(id.clone(), event("agent:abc", "third")).await;
        let msg = store.pull_undelivered(&id).await.unwrap();
        assert!(msg.contains("3 messages"));
        assert!(store.pull_undelivered(&id).await.is_none());
    }

    #[tokio::test]
    async fn pull_preserves_order() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("a", "first")).await;
        store.push(id.clone(), event("b", "second")).await;
        store.push(id.clone(), event("c", "third")).await;
        let msg = store.pull_undelivered(&id).await.unwrap();
        // Format: header, then per-message: source line + indented body lines.
        assert!(msg.contains("first"));
        assert!(msg.contains("second"));
        assert!(msg.contains("third"));
        // Verify order: first appears before second.
        let fi = msg.find("first").unwrap();
        let si = msg.find("second").unwrap();
        let ti = msg.find("third").unwrap();
        assert!(fi < si && si < ti, "messages should be in insertion order");
    }

    #[tokio::test]
    async fn pull_marks_delivered() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("s", "msg")).await;
        let _ = store.pull_undelivered(&id).await;
        assert_eq!(store.len_for(&id).await, 1);
        assert!(store.pull_undelivered(&id).await.is_none());
    }

    #[tokio::test]
    async fn eviction_protects_undelivered() {
        let mut store = InboxStore::open_in_memory().await;
        store.max_retained = 3;
        let id = AgentId::random();
        // Push 5 undelivered messages -- none evicted (cap is for delivered only).
        for i in 0..5u8 {
            store.push(id.clone(), event("s", &format!("ev-{i}"))).await;
        }
        assert_eq!(
            store.len_for(&id).await,
            5,
            "undelivered must not be evicted"
        );
        // Pull all -> delivered=1. Next push triggers eviction of delivered.
        let _ = store.pull_undelivered(&id).await;
        // Push 1 more -> eviction fires: 5 delivered > cap 3, evicts 2 oldest.
        store.push(id.clone(), event("s", "after-pull")).await;
        let remaining = store.list(&id, &InboxFilter::default()).await;
        // 3 delivered (ev-4, ev-3, ev-2) + 1 undelivered (after-pull) = 4.
        assert_eq!(remaining.len(), 4, "got {remaining:?}");
        assert!(
            !remaining.iter().any(|e| e.body.contains("ev-0")),
            "oldest delivered evicted"
        );
        assert!(
            !remaining.iter().any(|e| e.body.contains("ev-1")),
            "2nd oldest delivered evicted"
        );
        assert!(
            remaining.iter().any(|e| e.body.contains("after-pull")),
            "new undelivered kept"
        );
    }

    #[tokio::test]
    async fn clear_for_removes_events() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("s", "x")).await;
        store.clear_for(&id).await;
        assert_eq!(store.len_for(&id).await, 0);
    }

    #[tokio::test]
    async fn list_returns_entries_newest_first() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("op", "hello")).await;
        store.push(id.clone(), event("cron", "tick")).await;
        let entries = store.list(&id, &InboxFilter::default()).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].body, "tick");
        assert_eq!(entries[1].body, "hello");
    }

    #[tokio::test]
    async fn read_marks_as_read() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("op", "read me")).await;
        let msg_id = store.list(&id, &InboxFilter::default()).await[0].id;
        let entry = store.read(&id, msg_id).await.unwrap();
        assert_eq!(entry.body, "read me");
        assert_eq!(entry.status, "read");
    }

    #[tokio::test]
    async fn summary_counts() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("op", "one")).await;
        store.push(id.clone(), event("op", "two")).await;
        let summary = store.summary(&id).await;
        assert_eq!(summary.total, 2);
        assert_eq!(summary.unread, 2);
    }

    #[tokio::test]
    async fn mark_done_and_clear_done() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("op", "done-me")).await;
        let msg_id = store.list(&id, &InboxFilter::default()).await[0].id;
        assert!(store.mark_done(&id, msg_id).await);
        assert_eq!(store.clear(&id, false).await, 1);
        assert_eq!(store.len_for(&id).await, 0);
    }

    #[tokio::test]
    async fn clear_all() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("op", "one")).await;
        store.push(id.clone(), event("op", "two")).await;
        // --all with undelivered messages: none cleared (guard).
        assert_eq!(store.clear(&id, true).await, 0);
        // Pull first, then clear all.
        let _ = store.pull_undelivered(&id).await;
        assert_eq!(store.clear(&id, true).await, 2);
        assert_eq!(store.len_for(&id).await, 0);
    }

    #[tokio::test]
    async fn persistence_survives_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("inboxes.sqlite");
        let id = AgentId::random();
        {
            let store = InboxStore::open(&path).await.unwrap();
            store
                .push(id.clone(), event("operator", "persistent"))
                .await;
        }
        let store = InboxStore::open(&path).await.unwrap();
        assert_eq!(store.len_for(&id).await, 1);
        assert!(
            store
                .pull_undelivered(&id)
                .await
                .unwrap()
                .contains("persistent")
        );
    }

    #[tokio::test]
    async fn concurrent_push_and_pull_no_loss() {
        let store = InboxStore::open_in_memory().await;
        let id = AgentId::random();
        store.push(id.clone(), event("s", "seed-0")).await;
        store.push(id.clone(), event("s", "seed-1")).await;
        let store2 = store.clone();
        let id2 = id.clone();
        let push_handle = tokio::spawn(async move {
            store2.push(id2, event("s", "late")).await;
        });
        let pull_msg = store.pull_undelivered(&id).await;
        push_handle.await.unwrap();
        let in_inbox = store.len_for(&id).await;
        if let Some(ref msg) = pull_msg {
            assert!(
                msg.contains("late") || in_inbox > 2,
                "late message lost: not in pull, inbox has {in_inbox}"
            );
        } else {
            assert!(in_inbox >= 1, "messages lost on empty pull");
        }
    }
}

#[tokio::test]
async fn migration_backfills_and_preserves() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("inboxes.sqlite");

    // Seed at the m_01 schema (old columns).
    {
        let mut opts = ConnectOptions::new(format!("sqlite://{}?mode=rwc", path.display()));
        opts.max_connections(1);
        let db = Database::connect(opts).await.unwrap();
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "CREATE TABLE inbox_events ( \
                    id INTEGER PRIMARY KEY AUTOINCREMENT, \
                    agent_id TEXT NOT NULL, \
                    timestamp INTEGER NOT NULL, \
                    source TEXT NOT NULL, \
                    summary TEXT NOT NULL \
                )",
            [],
        ))
        .await
        .unwrap();
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "INSERT INTO inbox_events (agent_id, timestamp, source, summary) \
                 VALUES ('agent-x', 1700000000, 'operator', 'legacy message body')",
            [],
        ))
        .await
        .unwrap();
    }

    // Open with new migration -- table-rebuild should run.
    let store = InboxStore::open(&path).await.unwrap();
    let id: AgentId = "agent-x".parse().unwrap();
    let msg = store
        .pull_undelivered(&id)
        .await
        .expect("legacy row should be undelivered");
    assert!(
        msg.contains("legacy message body"),
        "body backfilled from summary: {msg}"
    );

    // Verify summary column no longer exists.
    let result = store.db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "INSERT INTO inbox_events (agent_id, timestamp, source, summary) VALUES ('x', 0, 'x', 'x')",
            [],
        )).await;
    assert!(result.is_err(), "summary column should be gone");
}

/// Concrete `MessagePuller` implementation: wraps an `InboxStore` + `AgentId`
/// so the runtime crate can pull undelivered messages without depending on the
/// tagma crate directly.
pub struct InboxPuller {
    store: InboxStore,
    agent_id: AgentId,
}

impl InboxPuller {
    pub fn new(store: InboxStore, agent_id: AgentId) -> Self {
        Self { store, agent_id }
    }
}

#[async_trait::async_trait]
impl kallip_runtime::agent_task::MessagePuller for InboxPuller {
    async fn pull_undelivered(&self) -> Option<String> {
        self.store.pull_undelivered(&self.agent_id).await
    }
}
