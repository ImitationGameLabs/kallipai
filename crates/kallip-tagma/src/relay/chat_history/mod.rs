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

pub(crate) use decode::{HistoryRow, decode_row};
use entity::{ActiveModel, Column, Entity, Model};
use kallip_lesche_common::message::Participant;

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

/// Append one typed frame and return `(row id, created_at)` — the id is the
/// `history_id` stamped onto the wire reply, the `created_at` is the same Unix
/// seconds just written so live stamping uses the DB's authoritative value with
/// zero skew. `direction` is `outbound`/`inbound`; `kind` is the wire
/// discriminant (`event`/`send_message`); `sender` + `text` are the typed
/// payload (no opaque blob).
pub(crate) async fn append(
    db: &Db,
    conversation_id: &str,
    direction: &str,
    kind: &str,
    sender: &Participant,
    text: &str,
) -> Result<(i64, i64)> {
    let (sender_kind, sender_id, sender_handle) = sender_fields(sender);
    let now = unix_secs();
    let row = ActiveModel {
        conversation_id: sea_orm::Set(conversation_id.to_string()),
        direction: sea_orm::Set(direction.to_string()),
        kind: sea_orm::Set(kind.to_string()),
        sender_kind: sea_orm::Set(sender_kind.to_string()),
        sender_id: sea_orm::Set(sender_id.to_string()),
        sender_handle: sea_orm::Set(sender_handle.to_string()),
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

/// Decompose a [`Participant`] into its typed column triple. The stored id is
/// the room-layer `participant_id` (the opaque derived identity); the kind label
/// goes through `ParticipantKind::as_str`.
fn sender_fields(sender: &Participant) -> (&str, &str, &str) {
    (sender.kind.as_str(), sender.id.as_ref(), &sender.handle)
}

/// Append an outbound `event` row and stamp its row id + `created_at` onto
/// `reply` in place. Shared by the relay emit path and the direct serving path.
/// The sender is the agent; `text` is the assistant content. A storage failure
/// degrades gracefully: the row is not recorded, the id stays 0, and the caller
/// still delivers the frame live (no dedup across reconnect for that one frame).
pub(crate) async fn stamp_reply(
    db: &Db,
    conversation_id: &str,
    sender: &Participant,
    text: &str,
    reply: &mut kallip_lesche_common::message::TagmaReply,
) {
    if let Ok((id, created_at)) =
        append(db, conversation_id, "outbound", "event", sender, text).await
    {
        reply.set_history_id(id);
        reply.set_created_at(created_at);
    }
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
    Ok(rows.into_iter().map(history_row_from_model).collect())
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
    Ok(rows.into_iter().map(history_row_from_model).collect())
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
    Ok(rows.into_iter().map(history_row_from_model).collect())
}

/// Map a sea-orm `Model` into the decoded-read [`HistoryRow`] shape.
fn history_row_from_model(m: Model) -> HistoryRow {
    HistoryRow {
        id: m.id,
        direction: m.direction,
        sender_kind: m.sender_kind,
        sender_id: m.sender_id,
        sender_handle: m.sender_handle,
        text: m.text,
        created_at: m.created_at,
    }
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
    use kallip_agora_common::ids::{ParticipantId, ParticipantKind, TagmaId, UserId};
    use kallip_common::protocol::AuthoredEvent;
    use kallip_lesche_common::message::{HistoryEntry, TagmaReply};
    use tempfile::TempDir;

    async fn open_tmp() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let db = open(&dir.path().join("h.sqlite")).await.unwrap();
        (db, dir)
    }

    fn agent_sender() -> Participant {
        Participant {
            id: ParticipantId::for_tagma(&TagmaId::from("t1".to_string())),
            kind: ParticipantKind::Agent,
            handle: "Tagma".into(),
            tagma_id: None,
        }
    }

    fn user_sender() -> Participant {
        Participant {
            id: ParticipantId::for_user(&UserId::from("u1".to_string())),
            kind: ParticipantKind::Human,
            handle: "Alice".into(),
            tagma_id: None,
        }
    }

    /// Append with the test's default sender for the given direction and return
    /// the row id.
    async fn ap(db: &Db, conv: &str, direction: &str, text: &str) -> i64 {
        let (kind, sender) = match direction {
            "outbound" => ("event", agent_sender()),
            "inbound" => ("send_message", user_sender()),
            other => panic!("unknown test direction: {other}"),
        };
        append(db, conv, direction, kind, &sender, text)
            .await
            .unwrap()
            .0
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
        let id = ap(&db, "c1", "outbound", "x").await;
        assert!(id > 0);
        assert_eq!(read_last_n(&db, "c1", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn append_returns_monotonic_ids() {
        let (db, _d) = open_tmp().await;
        let a = ap(&db, "c1", "outbound", "x").await;
        let b = ap(&db, "c1", "outbound", "x").await;
        let c = ap(&db, "c1", "outbound", "x").await;
        assert!(a < b && b < c, "ids must be monotonic: {a},{b},{c}");
    }

    #[tokio::test]
    async fn read_last_n_returns_newest_oldest_first() {
        let (db, _d) = open_tmp().await;
        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(ap(&db, "c1", "outbound", "x").await);
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
            ap(&db, "c1", "outbound", "x").await;
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
            ap(&db, "c1", "outbound", "x").await;
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
        let a = ap(&db, "c1", "outbound", "o1").await;
        let b = ap(&db, "c1", "inbound", "u1").await;
        let c = ap(&db, "c1", "outbound", "o2").await;
        let d = ap(&db, "c1", "inbound", "u2").await;
        let _e = ap(&db, "c2", "outbound", "other").await;
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
        let a = ap(&db, "c1", "outbound", "o1").await;
        let b = ap(&db, "c1", "inbound", "u1").await;
        let c = ap(&db, "c1", "outbound", "o2").await;
        let d = ap(&db, "c1", "inbound", "u2").await;
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

    #[tokio::test]
    async fn read_surfaces_created_at() {
        // HistoryRow.created_at is what the relay stamps onto replayed frames,
        // so a read must surface the row's append time (not drop it).
        let (db, _d) = open_tmp().await;
        let before = unix_secs();
        ap(&db, "c1", "outbound", "x").await;
        let after = unix_secs();
        let got = read_last_n(&db, "c1", 1).await.unwrap();
        let created_at = got[0].created_at;
        assert!(
            before <= created_at && created_at <= after,
            "created_at {created_at} should be within [{before},{after}]"
        );
    }

    #[tokio::test]
    async fn typed_round_trip_decodes_sender_and_content() {
        // The typed columns round-trip: an outbound agent row decodes back to an
        // Event with the agent sender + the original content; an inbound user
        // row decodes to a UserMessage with the user sender + text.
        let (db, _d) = open_tmp().await;
        append(&db, "c1", "outbound", "event", &agent_sender(), "hello")
            .await
            .unwrap();
        append(&db, "c1", "inbound", "send_message", &user_sender(), "hi")
            .await
            .unwrap();

        let rows = read_last_n(&db, "c1", 10).await.unwrap();
        let entries: Vec<HistoryEntry> = rows.into_iter().filter_map(decode_row).collect();
        assert_eq!(entries.len(), 2);

        match &entries[0] {
            HistoryEntry {
                sender,
                reply: TagmaReply::Event { event, .. },
            } => {
                assert_eq!(sender.kind, ParticipantKind::Agent);
                assert_eq!(
                    sender.id,
                    ParticipantId::for_tagma(&TagmaId::from("t1".to_string()))
                );
                assert_eq!(sender.handle, "Tagma");
                assert!(
                    matches!(event, AuthoredEvent::AssistantContent { content } if content == "hello")
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
                assert_eq!(
                    sender.id,
                    ParticipantId::for_user(&UserId::from("u1".to_string()))
                );
                assert_eq!(sender.handle, "Alice");
                assert_eq!(text, "hi");
            }
            other => panic!("expected user UserMessage, got {other:?}"),
        }
    }
}
