//! The tagma's durable chat-history store: the source of truth a reconnecting
//! (or freshly-paired) device pulls via `TagmaControl::History` on each open,
//! so the user sees what it missed while the app was offline.
//!
//! Distinct from `kallip_runtime::history` (the agent's own LLM-turn log):
//! this is the *relay wire transcript* of the conversation — both directions:
//! outbound `TagmaReply::Event` frames the pump produced AND inbound
//! `TagmaRequest::SendMessage` frames the user sent, in arrival order, kept as
//! plaintext so the tagma can re-encrypt them under whatever epoch key is
//! current at pull time.
//! Plaintext at rest is consistent with the host-trust model
//! (`history.ndjson` / `ContextStore` already store plaintext on the host;
//! E2EE protects transit, not the endpoint).
//!
//! SQLite/sea-orm: single-writer (the pump appends) + the occasional pull
//! read, so WAL is mandatory (`journal_mode(WAL)`) with `synchronous=NORMAL`
//! to keep the per-event append off the fsync hot path. Schema is managed by
//! sea-orm-migration (see [`migration`]); new schema changes are new `m_*`
//! files appended to `Migrator`, never in-place edits to the init migration.

use std::path::Path;

use anyhow::{Context, Result};
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectOptions, Database, DatabaseConnection, QueryOrder, QuerySelect};
use sea_orm_migration::MigratorTrait;
use tracing::warn;

pub(crate) mod migration;

/// Default retention window in days (env: `KALLIP_TAGMA_RELAY_HISTORY_TTL_DAYS`).
/// This is the *normal* retention boundary — what a user can normally see and
/// re-pull on reconnect.
pub(crate) const DEFAULT_HISTORY_TTL_DAYS: u64 = 30;
/// Default row cap (env: `KALLIP_TAGMA_RELAY_HISTORY_CAP`). NOT a usage quota — only
/// a runaway backstop that bounds damage from abnormal volume (a buggy agent
/// spamming, a counter glitch, an attack). Normal usage within the TTL window
/// must never approach it; a `warn!` fires when it trims so the event is
/// observable. 100k rows is a few tens of MB in SQLite, trivial for the host
/// daemon.
pub(crate) const DEFAULT_HISTORY_CAP: u64 = 100_000;

/// A cloned handle to the chat-history store. `DatabaseConnection` is
/// internally `Arc`'d, so cloning is cheap and shares the pool.
pub(crate) type Db = DatabaseConnection;

pub(crate) mod entities {
    pub(crate) mod chat_history {
        use sea_orm::entity::prelude::*;

        /// One wire frame the pump has produced (outbound) or the user has sent
        /// (inbound). `id` is a monotonic row id (AUTOINCREMENT: never reused,
        /// even after GC), stamped onto the wire as `history_id` so the app can
        /// dedup/order across batch replay and live delivery.
        ///
        /// Invariant: a single tagma daemon owns exactly ONE conversation
        /// (`conversation_id` is derived from the tagma id in `RelayHandle`),
        /// so the column is constant within a DB. The schema is multi-conversation
        /// shaped (column + `(conversation_id, id)` index) for forward
        /// compatibility; if a future phase hosts multiple conversations per
        /// tagma, only the GC cap needs scoping (see `gc` in the parent module).
        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "chat_history")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = true)]
            pub id: i64,
            #[sea_orm(column_type = "Text")]
            pub conversation_id: String,
            /// `outbound` (agent -> user, payload = serialized `TagmaReply::Event`)
            /// or `inbound` (user -> agent, payload = serialized
            /// `TagmaRequest::SendMessage`). The replay loop reads `direction`
            /// to decide how to re-emit each row.
            #[sea_orm(column_type = "Text")]
            pub direction: String,
            /// The `TagmaReply` discriminant (`event`, etc.) for debugging /
            /// future filtering. The full payload is `payload`.
            #[sea_orm(column_type = "Text")]
            pub kind: String,
            /// Serialized `TagmaReply` (history_id left at 0; the row `id` is
            /// authoritative and stamped onto the wire frame at emit time).
            pub payload: Vec<u8>,
            /// Unix seconds. Indexed indirectly via the id ordering; GC keys off
            /// this. i64 (not OffsetDateTime) to avoid time-format drift in
            /// SQLite and keep GC a plain integer compare.
            pub created_at: i64,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }
}

