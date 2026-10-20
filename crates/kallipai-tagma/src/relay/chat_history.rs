//! The tagma's durable chat-history store: the source of truth a reconnecting
//! (or freshly-paired) device pulls via `TagmaControl::History` on each open,
//! so the user sees what it missed while the app was offline.
//!
//! Distinct from `kallipai_adk::history` (the agent's own LLM-turn log):
//! this is the *relay wire transcript* of the conversation — both directions:
//! outbound `TagmaReply::Event` frames the pump produced AND inbound
//! `TagmaRequest::SendMessage` frames the user sent, in arrival order, kept as
//! plaintext so the tagma can re-encrypt them under whatever epoch key is
//! current at pull time. Plaintext at rest is consistent with the host-trust
//! model (`history.ndjson` / `ContextStore` already store plaintext on the host;
//! E2EE protects transit, not the endpoint).
//!
//! Rows are keyed by **peer**: `user_id` is the conversation partner's
//! `ParticipantId` (relay), or `NULL` (the operator on the direct path). The
//! agent's identity is never stored (reconstructed at read via `agent_sender`),
//! so only authenticated peer ids are ever persisted.
//!
//! SQLite/sea-orm: single-writer (the pump appends) + the occasional pull
//! read, so WAL is mandatory (`journal_mode(WAL)`) with `synchronous=NORMAL`
//! to keep the per-event append off the fsync hot path. Schema is managed by
//! sea-orm-migration (see [`migration`]); new schema changes are new `m_*`
//! files appended to `Migrator`, never in-place edits to the init migration.
//!
//! ## Module layout
//!
//! The sea-orm model lives in [`entity`] and the row->wire decode in
//! [`decode`]; this module holds the store operations (open/append/read/gc).
//! `decode_row` and `HistoryRow` are re-exported here so callers keep using the
//! stable `chat_history::decode_row` path.

mod decode;
mod entity;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sea_orm::entity::prelude::*;
use sea_orm::{
    Database, DatabaseBackend, DatabaseConnection, QueryOrder, QuerySelect, Select, Statement,
    Value,
};
use sea_orm_migration::MigratorTrait;
use tracing::warn;

pub(crate) mod migration;

/// Default retention window in days (env: `KALLIPAI_TAGMA_RELAY_HISTORY_TTL_DAYS`).
/// This is the *normal* retention boundary — what a user can normally see and
/// re-pull on reconnect.
pub(crate) const DEFAULT_HISTORY_TTL_DAYS: u64 = 30;
/// Default row cap (env: `KALLIPAI_TAGMA_RELAY_HISTORY_CAP`). NOT a usage quota — only
/// a runaway backstop that bounds damage from abnormal volume (a buggy agent
/// spamming, a counter glitch, an attack). Normal usage within the TTL window
/// must never approach it; a `warn!` fires when it trims so the event is
/// observable. 100k rows is a few tens of MB in SQLite, trivial for the host
/// daemon.
pub(crate) const DEFAULT_HISTORY_CAP: u64 = 100_000;

/// A cloned handle to the chat-history store. `DatabaseConnection` is
/// internally `Arc`'d, so cloning is cheap and shares the pool.
pub(crate) type Db = DatabaseConnection;

pub(crate) use decode::{HistoryRow, decode_row};
use entity::{ActiveModel, Column, Entity, Model};

/// Open (or create) the chat-history SQLite database at `path` and apply any
/// pending migrations. The parent directory must already exist with owner-only
/// permissions (the caller — `activate_relay` — sets that up alongside
/// `credentials/`).
pub(crate) async fn open(path: &Path, sql_slow: Duration) -> Result<Db> {
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let mut opts = kallipai_db_helper::connect_options(&url, sql_slow);
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

/// Apply the per-peer partition filter to a query. `None` selects the direct
/// (operator) partition (`user_id IS NULL`); `Some(id)` selects the relay
/// partition for that peer.
fn by_peer(select: Select<Entity>, user_id: Option<&str>) -> Select<Entity> {
    match user_id {
        Some(id) => select.filter(Column::UserId.eq(id)),
        None => select.filter(Column::UserId.is_null()),
    }
}

/// The `chat_read_cursor` key for a partition: the relay peer's
/// `ParticipantId`, or the empty string for the direct (operator) partition.
/// SQLite primary keys are never `NULL`, so the direct case needs a sentinel
/// rather than the `NULL` the history rows themselves use.
pub(crate) fn peer_key(user_id: Option<&str>) -> &str {
    user_id.unwrap_or("")
}

/// The raw-SQL partition predicate, the exact counterpart of [`by_peer`]
/// (`None` = the direct/operator partition). Interpolated as a constant — the
/// peer id itself always travels as a bound parameter.
fn partition_sql(user_id: Option<&str>) -> &'static str {
    match user_id {
        Some(_) => "user_id = ?",
        None => "user_id IS NULL",
    }
}

