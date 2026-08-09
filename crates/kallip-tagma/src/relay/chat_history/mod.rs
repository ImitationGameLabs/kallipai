//! The tagma's durable chat-history store: the source of truth a reconnecting
//! (or freshly-paired) device pulls via `TagmaControl::History` on each open,
//! so the user sees what it missed while the app was offline.
//!
//! Distinct from `kallip_runtime::history` (the agent's own LLM-turn log):
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

use anyhow::{Context, Result};
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectOptions, Database, DatabaseConnection, QueryOrder, QuerySelect, Select};
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

pub(crate) use decode::{HistoryRow, decode_row};
use entity::{ActiveModel, Column, Entity, Model};

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

/// Apply the per-peer partition filter to a query. `None` selects the direct
/// (operator) partition (`user_id IS NULL`); `Some(id)` selects the relay
/// partition for that peer.
fn by_peer(select: Select<Entity>, user_id: Option<&str>) -> Select<Entity> {
    match user_id {
        Some(id) => select.filter(Column::UserId.eq(id)),
        None => select.filter(Column::UserId.is_null()),
    }
}

/// Append one frame and return `(row id, created_at)` — the id is the
/// `history_id` stamped onto the wire reply, the `created_at` is the same Unix
/// seconds just written so live stamping uses the DB's authoritative value with
/// zero skew. `direction` is `outbound`/`inbound`; `user_id`/`username` are the
/// peer (`None` = the operator on the direct path). No sender is stored: the
/// agent is reconstructed at read, and the peer's identity is `user_id`.
pub(crate) async fn append(
    db: &Db,
    user_id: Option<&str>,
    username: Option<&str>,
    direction: &str,
    text: &str,
) -> Result<(i64, i64)> {
    let now = unix_secs();
    let row = ActiveModel {
        user_id: sea_orm::Set(user_id.map(|s| s.to_string())),
        username: sea_orm::Set(username.map(|s| s.to_string())),
        direction: sea_orm::Set(direction.to_string()),
        text: sea_orm::Set(text.to_string()),
        created_at: sea_orm::Set(now),
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
    use kallip_agora_common::ids::{ParticipantId, ParticipantKind};
    use kallip_lesche_common::message::{HistoryEntry, Participant, TagmaReply};
    use tempfile::TempDir;

    async fn open_tmp() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let db = open(&dir.path().join("h.sqlite")).await.unwrap();
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
        append(db, user_id, username, direction, text)
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn open_applies_migrations_idempotently() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("h.sqlite");
        {
            let _db = open(&path).await.unwrap();
        }
        let db = open(&path).await.unwrap();
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
        append(&db, Some("u1"), Some("Alice"), "outbound", "hello")
            .await
            .unwrap();
        append(&db, Some("u1"), Some("Alice"), "inbound", "hi")
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
                    matches!(event, kallip_common::protocol::AuthoredEvent::AssistantContent { content } if content == "hello")
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
}
