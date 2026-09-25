//! The direct-session durable store: append-only per-session message log with
//! a race-free per-session sequence, and clamp-on-write read cursors. The
//! mechanics mirror [`crate::db::store`] (the room store) statement-for-
//! statement; the schema differs -- fixed two-member sessions (the sender is
//! the tagma id itself, no kind column), no membership epoch, and the
//! create-or-get session upsert the room domain has no equivalent of.

use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseBackend, DbErr,
    EntityTrait, QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
    query::Condition,
};
use time::OffsetDateTime;

use crate::db::Db;

use crate::db::entity::{direct_messages, direct_sessions};

/// One direct-session row, as read back for routing/fan-out/listing.
#[derive(Debug, Clone)]
pub struct DirectSessionRow {
    pub id: String,
    /// Canonical byte-ordered member pair (`member_a` <= `member_b`).
    pub member_a: String,
    pub member_b: String,
    pub created_at: OffsetDateTime,
}

impl DirectSessionRow {
    /// Is `tagma_id` one of the session's two members?
    pub fn has_member(&self, tagma_id: &str) -> bool {
        self.member_a == tagma_id || self.member_b == tagma_id
    }

    /// The OTHER member (the peer) of `tagma_id`. Membership must be gated by
    /// the caller (`has_member`) before this is meaningful.
    pub fn peer_of(&self, tagma_id: &str) -> &str {
        if self.member_a == tagma_id {
            &self.member_b
        } else {
            &self.member_a
        }
    }
}

/// One stored direct message, as read back for a history pull. Carries the
/// sender's STABLE identity (its tagma id); the display handle is resolved by
/// the caller from the registry (it is a function of this identity, not a
/// fact to persist).
#[derive(Debug, Clone)]
pub struct StoredDirectMessage {
    pub seq: i64,
    pub sender: String,
    pub payload: Vec<u8>,
    pub created_at: OffsetDateTime,
}

/// Fetch the session row by id (`None` = unknown session).
pub async fn find_session(db: &Db, id: &str) -> Result<Option<DirectSessionRow>, DbErr> {
    Ok(direct_sessions::Entity::find_by_id(id)
        .one(db)
        .await?
        .map(|r| DirectSessionRow {
            id: r.id,
            member_a: r.member_a,
            member_b: r.member_b,
            created_at: r.created_at,
        }))
}

/// Create-or-get the session `(id, a, b)` and return the stored row. The
/// caller derives the canonical order and the id (`kallipai_lesche_common::
/// direct`); this only guarantees one row per pair. The create-or-ignore is
/// a single statement: even a TRUE double-initiation race is idempotent (the
/// loser's insert is a no-op under the winner's row lock; the re-select in
/// the same transaction returns the stored row), so both callers observe the
/// one session and neither errors.
pub async fn ensure_session(
    db: &Db,
    id: &str,
    member_a: &str,
    member_b: &str,
) -> Result<DirectSessionRow, DbErr> {
    let result = db
        .transaction::<_, DirectSessionRow, DbErr>(|txn| {
            let id = id.to_string();
            let a = member_a.to_string();
            let b = member_b.to_string();
            Box::pin(async move {
                // One create-or-ignore statement (the next_seq upsert idiom):
                // a TRUE double-initiation race serializes on the PK insert,
                // the loser lands as a no-op, and the re-select in the same
                // transaction returns the winner's row to both callers --
                // idempotent end to end, no unique-violation error path.
                txn.execute(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "INSERT INTO direct_sessions (id, member_a, member_b, created_at) VALUES ($1, $2, $3, NOW()) ON CONFLICT (id) DO NOTHING",
                    [id.clone().into(), a.into(), b.into()],
                ))
                .await?;
                let row = direct_sessions::Entity::find_by_id(id)
                    .one(txn)
                    .await?
                    .ok_or_else(|| {
                        sea_orm::DbErr::Custom(
                            "direct session row missing after create-or-ignore".into(),
                        )
                    })?;
                Ok(DirectSessionRow {
                    id: row.id,
                    member_a: row.member_a,
                    member_b: row.member_b,
                    created_at: row.created_at,
                })
            })
        })
        .await;
    result.map_err(|e| match e {
        sea_orm::TransactionError::Connection(e) | sea_orm::TransactionError::Transaction(e) => e,
    })
}