/// The stored read cursor of a partition, `None` when the partition was never
/// read (no row yet). Callers that need a number go through [`unread`], which
/// seeds a missing row first.
pub(crate) async fn read_cursor(db: &Db, user_id: Option<&str>) -> Result<Option<i64>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT read_id FROM chat_read_cursor WHERE peer_key = ?",
            [peer_key(user_id).into()],
        ))
        .await
        .context("read chat_read_cursor")?;
    match row {
        Some(row) => Ok(Some(
            row.try_get("", "read_id")
                .context("chat_read_cursor.read_id")?,
        )),
        None => Ok(None),
    }
}

/// Advance a partition's read cursor to `cursor` (a `chat_history.id`) and
/// return the value actually stored.
///
/// Two clamps, both in SQL so neither opens a read-modify-write window:
/// monotonic (two devices PUT out of order, and the lower one must not
/// un-read what the higher one already read) and capped at the partition's
/// newest row id. The cap matters because ids come from one global
/// AUTOINCREMENT sequence: a cursor minted in another partition sits past
/// everything this partition will ever write, and an uncapped write would
/// pin the badge at zero for good. A write at or below the stored value
/// leaves both columns untouched.
///
/// The cap's premise: ids are never reissued (AUTOINCREMENT, and a deleted
/// tail keeps its ids retired), so the partition's `MAX(id)` only grows and
/// the clamp cannot regress -- `gc` trims the oldest rows (TTL by
/// `created_at`, then `ORDER BY id ASC`), never the newest.
pub(crate) async fn put_cursor(db: &Db, user_id: Option<&str>, cursor: i64) -> Result<i64> {
    let now = unix_secs();
    let partition = partition_sql(user_id);
    let sql = format!(
        "INSERT INTO chat_read_cursor (peer_key, read_id, updated_at) \
         VALUES (?, MAX(0, MIN(?, COALESCE((SELECT MAX(id) FROM chat_history WHERE {partition}), 0))), ?) \
         ON CONFLICT(peer_key) DO UPDATE SET \
            read_id = MAX(chat_read_cursor.read_id, excluded.read_id), \
            updated_at = CASE WHEN excluded.read_id > chat_read_cursor.read_id \
                THEN excluded.updated_at ELSE chat_read_cursor.updated_at END \
         RETURNING read_id"
    );
    // Bind order follows the statement text: peer key, the requested cursor,
    // the partition filter inside the cap subquery (only when it binds a
    // peer), then the timestamp.
    let mut values: Vec<Value> = Vec::with_capacity(4);
    values.push(peer_key(user_id).into());
    values.push(cursor.into());
    if user_id.is_some() {
        values.push(peer_key(user_id).into());
    }
    values.push(now.into());
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            sql,
            values,
        ))
        .await
        .context("write chat_read_cursor")?;
    let stored = row
        .context("chat_read_cursor upsert returned no row")?
        .try_get("", "read_id")
        .context("chat_read_cursor.read_id")?;
    Ok(stored)
}

/// Seed a partition's cursor at its current newest row when no row exists yet,
/// so a first-ever read reports 0 unread instead of the whole backlog. Rows
/// already on disk are pre-existing history the user could not have seen
/// through this watermark, and a fresh install must not open with a
/// four-digit badge. `NOT EXISTS` makes it first-writer-wins under concurrent
/// readers.
async fn seed_cursor(db: &Db, user_id: Option<&str>) -> Result<()> {
    let partition = partition_sql(user_id);
    let sql = format!(
        "INSERT INTO chat_read_cursor (peer_key, read_id, updated_at) \
         SELECT ?, COALESCE((SELECT MAX(id) FROM chat_history WHERE {partition}), 0), ? \
         WHERE NOT EXISTS (SELECT 1 FROM chat_read_cursor WHERE peer_key = ?)"
    );
    // Bind order follows the statement text: the SELECT list first (peer
    // key, then the partition filter inside COALESCE, then the timestamp),
    // then the NOT EXISTS probe.
    let now = unix_secs();
    let mut values: Vec<Value> = vec![peer_key(user_id).into()];
    if user_id.is_some() {
        values.push(peer_key(user_id).into());
    }
    values.push(now.into());
    values.push(peer_key(user_id).into());
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        sql,
        values,
    ))
    .await
    .context("seed chat_read_cursor")?;
    Ok(())
}