use entities::chat_history::{Column, Entity};

/// One row returned for re-encryption + emit by the history pull paths.
/// `direction` tells the replay loop how to interpret `payload`: `outbound` ->
/// a serialized `TagmaReply::Event` (emit as-is); `inbound` -> a serialized
/// `TagmaRequest::SendMessage` (emit as a `TagmaReply::UserMessage` echo).
pub(crate) struct HistoryRow {
    pub id: i64,
    pub direction: String,
    pub payload: Vec<u8>,
}

/// Open (or create) the chat-history SQLite database at `path` and apply any
/// pending migrations. The parent directory must already exist with owner-only
/// permissions (the caller — `activate_relay` — sets that up alongside
/// `credentials/`).
pub(crate) async fn open(path: &Path) -> Result<Db> {
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let mut opts = ConnectOptions::new(url);
    // A small pool is enough: one writer (the pump) + a couple of concurrent
    // TagmaControl::History readers. WAL lets reads overlap writes.
    opts.max_connections(8);
    // Apply WAL + synchronous=NORMAL on every pooled connection via sea-orm's
    // sqlite-options hook. WAL is also persisted in the DB file header, but
    // setting it per-connection keeps the -wal/-shm siblings stable across
    // reopens; NORMAL keeps the per-event append off the fsync hot path.
    opts.map_sqlx_sqlite_opts(|o| {
        o.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
    });
    let db = Database::connect(opts).await?;
    migration::Migrator::up(&db, None)
        .await
        .context("apply chat_history migrations")?;
    Ok(db)
}

/// Append one outbound frame and return its row id (the `history_id` stamped
/// onto the wire reply). `kind` is the reply discriminant; `payload` is the
/// serialized `TagmaReply`.
pub(crate) async fn append(
    db: &Db,
    conversation_id: &str,
    direction: &str,
    kind: &str,
    payload: &[u8],
) -> Result<i64> {
    let now = unix_secs();
    let row = entities::chat_history::ActiveModel {
        conversation_id: sea_orm::Set(conversation_id.to_string()),
        direction: sea_orm::Set(direction.to_string()),
        kind: sea_orm::Set(kind.to_string()),
        payload: sea_orm::Set(payload.to_vec()),
        created_at: sea_orm::Set(now),
        ..Default::default()
    };
    let res = Entity::insert(row)
        .exec(db)
        .await
        .context("append chat_history")?;
    Ok(res.last_insert_id)
}

/// Read the most recent `n` rows for a conversation, oldest-first (i.e. in the
/// order they should be replayed). Used by the `latest` history-pull mode (a
/// first-time device with an empty local cache): the tagma re-encrypts these
/// under the fresh epoch key and emits them so the device immediately sees the
/// recent conversation.
pub(crate) async fn read_last_n(db: &Db, conversation_id: &str, n: u64) -> Result<Vec<HistoryRow>> {
    // Fetch newest-first (DESC), then reverse to oldest-first for replay.
    let mut rows = Entity::find()
        .filter(Column::ConversationId.eq(conversation_id))
        .order_by_desc(Column::Id)
        .limit(n)
        .all(db)
        .await
        .context("read chat_history last_n")?;
    rows.reverse();
    Ok(rows
        .into_iter()
        .map(|m| HistoryRow {
            id: m.id,
            direction: m.direction,
            payload: m.payload,
        })
        .collect())
}

/// Read up to `limit` rows with `id > after`, oldest-first, both directions
/// mixed (the incremental catch-up window). Used by the `after` history-pull
/// mode on reconnect: the app sends its rendered high-water mark and gets only
/// what it has not seen.
pub(crate) async fn read_after(
    db: &Db,
    conversation_id: &str,
    after: i64,
    limit: u32,
) -> Result<Vec<HistoryRow>> {
    let rows = Entity::find()
        .filter(Column::ConversationId.eq(conversation_id))
        .filter(Column::Id.gt(after))
        .order_by_asc(Column::Id)
        .limit(limit as u64)
        .all(db)
        .await
        .context("read chat_history after")?;
    Ok(rows
        .into_iter()
        .map(|m| HistoryRow {
            id: m.id,
            direction: m.direction,
            payload: m.payload,
        })
        .collect())
}