/// List every session where `tagma_id` is a member (either column), creation
/// order. The direct-session poll/list surface.
pub async fn sessions_for_member(db: &Db, tagma_id: &str) -> Result<Vec<DirectSessionRow>, DbErr> {
    let rows = direct_sessions::Entity::find()
        .filter(
            Condition::any()
                .add(direct_sessions::Column::MemberA.eq(tagma_id))
                .add(direct_sessions::Column::MemberB.eq(tagma_id)),
        )
        .order_by_asc(direct_sessions::Column::CreatedAt)
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| DirectSessionRow {
            id: r.id,
            member_a: r.member_a,
            member_b: r.member_b,
            created_at: r.created_at,
        })
        .collect())
}

/// Atomically advance the per-session sequence and return the next `seq`.
/// The `INSERT ... ON CONFLICT DO UPDATE ... RETURNING` is a single statement
/// that seeds the row on first append and increments it thereafter, all under
/// the row lock the upsert acquires -- concurrent appends to the same session
/// serialize with no PK-violation loser.
async fn next_seq(txn: &impl ConnectionTrait, session: &str) -> Result<i64, DbErr> {
    let row = txn
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO direct_message_seq (session_id, next_seq) VALUES ($1, 1) \
             ON CONFLICT (session_id) DO UPDATE SET \
             next_seq = direct_message_seq.next_seq + 1 \
             RETURNING next_seq",
            [session.into()],
        ))
        .await?
        .ok_or_else(|| {
            sea_orm::DbErr::Custom("direct_message_seq upsert returned no row".into())
        })?;
    let seq: i64 = row.try_get("", "next_seq")?;
    Ok(seq)
}

/// Append a direct-message payload row to the session's history (the payload
/// is plaintext `DirectMessage` JSON stored opaquely). Returns the assigned
/// `seq`. Only the sender's tagma id is persisted; the display handle is
/// derived at read time. Sequence advance + row insert share one transaction,
/// so a failed insert rolls the sequence back (no gaps in the log).
pub async fn append(db: &Db, session_id: &str, sender: &str, payload: &[u8]) -> Result<i64, DbErr> {
    let result = db
        .transaction::<_, i64, DbErr>(|txn| {
            let session = session_id.to_string();
            let sender = sender.to_string();
            let payload = payload.to_vec();
            Box::pin(async move {
                let seq = next_seq(txn, &session).await?;
                direct_messages::ActiveModel {
                    session_id: Set(session),
                    seq: Set(seq),
                    sender: Set(sender),
                    payload: Set(payload),
                    created_at: Set(OffsetDateTime::now_utc()),
                }
                .insert(txn)
                .await?;
                Ok(seq)
            })
        })
        .await;
    let seq = result.map_err(|e| match e {
        sea_orm::TransactionError::Connection(e) | sea_orm::TransactionError::Transaction(e) => e,
    })?;
    Ok(seq)
}

/// Read the session's history with `seq > after_seq`, ascending, up to
/// `limit`. `after_seq = 0` reads from the start.
pub async fn read_since(
    db: &Db,
    session_id: &str,
    after_seq: i64,
    limit: u64,
) -> Result<Vec<StoredDirectMessage>, DbErr> {
    let rows = direct_messages::Entity::find()
        .filter(direct_messages::Column::SessionId.eq(session_id))
        .filter(direct_messages::Column::Seq.gt(after_seq))
        .order_by_asc(direct_messages::Column::Seq)
        .limit(limit)
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| StoredDirectMessage {
            seq: r.seq,
            sender: r.sender,
            payload: r.payload,
            created_at: r.created_at,
        })
        .collect())
}