/// The partition's unread **outbound** count paired with the cursor it was
/// measured against (the snapshot the wire carries). Inbound rows are the
/// user's own messages and never count: the badge answers "what did the
/// agent say that I have not read".
/// Seeded on first call (see [`seed_cursor`]) so both halves are relative to
/// a watermark, never to the whole partition.
pub(crate) async fn unread_snapshot(db: &Db, user_id: Option<&str>) -> Result<(i64, u32)> {
    seed_cursor(db, user_id).await?;
    let cursor = read_cursor(db, user_id)
        .await?
        .context("chat_read_cursor missing after seed")?;
    let partition = partition_sql(user_id);
    let sql = format!(
        "SELECT COUNT(*) AS n FROM chat_history \
         WHERE {partition} AND direction = 'outbound' AND id > ?"
    );
    let mut values: Vec<Value> = Vec::with_capacity(2);
    if user_id.is_some() {
        values.push(peer_key(user_id).into());
    }
    values.push(cursor.into());
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            sql,
            values,
        ))
        .await
        .context("count unread chat_history")?
        .context("unread count returned no row")?;
    let n: i64 = row.try_get("", "n").context("unread count")?;
    Ok((cursor, n.clamp(0, u32::MAX as i64) as u32))
}

/// Append one outbound row and return it with the partition's unread
/// snapshot, taking the seed BEFORE the row lands.
///
/// The ordering is the point: a partition without a cursor row seeds at its
/// current newest id (see [`seed_cursor`]), so seeding after this append
/// would stamp the watermark at the row that just arrived and the first
/// message of a fresh conversation would read as zero unread. Seeding first
/// leaves that row above the watermark, which is what a new message is.
pub(crate) async fn append_outbound_snapshot(
    db: &Db,
    user_id: Option<&str>,
    username: Option<&str>,
    text: &str,
) -> Result<(i64, i64, i64, u32)> {
    seed_cursor(db, user_id).await?;
    let (id, created_at) = append(db, user_id, username, "outbound", text, None, None).await?;
    let (cursor, unread) = unread_snapshot(db, user_id).await?;
    Ok((id, created_at, cursor, unread))
}

/// Every partition this tagma holds a history row for (`None` = the direct
/// partition): the enumeration a cold-start unread announcement walks.
pub(crate) async fn peers(db: &Db) -> Result<Vec<Option<String>>> {
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT DISTINCT user_id FROM chat_history",
        ))
        .await
        .context("list chat_history partitions")?;
    rows.into_iter()
        .map(|row| {
            row.try_get::<Option<String>>("", "user_id")
                .context("chat_history.user_id")
        })
        .collect()
}

/// Append one frame and return `(row id, created_at)` — the id is the
/// `history_id` stamped onto the wire reply, the `created_at` is the same Unix
/// seconds just written so live stamping uses the DB's authoritative value with
/// zero skew. `direction` is `outbound`/`inbound`; `user_id`/`username` are the
/// peer (`None` = the operator on the direct path). No sender is stored: the
/// agent is reconstructed at read, and the peer's identity is `user_id`.
/// `quote` is the id of the row this message quotes (`None` = no quote); the
/// id is stored as-is and resolved at read, never expanded into text.
pub(crate) async fn append(
    db: &Db,
    user_id: Option<&str>,
    username: Option<&str>,
    direction: &str,
    text: &str,
    attachment: Option<&kallipai_common::protocol::agent::FileAttachment>,
    quote: Option<i64>,
) -> Result<(i64, i64)> {
    let now = unix_secs();
    let attachment = attachment
        .map(serde_json::to_string)
        .transpose()
        .context("serialize attachment")?;
    let row = ActiveModel {
        user_id: sea_orm::Set(user_id.map(|s| s.to_string())),
        username: sea_orm::Set(username.map(|s| s.to_string())),
        direction: sea_orm::Set(direction.to_string()),
        text: sea_orm::Set(text.to_string()),
        created_at: sea_orm::Set(now),
        attachment: sea_orm::Set(attachment),
        quote: sea_orm::Set(quote),
        ..Default::default()
    };
    let res = Entity::insert(row)
        .exec(db)
        .await
        .context("append chat_history")?;
    Ok((res.last_insert_id, now))
}