/// Read up to `limit` rows with `id < before`, oldest-first, both directions
/// mixed (the scroll-up lazy-load window). Used by the `before` history-pull
/// mode: the app sends the oldest id currently in view and gets the next older
/// chunk to prepend.
pub(crate) async fn read_before(
    db: &Db,
    conversation_id: &str,
    before: i64,
    limit: u32,
) -> Result<Vec<HistoryRow>> {
    // Fetch newest-first (DESC, bounded by `before`), then reverse to
    // oldest-first so the app can prepend in order.
    let mut rows = Entity::find()
        .filter(Column::ConversationId.eq(conversation_id))
        .filter(Column::Id.lt(before))
        .order_by_desc(Column::Id)
        .limit(limit as u64)
        .all(db)
        .await
        .context("read chat_history before")?;
    rows.reverse();
    Ok(rows
        .into_iter()
        .map(|m| HistoryRow {
            id: m.id,
            direction: m.direction,
            payload: m.payload,
        })
        .collect())
}

/// Delete rows older than `ttl_secs` (by `created_at`), then if the row count
/// exceeds `cap`, trim the oldest down to `cap`. Returns the number deleted.
/// Best-effort: a failure is logged, not propagated, so a GC fault never takes
/// down the relay.
///
/// The cap is **per-DB** (i.e. per-tagma), which today equals per-conversation:
/// the daemon owns a single conversation (`conversation_id` is derived from
/// the tagma id). If a future phase hosts multiple conversations per tagma,
/// scope this count + trim per `conversation_id` (the schema and read paths
/// are already multi-conversation ready); TTL is age-based and conv-agnostic,
/// so it stays as-is.
pub(crate) async fn gc(db: &Db, ttl_secs: u64, cap: u64) -> usize {
    let mut deleted = 0usize;
    // Guard against a huge env-configured TTL overflowing i64 (u64::MAX as i64
    // is -1, which would make the cutoff future-dated and match every row).
    let ttl = ttl_secs.min(i64::MAX as u64) as i64;
    let cutoff = unix_secs().saturating_sub(ttl);
    // TTL delete: rows older than the retention window.
    match Entity::delete_many()
        .filter(Column::CreatedAt.lt(cutoff))
        .exec(db)
        .await
    {
        Ok(r) => deleted += r.rows_affected as usize,
        Err(e) => warn!(error = %e, "chat_history gc: ttl delete failed"),
    }
    // Capacity cap: if over cap, trim the oldest (count - cap) rows. The cap is
    // a runaway backstop, not a usage quota (see DEFAULT_HISTORY_CAP), so
    // trimming is an abnormal signal worth a warn.
    if let Ok(count) = Entity::find().count(db).await
        && count > cap
    {
        let over = count - cap;
        warn!(
            cap,
            count,
            trimming = over,
            "chat_history hit the runaway cap; normal usage should not reach this"
        );
        // No clean sea-orm "delete bottom N" idiom; raw SQL keyed on the oldest
        // N ids. `over` is a u64 derived from a row count, never user input.
        let sql = format!(
            "DELETE FROM chat_history WHERE id IN ( \
                SELECT id FROM chat_history ORDER BY id ASC LIMIT {over} \
             );"
        );
        match db.execute_unprepared(&sql).await {
            Ok(r) => deleted += r.rows_affected() as usize,
            Err(e) => warn!(error = %e, "chat_history gc: cap trim failed"),
        }
    }
    deleted
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn open_tmp() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let db = open(&dir.path().join("h.sqlite")).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn open_applies_migrations_idempotently() {
        // open() applies pending migrations; a second open on the same file is
        // a no-op (sea-orm-migration records applied migrations). The schema is
        // usable after migration: append + read round-trip.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("h.sqlite");
        {
            let _db = open(&path).await.unwrap();
        }
        let db = open(&path).await.unwrap();
        let id = append(&db, "c1", "outbound", "event", b"x").await.unwrap();
        assert!(id > 0);
        assert_eq!(read_last_n(&db, "c1", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn append_returns_monotonic_ids() {
        let (db, _d) = open_tmp().await;
        let a = append(&db, "c1", "outbound", "event", b"{}").await.unwrap();
        let b = append(&db, "c1", "outbound", "event", b"{}").await.unwrap();
        let c = append(&db, "c1", "outbound", "event", b"{}").await.unwrap();
        assert!(a < b && b < c, "ids must be monotonic: {a},{b},{c}");
    }

    #[tokio::test]
    async fn read_last_n_returns_newest_oldest_first() {
        let (db, _d) = open_tmp().await;
        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(append(&db, "c1", "outbound", "event", b"x").await.unwrap());
        }
        // Asking for the last 3 returns ids[2..5], oldest-first for replay.
        let got = read_last_n(&db, "c1", 3).await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, ids[2]);
        assert_eq!(got[2].id, ids[4]);
        // Asking for more than stored returns all, oldest-first.
        let got = read_last_n(&db, "c1", 100).await.unwrap();
        assert_eq!(got.len(), 5);
        assert_eq!(got[0].id, ids[0]);
        // Another conversation is invisible.
        assert!(read_last_n(&db, "c2", 3).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gc_respects_ttl_and_cap() {
        let (db, _d) = open_tmp().await;
        // Insert with an artificially old created_at via raw SQL so TTL hits.
        for _ in 0..3 {
            append(&db, "c1", "outbound", "event", b"x").await.unwrap();
        }
        // Backdate all rows by 1000s.
        db.execute_unprepared("UPDATE chat_history SET created_at = created_at - 1000 WHERE 1;")
            .await
            .unwrap();
        let deleted = gc(&db, 60, 10_000).await; // ttl 60s; rows are 1000s old
        assert_eq!(deleted, 3);
        assert!(read_last_n(&db, "c1", 100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gc_cap_trims_oldest() {
        let (db, _d) = open_tmp().await;
        for _ in 0..5 {
            append(&db, "c1", "outbound", "event", b"x").await.unwrap();
        }
        // A TTL large enough that no row ages out (10 years), so only the cap
        // of 2 trims the 3 oldest.
        let ten_years_secs = 10 * 365 * 24 * 3600;
        let deleted = gc(&db, ten_years_secs, 2).await;
        assert_eq!(deleted, 3);
        let remaining = read_last_n(&db, "c1", 100).await.unwrap();
        assert_eq!(remaining.len(), 2, "only the 2 newest survive");
    }

    #[tokio::test]
    async fn read_after_returns_newer_both_directions_oldest_first() {
        let (db, _d) = open_tmp().await;
        // Interleave inbound/outbound; ids are assigned in append order.
        let a = append(&db, "c1", "outbound", "event", b"o1").await.unwrap();
        let b = append(&db, "c1", "inbound", "send_message", b"u1")
            .await
            .unwrap();
        let c = append(&db, "c1", "outbound", "event", b"o2").await.unwrap();
        let d = append(&db, "c1", "inbound", "send_message", b"u2")
            .await
            .unwrap();
        let _e = append(&db, "c2", "outbound", "event", b"other")
            .await
            .unwrap();
        // after=b -> rows c, d (both directions, oldest-first), c2 invisible.
        let got = read_after(&db, "c1", b, 50).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, c);
        assert_eq!(got[0].direction, "outbound");
        assert_eq!(got[1].id, d);
        assert_eq!(got[1].direction, "inbound");
        // after=a with limit 1 returns only b (the first row after a).
        let got = read_after(&db, "c1", a, 1).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, b);
        // after the last id returns nothing.
        assert!(read_after(&db, "c1", d, 50).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn read_before_returns_older_both_directions_oldest_first() {
        let (db, _d) = open_tmp().await;
        let a = append(&db, "c1", "outbound", "event", b"o1").await.unwrap();
        let b = append(&db, "c1", "inbound", "send_message", b"u1")
            .await
            .unwrap();
        let c = append(&db, "c1", "outbound", "event", b"o2").await.unwrap();
        let d = append(&db, "c1", "inbound", "send_message", b"u2")
            .await
            .unwrap();
        // before=d -> rows a, b, c oldest-first (the chunk older than d).
        let got = read_before(&db, "c1", d, 50).await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, a);
        assert_eq!(got[1].id, b);
        assert_eq!(got[2].id, c);
        // before=d limit 2 -> the 2 rows immediately older than d (b, c).
        let got = read_before(&db, "c1", d, 2).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, b);
        assert_eq!(got[1].id, c);
        // before=a (the oldest) returns nothing.
        assert!(read_before(&db, "c1", a, 50).await.unwrap().is_empty());
    }
}