/// Clamp-advance a member's read cursor in a session. The
/// `INSERT ... ON CONFLICT DO UPDATE ... GREATEST` is a single statement: the
/// row lock the upsert takes serializes concurrent writers, and `GREATEST`
/// makes the watermark monotonic -- a stale write can never move it
/// backwards. Returns the watermark as stored (the clamped value).
pub async fn set_read_cursor(
    db: &Db,
    session_id: &str,
    member: &str,
    last_read_seq: i64,
) -> Result<i64, DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO direct_read_cursors (session_id, member, last_read_seq, updated_at) \
             VALUES ($1, $2, $3, NOW()) \
             ON CONFLICT (session_id, member) DO UPDATE SET \
             last_read_seq = GREATEST(direct_read_cursors.last_read_seq, EXCLUDED.last_read_seq), \
             updated_at = NOW() \
             RETURNING last_read_seq",
            [session_id.into(), member.into(), last_read_seq.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Custom("direct_read_cursors upsert returned no row".into()))?;
    row.try_get("", "last_read_seq")
}

#[cfg(test)]
mod tests {
    //! Create-or-get + append + cursor round-trips against ephemeral Postgres:
    //! the per-session sequence is race-free under concurrent appends, and a
    //! deleted session cascades to its messages, seq row, and cursors.

    use super::*;

    async fn fresh_db() -> Db {
        crate::test_support::provision_test_db().await
    }

    fn pair(seed: u8) -> (String, String) {
        (format!("tagma-a-{seed}"), format!("tagma-b-{seed}"))
    }

    async fn seed_session(db: &Db, id: &str, a: &str, b: &str) -> DirectSessionRow {
        ensure_session(db, id, a, b).await.expect("seed session")
    }

    #[tokio::test]
    async fn ensure_session_is_idempotent_and_order_canonical() {
        let db = fresh_db().await;
        let (a, b) = pair(1);
        let first = seed_session(&db, "s-1", &a, &b).await;
        // Same pair, either order: the stored row, not a new one.
        let again = ensure_session(&db, "s-1", &a, &b).await.unwrap();
        let flipped = ensure_session(&db, "s-1", &b, &a).await.unwrap();
        assert_eq!(again.created_at, first.created_at);
        assert_eq!(flipped.created_at, first.created_at);
        assert_eq!(flipped.member_a, a);
        assert_eq!(flipped.member_b, b);
        assert!(first.has_member(&a) && first.has_member(&b));
        assert_eq!(first.peer_of(&a), b);
        assert_eq!(first.peer_of(&b), a);
    }