/// Read the most recent `n` rows for a peer partition, oldest-first (i.e. in
/// the order they should be replayed). Used by the `latest` history-pull mode.
pub(crate) async fn read_last_n(db: &Db, user_id: Option<&str>, n: u64) -> Result<Vec<HistoryRow>> {
    // Fetch newest-first (DESC), then reverse to oldest-first for replay.
    let mut rows = by_peer(Entity::find(), user_id)
        .order_by_desc(Column::Id)
        .limit(n)
        .all(db)
        .await
        .context("read chat_history last_n")?;
    rows.reverse();
    Ok(rows.into_iter().map(history_row_from_model).collect())
}

/// Read up to `limit` rows with `id > after` for a peer partition, oldest-first
/// (the incremental catch-up window). Used by the `after` history-pull mode on
/// reconnect.
pub(crate) async fn read_after(
    db: &Db,
    user_id: Option<&str>,
    after: i64,
    limit: u32,
) -> Result<Vec<HistoryRow>> {
    let rows = by_peer(Entity::find(), user_id)
        .filter(Column::Id.gt(after))
        .order_by_asc(Column::Id)
        .limit(limit as u64)
        .all(db)
        .await
        .context("read chat_history after")?;
    Ok(rows.into_iter().map(history_row_from_model).collect())
}

/// Read up to `limit` rows with `id < before` for a peer partition, oldest-first
/// (the scroll-up lazy-load window). Used by the `before` history-pull mode.
pub(crate) async fn read_before(
    db: &Db,
    user_id: Option<&str>,
    before: i64,
    limit: u32,
) -> Result<Vec<HistoryRow>> {
    // Fetch newest-first (DESC, bounded by `before`), then reverse to
    // oldest-first so the app can prepend in order.
    let mut rows = by_peer(Entity::find(), user_id)
        .filter(Column::Id.lt(before))
        .order_by_desc(Column::Id)
        .limit(limit as u64)
        .all(db)
        .await
        .context("read chat_history before")?;
    rows.reverse();
    Ok(rows.into_iter().map(history_row_from_model).collect())
}

/// Map a sea-orm `Model` into the decoded-read [`HistoryRow`] shape.
fn history_row_from_model(m: Model) -> HistoryRow {
    HistoryRow {
        id: m.id,
        user_id: m.user_id,
        username: m.username,
        direction: m.direction,
        text: m.text,
        created_at: m.created_at,
        attachment: m.attachment,
        quote: m.quote,
    }
}

/// Delete rows older than `ttl_secs` (by `created_at`), then if the row count
/// exceeds `cap`, trim the oldest down to `cap`. Returns the number deleted.
/// Best-effort: a failure is logged, not propagated, so a GC fault never takes
/// down the relay.
///
/// The cap is **per-DB** (per-tagma) and now spans all peer partitions (the
/// direct `NULL` partition plus each relay `user_id` partition). It is a
/// runaway backstop, not a per-peer quota: one chatty peer can push another's
/// oldest rows past the cap. If a future phase wants per-peer fairness, scope
/// the count + trim per `user_id` (the schema and read paths are already
/// multi-peer ready); TTL is age-based and peer-agnostic, so it stays as-is.
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
    use kallipai_archeion_common::ids::{ParticipantId, ParticipantKind};
    use kallipai_lesche_common::message::{HistoryEntry, Participant, TagmaReply};
    use tempfile::TempDir;

    async fn open_tmp() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let db = open(
            &dir.path().join("h.sqlite"),
            std::time::Duration::from_millis(100),
        )
        .await
        .unwrap();
        (db, dir)
    }

    fn agent_sender() -> Participant {
        Participant {
            id: ParticipantId::from("agent-id".to_string()),
            kind: ParticipantKind::Agent,
            handle: "Tagma".into(),
            tagma_id: None,
        }
    }

    /// The relay peer's wire sender, reconstructed at read from the stored
    /// `user_id`/`username` (the id is stored verbatim, not re-derived).
    fn peer_sender(row: &HistoryRow) -> Participant {
        Participant {
            id: ParticipantId::from(row.user_id.clone().unwrap_or_default()),
            kind: ParticipantKind::Human,
            handle: row.username.clone().unwrap_or_default(),
            tagma_id: None,
        }
    }

    /// Append with a peer partition and direction, returning the row id.
    /// `user_id = None` is the direct (operator) partition.
    async fn ap(db: &Db, user_id: Option<&str>, direction: &str, text: &str) -> i64 {
        let username = user_id.map(|_| "peer-handle");
        append(db, user_id, username, direction, text, None, None)
            .await
            .unwrap()
            .0
    }

    /// Append with an explicit quote id: the column round trip needs a real
    /// row id, which only the store can supply.
    async fn apq(
        db: &Db,
        user_id: Option<&str>,
        direction: &str,
        text: &str,
        quote: Option<i64>,
    ) -> i64 {
        let username = user_id.map(|_| "peer-handle");
        append(db, user_id, username, direction, text, None, quote)
            .await
            .unwrap()
            .0
    }

    /// The unread count alone: these tests assert the number, while
    /// production callers take the snapshot.
    async fn unread(db: &Db, user_id: Option<&str>) -> Result<u32> {
        Ok(unread_snapshot(db, user_id).await?.1)
    }

    #[tokio::test]
    async fn open_applies_migrations_idempotently() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("h.sqlite");
        {
            let _db = open(&path, std::time::Duration::from_millis(100))
                .await
                .unwrap();
        }
        let db = open(&path, std::time::Duration::from_millis(100))
            .await
            .unwrap();
        let id = ap(&db, Some("u1"), "outbound", "x").await;
        assert!(id > 0);
        assert_eq!(read_last_n(&db, Some("u1"), 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn append_returns_monotonic_ids() {
        let (db, _d) = open_tmp().await;
        let a = ap(&db, Some("u1"), "outbound", "x").await;
        let b = ap(&db, Some("u1"), "outbound", "x").await;
        let c = ap(&db, Some("u1"), "outbound", "x").await;
        assert!(a < b && b < c, "ids must be monotonic: {a},{b},{c}");
    }

    #[tokio::test]
    async fn read_last_n_returns_newest_oldest_first() {
        let (db, _d) = open_tmp().await;
        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(ap(&db, Some("u1"), "outbound", "x").await);
        }
        let got = read_last_n(&db, Some("u1"), 3).await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, ids[2]);
        assert_eq!(got[2].id, ids[4]);
        let got = read_last_n(&db, Some("u1"), 100).await.unwrap();
        assert_eq!(got.len(), 5);
        assert_eq!(got[0].id, ids[0]);
        // A different peer partition is invisible.
        assert!(read_last_n(&db, Some("u2"), 3).await.unwrap().is_empty());
        // The direct (NULL) partition is invisible to a relay read.
        ap(&db, None, "outbound", "operator").await;
        assert!(read_last_n(&db, Some("u1"), 100).await.unwrap().len() == 5);
        assert_eq!(read_last_n(&db, None, 100).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn direct_and_relay_partitions_are_separate() {
        let (db, _d) = open_tmp().await;
        ap(&db, None, "inbound", "operator-msg").await;
        ap(&db, Some("owner"), "inbound", "owner-msg").await;
        // Each partition sees only its own rows.
        let direct = read_last_n(&db, None, 10).await.unwrap();
        let relay = read_last_n(&db, Some("owner"), 10).await.unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].text, "operator-msg");
        assert_eq!(relay.len(), 1);
        assert_eq!(relay[0].text, "owner-msg");
    }

    #[tokio::test]
    async fn gc_respects_ttl_and_cap() {
        let (db, _d) = open_tmp().await;
        for _ in 0..3 {
            ap(&db, Some("u1"), "outbound", "x").await;
        }
        db.execute_unprepared("UPDATE chat_history SET created_at = created_at - 1000 WHERE 1;")
            .await
            .unwrap();
        let deleted = gc(&db, 60, 10_000).await;
        assert_eq!(deleted, 3);
        assert!(read_last_n(&db, Some("u1"), 100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gc_cap_trims_oldest() {
        let (db, _d) = open_tmp().await;
        for _ in 0..5 {
            ap(&db, Some("u1"), "outbound", "x").await;
        }
        let ten_years_secs = 10 * 365 * 24 * 3600;
        let deleted = gc(&db, ten_years_secs, 2).await;
        assert_eq!(deleted, 3);
        let remaining = read_last_n(&db, Some("u1"), 100).await.unwrap();
        assert_eq!(remaining.len(), 2, "only the 2 newest survive");
    }

    #[tokio::test]
    async fn read_after_returns_newer_both_directions_oldest_first() {
        let (db, _d) = open_tmp().await;
        let a = ap(&db, Some("u1"), "outbound", "o1").await;
        let b = ap(&db, Some("u1"), "inbound", "u1").await;
        let c = ap(&db, Some("u1"), "outbound", "o2").await;
        let d = ap(&db, Some("u1"), "inbound", "u2").await;
        let _e = ap(&db, Some("u2"), "outbound", "other").await;
        let got = read_after(&db, Some("u1"), b, 50).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, c);
        assert_eq!(got[0].direction, "outbound");
        assert_eq!(got[1].id, d);
        assert_eq!(got[1].direction, "inbound");
        let got = read_after(&db, Some("u1"), a, 1).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, b);
        assert!(read_after(&db, Some("u1"), d, 50).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn read_before_returns_older_both_directions_oldest_first() {
        let (db, _d) = open_tmp().await;
        let a = ap(&db, Some("u1"), "outbound", "o1").await;
        let b = ap(&db, Some("u1"), "inbound", "u1").await;
        let c = ap(&db, Some("u1"), "outbound", "o2").await;
        let d = ap(&db, Some("u1"), "inbound", "u2").await;
        let got = read_before(&db, Some("u1"), d, 50).await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, a);
        assert_eq!(got[1].id, b);
        assert_eq!(got[2].id, c);
        let got = read_before(&db, Some("u1"), d, 2).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, b);
        assert_eq!(got[1].id, c);
        assert!(
            read_before(&db, Some("u1"), a, 50)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn read_surfaces_created_at() {
        let (db, _d) = open_tmp().await;
        let before = unix_secs();
        ap(&db, Some("u1"), "outbound", "x").await;
        let after = unix_secs();
        let got = read_last_n(&db, Some("u1"), 1).await.unwrap();
        let created_at = got[0].created_at;
        assert!(
            before <= created_at && created_at <= after,
            "created_at {created_at} should be within [{before},{after}]"
        );
    }

    #[tokio::test]
    async fn decode_round_trip_uses_resolved_sender() {
        // The store holds no sender; the caller resolves one per row (outbound
        // => agent, inbound => peer from user_id/username) and decode_row maps
        // direction + text onto the wire reply shape.
        let (db, _d) = open_tmp().await;
        append(
            &db,
            Some("u1"),
            Some("Alice"),
            "outbound",
            "hello",
            None,
            None,
        )
        .await
        .unwrap();
        append(&db, Some("u1"), Some("Alice"), "inbound", "hi", None, None)
            .await
            .unwrap();

        let rows = read_last_n(&db, Some("u1"), 10).await.unwrap();
        let entries: Vec<HistoryEntry> = rows
            .into_iter()
            .filter_map(|r| {
                let sender = if r.direction == "outbound" {
                    agent_sender()
                } else {
                    peer_sender(&r)
                };
                decode_row(r, sender)
            })
            .collect();
        assert_eq!(entries.len(), 2);

        match &entries[0] {
            HistoryEntry {
                sender,
                reply: TagmaReply::Event { event, .. },
            } => {
                assert_eq!(sender.kind, ParticipantKind::Agent);
                assert_eq!(sender.id, ParticipantId::from("agent-id".to_string()));
                assert!(
                    matches!(event, kallipai_common::protocol::AuthoredEvent::AssistantContent { content } if content == "hello")
                );
            }
            other => panic!("expected agent Event, got {other:?}"),
        }
        match &entries[1] {
            HistoryEntry {
                sender,
                reply: TagmaReply::UserMessage { text, .. },
            } => {
                assert_eq!(sender.kind, ParticipantKind::Human);
                assert_eq!(sender.id, ParticipantId::from("u1".to_string()));
                assert_eq!(sender.handle, "Alice");
                assert_eq!(text, "hi");
            }
            other => panic!("expected user UserMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_cursor_seeds_at_newest_and_reports_zero() {
        let (db, _d) = open_tmp().await;
        // Pre-existing rows: an install that already has history must not open
        // with the whole partition counted as unread.
        ap(&db, Some("u1"), "inbound", "old").await;
        let newest = ap(&db, Some("u1"), "outbound", "old-reply").await;
        assert_eq!(read_cursor(&db, Some("u1")).await.unwrap(), None);
        assert_eq!(unread(&db, Some("u1")).await.unwrap(), 0);
        assert_eq!(read_cursor(&db, Some("u1")).await.unwrap(), Some(newest));
    }

    #[tokio::test]
    async fn unread_counts_only_outbound_above_the_cursor() {
        let (db, _d) = open_tmp().await;
        assert_eq!(unread(&db, Some("u1")).await.unwrap(), 0); // seeds at 0
        let first = ap(&db, Some("u1"), "outbound", "a").await;
        ap(&db, Some("u1"), "inbound", "mine").await; // the user's own: never unread
        let newest = ap(&db, Some("u1"), "outbound", "b").await;
        assert_eq!(unread(&db, Some("u1")).await.unwrap(), 2);
        assert_eq!(put_cursor(&db, Some("u1"), first).await.unwrap(), first);
        assert_eq!(unread(&db, Some("u1")).await.unwrap(), 1);
        put_cursor(&db, Some("u1"), newest).await.unwrap();
        assert_eq!(unread(&db, Some("u1")).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn put_cursor_is_monotonic() {
        let (db, _d) = open_tmp().await;
        for i in 0..12 {
            ap(&db, Some("u1"), "outbound", &format!("m{i}")).await;
        }
        assert_eq!(put_cursor(&db, Some("u1"), 10).await.unwrap(), 10);
        // An out-of-order write from a second device must not un-read.
        assert_eq!(put_cursor(&db, Some("u1"), 4).await.unwrap(), 10);
        assert_eq!(read_cursor(&db, Some("u1")).await.unwrap(), Some(10));
        assert_eq!(put_cursor(&db, Some("u1"), 11).await.unwrap(), 11);
        assert_eq!(read_cursor(&db, Some("u1")).await.unwrap(), Some(11));
    }

    #[tokio::test]
    async fn peers_cover_every_partition_once() {
        let (db, _d) = open_tmp().await;
        assert!(peers(&db).await.unwrap().is_empty());
        ap(&db, Some("u1"), "outbound", "a").await;
        ap(&db, Some("u1"), "inbound", "b").await;
        ap(&db, None, "outbound", "direct").await;
        let mut seen = peers(&db).await.unwrap();
        seen.sort();
        assert_eq!(seen, vec![None, Some("u1".to_string())]);
    }

    #[tokio::test]
    async fn read_cursors_of_both_partitions_are_separate() {
        let (db, _d) = open_tmp().await;
        let direct = ap(&db, None, "outbound", "direct-reply").await;
        let relay = ap(&db, Some("u1"), "outbound", "relay-reply").await;
        assert_eq!(unread(&db, None).await.unwrap(), 0);
        assert_eq!(unread(&db, Some("u1")).await.unwrap(), 0);
        // Direct keys on the empty string, relay on the peer id: advancing one
        // must not move the other.
        assert_eq!(put_cursor(&db, None, direct).await.unwrap(), direct);
        assert_eq!(read_cursor(&db, None).await.unwrap(), Some(direct));
        assert_eq!(read_cursor(&db, Some("u1")).await.unwrap(), Some(relay));
    }

    #[tokio::test]
    async fn read_cursor_survives_a_migration_rerun() {
        let (db, _d) = open_tmp().await;
        // The cursor has to be a row this partition actually holds: the store
        // caps a write at the partition's newest id.
        let newest = ap(&db, Some("u1"), "outbound", "a").await;
        assert_eq!(put_cursor(&db, Some("u1"), newest).await.unwrap(), newest);
        migration::Migrator::up(&db, None).await.unwrap();
        assert_eq!(read_cursor(&db, Some("u1")).await.unwrap(), Some(newest));
    }

    #[tokio::test]
    async fn append_of_a_first_message_reports_it_unread() {
        let (db, _d) = open_tmp().await;
        // A fresh partition: the seed must land before the row, so the first
        // outbound message is one unread instead of zero.
        let (_, _, cursor, unread) = append_outbound_snapshot(&db, Some("u1"), Some("h"), "hi")
            .await
            .unwrap();
        assert_eq!(cursor, 0);
        assert_eq!(unread, 1);
    }

    #[tokio::test]
    async fn a_cursor_past_the_partition_tail_does_not_swallow_later_rows() {
        let (db, _d) = open_tmp().await;
        ap(&db, Some("u1"), "inbound", "mine").await;
        let tail = ap(&db, Some("u1"), "outbound", "a").await;
        // Ids are one global sequence, so a cursor minted in another
        // partition sits past every row this one will write. Capped, the
        // stored value stays at this partition's tail and the next row
        // still counts.
        assert_eq!(
            put_cursor(&db, Some("u1"), tail + 1000).await.unwrap(),
            tail
        );
        ap(&db, Some("u1"), "outbound", "b").await;
        assert_eq!(unread_snapshot(&db, Some("u1")).await.unwrap().1, 1);
    }

    #[tokio::test]
    async fn seed_lands_at_the_newest_preexisting_row() {
        let (db, _d) = open_tmp().await;
        // Two beats on one partition. Existing history seeds to zero unread:
        // rows already on disk were never readable through a watermark this
        // install wrote (the upgrade semantics). A watermark one row back
        // then counts exactly the row above it, which fails if the seed ever
        // moves behind the count.
        ap(&db, Some("u1"), "outbound", "old-a").await;
        let newest = ap(&db, Some("u1"), "outbound", "old-b").await;
        assert_eq!(unread_snapshot(&db, Some("u1")).await.unwrap().1, 0);
        assert_eq!(read_cursor(&db, Some("u1")).await.unwrap(), Some(newest));
        // Rewind by hand: put_cursor is monotonic by design, so only a direct
        // write can move the watermark back under the row.
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE chat_read_cursor SET read_id = ? WHERE peer_key = ?",
            [(newest - 1).into(), "u1".into()],
        ))
        .await
        .unwrap();
        assert_eq!(unread_snapshot(&db, Some("u1")).await.unwrap().1, 1);
    }

    #[tokio::test]
    async fn quote_survives_every_read_window_and_decodes() {
        let (db, _d) = open_tmp().await;
        // The agent's line is what gets quoted; the peer's answering line
        // carries the id (the wire UserMessage is the inbound side).
        let original = ap(&db, Some("u1"), "outbound", "the first one").await;
        let reply = apq(&db, Some("u1"), "inbound", "answering it", Some(original)).await;

        let last = read_last_n(&db, Some("u1"), 10).await.unwrap();
        assert_eq!(last.len(), 2);
        assert_eq!(last[1].id, reply);
        assert_eq!(last[1].quote, Some(original), "last_n carries the id");

        let older = read_before(&db, Some("u1"), reply + 1, 10).await.unwrap();
        let quotes: Vec<Option<i64>> = older.iter().map(|r| r.quote).collect();
        assert_eq!(quotes, vec![None, Some(original)]);

        let mut newer = read_after(&db, Some("u1"), original, 10).await.unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].quote, Some(original), "after carries the id");

        // The wire shape keeps it too: the echo is a UserMessage with the id.
        let row = newer.pop().unwrap();
        let sender = peer_sender(&row);
        match decode_row(row, sender) {
            Some(HistoryEntry {
                reply: TagmaReply::UserMessage { quote, .. },
                ..
            }) => assert_eq!(quote, Some(original)),
            other => panic!("expected UserMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rows_inserted_without_the_quote_column_replay_without_one() {
        let (db, _d) = open_tmp().await;
        // A row inserted without naming the column -- what a writer from
        // before the column (or a raw insert) leaves behind: the value is NULL.
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "INSERT INTO chat_history (user_id, username, direction, text, created_at) \
             VALUES ('u1', 'peer-handle', 'inbound', 'legacy row', 1700000000)",
        ))
        .await
        .unwrap();
        // Re-applying the migrator must neither re-add the column nor drop the
        // row it backfilled with NULL.
        migration::Migrator::up(&db, None).await.unwrap();

        let rows = read_last_n(&db, Some("u1"), 10).await.unwrap();
        assert_eq!(rows.len(), 1, "the legacy row is still there");
        assert_eq!(rows[0].text, "legacy row");
        assert_eq!(rows[0].quote, None, "a pre-column row replays quote-less");
    }

    /// A connection with no migrations applied: the upgrade path needs a
    /// store that already exists at the previous schema.
    async fn open_raw(dir: &TempDir) -> Db {
        let path = dir.path().join("raw.sqlite");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        Database::connect(&url).await.unwrap()
    }

    #[tokio::test]
    async fn an_install_at_the_previous_schema_upgrades_with_its_rows_intact() {
        let dir = TempDir::new().unwrap();
        let db = open_raw(&dir).await;
        // Stop one migration short: a real pre-quote install.
        migration::Migrator::up(&db, Some(5)).await.unwrap();
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "INSERT INTO chat_history (user_id, username, direction, text, created_at) \
             VALUES ('peer-1', 'peer-handle', 'inbound', 'before the upgrade', 1700000000)",
        ))
        .await
        .unwrap();
        // The upgrade an install performs on boot.
        migration::Migrator::up(&db, None).await.unwrap();
        let rows = read_last_n(&db, Some("peer-1"), 10).await.unwrap();
        assert_eq!(rows.len(), 1, "the upgraded store keeps the row");
        assert_eq!(rows[0].text, "before the upgrade");
        assert_eq!(rows[0].quote, None, "a row from before the column has none");
    }
}