    #[tokio::test]
    async fn concurrent_double_creation_lands_one_row_for_both() {
        let db = fresh_db().await;
        let (a, b) = pair(1);
        // True concurrent creation, each side initiating with its own pair
        // order: the PK conflict must resolve into a no-op + re-select, not
        // a unique-violation error, and both callers get the SAME row.
        let mut handles = Vec::new();
        for (x, y) in [(a.clone(), b.clone()), (b.clone(), a.clone())] {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                ensure_session(&db, "s-1", &x, &y).await
            }));
        }
        let one = handles
            .swap_remove(0)
            .await
            .unwrap()
            .expect("initiator create");
        let two = handles.swap_remove(0).await.unwrap().expect("peer create");
        assert_eq!(one.id, "s-1");
        assert_eq!(two.id, "s-1");
        assert_eq!(one.created_at, two.created_at);
        assert_eq!(one.member_a, two.member_a);
        assert_eq!(one.member_b, two.member_b);
    }

    #[tokio::test]
    async fn sessions_for_member_scopes_to_member() {
        let db = fresh_db().await;
        let (a1, b1) = pair(1);
        let (a2, b2) = pair(2);
        let outsider = "tagma-outsider".to_string();
        seed_session(&db, "s-1", &a1, &b1).await;
        seed_session(&db, "s-2", &a2, &b2).await;
        let mine = sessions_for_member(&db, &a1).await.unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, "s-1");
        assert!(
            sessions_for_member(&db, &outsider)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn append_assigns_monotonic_seq_and_reads_back() {
        let db = fresh_db().await;
        let (a, b) = pair(1);
        seed_session(&db, "s-1", &a, &b).await;
        let s1 = append(&db, "s-1", &a, b"[one]").await.unwrap();
        let s2 = append(&db, "s-1", &b, b"[two]").await.unwrap();
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        let rows = read_since(&db, "s-1", 0, 100).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[0].sender, a);
        assert_eq!(rows[1].sender, b);
        assert_eq!(rows[1].payload, b"[two]");
        // Windowed read: strictly-after semantics.
        let after = read_since(&db, "s-1", 1, 100).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].seq, 2);
        // Unknown session reads empty, never errors.
        assert!(read_since(&db, "s-none", 0, 100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn seq_is_per_session() {
        let db = fresh_db().await;
        let (a1, b1) = pair(1);
        let (a2, b2) = pair(2);
        seed_session(&db, "s-1", &a1, &b1).await;
        seed_session(&db, "s-2", &a2, &b2).await;
        let s1 = append(&db, "s-1", &a1, b"[]").await.unwrap();
        let s2 = append(&db, "s-2", &a2, b"[]").await.unwrap();
        assert_eq!(s1, 1);
        assert_eq!(s2, 1, "each session's sequence starts at 1");
    }

    #[tokio::test]
    async fn concurrent_appends_get_distinct_seqs() {
        let db = fresh_db().await;
        let (a, b) = pair(1);
        seed_session(&db, "s-1", &a, &b).await;
        let mut handles = Vec::new();
        for i in 0..8u32 {
            let db = db.clone();
            let sender = if i % 2 == 0 { a.clone() } else { b.clone() };
            handles.push(tokio::spawn(async move {
                append(&db, "s-1", &sender, &[i as u8]).await.unwrap()
            }));
        }
        let mut seqs = Vec::new();
        for h in handles {
            seqs.push(h.await.unwrap());
        }
        seqs.sort();
        assert_eq!(seqs, (1..=8).collect::<Vec<i64>>());
    }

    #[tokio::test]
    async fn deleting_session_cascades_to_messages_seq_and_cursors() {
        let db = fresh_db().await;
        let (a, b) = pair(1);
        seed_session(&db, "s-1", &a, &b).await;
        append(&db, "s-1", &a, b"[]").await.unwrap();
        set_read_cursor(&db, "s-1", &b, 1).await.unwrap();
        db.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "DELETE FROM direct_sessions WHERE id = 's-1'",
        ))
        .await
        .unwrap();
        // Every child row cascaded away (the cursor row checked via raw SQL:
        // nothing reads the cursors table through a query entity).
        assert!(find_session(&db, "s-1").await.unwrap().is_none());
        assert!(read_since(&db, "s-1", 0, 100).await.unwrap().is_empty());
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*) AS n FROM direct_read_cursors WHERE session_id = 's-1'",
            ))
            .await
            .unwrap()
            .unwrap();
        let n: i64 = row.try_get("", "n").unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn set_read_cursor_clamps_stale_writes() {
        let db = fresh_db().await;
        let (a, b) = pair(1);
        seed_session(&db, "s-1", &a, &b).await;
        assert_eq!(set_read_cursor(&db, "s-1", &b, 5).await.unwrap(), 5);
        // A stale write (a lagging client replaying an old watermark) never
        // moves the cursor backwards.
        assert_eq!(set_read_cursor(&db, "s-1", &b, 2).await.unwrap(), 5);
        // The other member's cursor is independent.
        assert_eq!(set_read_cursor(&db, "s-1", &a, 1).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn concurrent_cursor_writes_land_on_max() {
        let db = fresh_db().await;
        let (a, b) = pair(1);
        seed_session(&db, "s-1", &a, &b).await;
        let mut handles = Vec::new();
        for seq in [9i64, 3, 7, 1, 5] {
            let db = db.clone();
            let member = b.clone();
            handles.push(tokio::spawn(async move {
                set_read_cursor(&db, "s-1", &member, seq).await.unwrap()
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let seq: i64 = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT last_read_seq FROM direct_read_cursors WHERE session_id = 's-1' AND member = 'tagma-b-1'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "last_read_seq")
            .unwrap();
        assert_eq!(seq, 9);
    }
}
