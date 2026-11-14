//! Backfill `thread_participants` from the message log, once.
//!
//! Batch 2 started recording participants with the first message appended
//! after the table existed, and left history empty on purpose: nothing read
//! the table yet, so a hole cost nothing. Batch 3 routes delivery on it, so
//! the hole is now a missed wake for everyone who took part in a thread
//! between the `root_seq` column landing and the table landing. This
//! migration closes it, once, from the rows themselves.
//!
//! Two statements, because the head of a thread stores a NULL `root_seq`:
//!
//! - the author of every thread head that ever got a reply becomes
//!   `started`, joined at the head. Heads with no replies get no row, which
//!   is the same empty answer batch 2 documents: a thread nobody replied to
//!   has no delivery of its own to scope. This one runs FIRST, so a head
//!   author who also replied inside their own thread keeps the head position
//!   and the `started` reason; the other statement then skips them.
//! - everyone else who posted INSIDE a thread becomes `replied`, keyed to the
//!   thread they named. `DISTINCT ON` keeps each (thread, sender) pair's
//!   EARLIEST row: `joined_at_seq` is the position a reader starts counting
//!   unread from, so it must be the message that first put them in the
//!   thread, not the last one they sent.
//!
//! Mentions inside the window stay UNRECORDED here. They are not out of
//! reach -- the send already carries them in the payload JSON, which
//! `thread_facts` reads on every send -- this migration reads columns only and
//! does not decode payloads. So a member named in an old message and never
//! heard from again stays out of that thread's audience. That is the one
//! residue of the window, and it is narrower than the hole this migration
//! closes.
//!
//! Rerunning is a no-op: `ON CONFLICT DO NOTHING` skips the rows batch 2 and
//! the first run already wrote, so the count reported on the second run is 0.
//!
//! No deletion path is added. Rows still outlive members leaving the room;
//! readers intersect with the current membership.

use sea_orm::DatabaseBackend;
use sea_orm::Statement;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Everyone who posted inside a thread, at the earliest position they did.
pub(crate) const BACKFILL_REPLIES: &str = "\
INSERT INTO thread_participants \
    (room_id, root_seq, participant_id, joined_via, joined_at_seq, created_at) \
SELECT DISTINCT ON (room_id, root_seq, sender_id) \
    room_id, root_seq, sender_id, 'replied', seq, now() \
FROM room_messages \
WHERE root_seq IS NOT NULL AND sender_kind = 'agent' \
ORDER BY room_id, root_seq, sender_id, seq \
ON CONFLICT (room_id, root_seq, participant_id) DO NOTHING";

/// The author of each head that ever got a reply, joined at the head.
pub(crate) const BACKFILL_HEADS: &str = "\
INSERT INTO thread_participants \
    (room_id, root_seq, participant_id, joined_via, joined_at_seq, created_at) \
SELECT head.room_id, head.seq, head.sender_id, 'started', head.seq, now() \
FROM room_messages AS head \
WHERE head.root_seq IS NULL AND head.sender_kind = 'agent' \
    AND EXISTS ( \
        SELECT 1 FROM room_messages AS child \
        WHERE child.room_id = head.room_id AND child.root_seq = head.seq \
    ) \
ON CONFLICT (room_id, root_seq, participant_id) DO NOTHING";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        // Two statements, one line of evidence: the count is how a guess
        // ("the window is probably empty in production") becomes an
        // observation instead. 0 means history was already complete.
        // Heads go first. A head author who also replied inside their own
        // thread must be dated from the head; the replies statement's
        // `DO NOTHING` then skips them. The other order would record them
        // as `replied` at their first reply and drop the `started` reason.
        let heads = conn
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                BACKFILL_HEADS,
            ))
            .await?;
        let replies = conn
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                BACKFILL_REPLIES,
            ))
            .await?;
        let inserted = replies.rows_affected() + heads.rows_affected();
        tracing::info!(
            inserted,
            replied = replies.rows_affected(),
            started = heads.rows_affected(),
            "thread_participants backfill complete"
        );
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Down is a no-op on purpose: the rows are indistinguishable from the
        // ones the write path stores, and deleting them would take real
        // participation with them. Schema rollback is the table's own `down`.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! The backfill against a real Postgres: the rows it invents, the order it
    //! keeps, the rows it must not touch, and the second run that must do
    //! nothing. The migration's own `up` is not called here -- it would need a
    //! `SchemaManager`, and the SQL is the thing under test.

    use super::*;

    async fn seed_room(db: &crate::chat::db::Db, room: &str) {
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO rooms (id, created_by_user_id, created_at, membership_epoch) \
             VALUES ($1, $2, NOW(), 1)",
            [room.into(), "creator".into()],
        ))
        .await
        .expect("seed room");
    }

    /// One stored message row. `root` is the thread head's position; a head
    /// itself stores NULL, which is what makes the two statements necessary.
    async fn seed_message(
        db: &crate::chat::db::Db,
        room: &str,
        seq: i64,
        kind: &str,
        sender: &str,
        root: Option<i64>,
    ) {
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO room_messages \
             (room_id, seq, sender_kind, sender_id, epoch, ciphertext, root_seq, created_at) \
             VALUES ($1, $2, $3, $4, 1, $5, $6, NOW())",
            [
                room.into(),
                seq.into(),
                kind.into(),
                sender.into(),
                b"ct".to_vec().into(),
                root.into(),
            ],
        ))
        .await
        .expect("seed message");
    }

    async fn run_backfill(db: &crate::chat::db::Db) -> u64 {
        // Same order as `up`: heads before replies, so the head author of a
        // thread they also replied in keeps the head position.
        let heads = db
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                BACKFILL_HEADS,
            ))
            .await
            .expect("heads backfill");
        let replies = db
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                BACKFILL_REPLIES,
            ))
            .await
            .expect("replies backfill");
        replies.rows_affected() + heads.rows_affected()
    }

    /// `(participant_id, joined_via, joined_at_seq)` for one thread, in id order.
    async fn rows(db: &crate::chat::db::Db, room: &str, root: i64) -> Vec<(String, String, i64)> {
        db.query_all(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT participant_id, joined_via, joined_at_seq FROM thread_participants \
             WHERE room_id = $1 AND root_seq = $2 ORDER BY participant_id",
            [room.into(), root.into()],
        ))
        .await
        .expect("read participants")
        .into_iter()
        .map(|r| {
            (
                r.try_get::<String>("", "participant_id").expect("id"),
                r.try_get::<String>("", "joined_via").expect("via"),
                r.try_get::<i64>("", "joined_at_seq").expect("at"),
            )
        })
        .collect()
    }

    #[tokio::test]
    async fn backfill_records_the_earliest_reply_and_the_head_author() {
        let db = crate::test_support::provision_test_db().await;
        seed_room(&db, "room-1").await;
        // seq 1 heads a thread; 2 and 4 are replies by the same agent, 3 by
        // another, 5 a human reply (never a participant row), and 6 the head
        // author speaking inside its own thread -- the head rule has to win
        // over that reply, which is why the two statements run in this order.
        seed_message(&db, "room-1", 1, "agent", "a1", None).await;
        seed_message(&db, "room-1", 2, "agent", "a2", Some(1)).await;
        seed_message(&db, "room-1", 3, "agent", "a3", Some(1)).await;
        seed_message(&db, "room-1", 4, "agent", "a2", Some(1)).await;
        seed_message(&db, "room-1", 5, "human", "h1", Some(1)).await;
        seed_message(&db, "room-1", 6, "agent", "a1", Some(1)).await;

        let inserted = run_backfill(&db).await;
        assert_eq!(
            inserted, 3,
            "the head author plus one row per distinct agent, not per message"
        );

        assert_eq!(
            rows(&db, "room-1", 1).await,
            vec![
                ("a1".to_string(), "started".to_string(), 1),
                ("a2".to_string(), "replied".to_string(), 2),
                ("a3".to_string(), "replied".to_string(), 3),
            ],
            "one row per person at the EARLIEST position they entered"
        );
    }

    #[tokio::test]
    async fn backfill_is_idempotent_and_keeps_a_recorded_mention() {
        let db = crate::test_support::provision_test_db().await;
        seed_room(&db, "room-1").await;
        seed_message(&db, "room-1", 1, "agent", "a1", None).await;
        seed_message(&db, "room-1", 2, "agent", "a2", Some(1)).await;
        // The write path already recorded a mention at seq 2; the backfill must
        // not rewrite it into `replied`.
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO thread_participants \
             (room_id, root_seq, participant_id, joined_via, joined_at_seq, created_at) \
             VALUES ($1, 1, 'a3', 'mentioned', 2, NOW())",
            ["room-1".into()],
        ))
        .await
        .expect("seed mention");

        assert_eq!(run_backfill(&db).await, 2, "head author plus the one reply");
        assert_eq!(run_backfill(&db).await, 0, "a second run inserts nothing");

        let got = rows(&db, "room-1", 1).await;
        assert_eq!(
            got,
            vec![
                ("a1".to_string(), "started".to_string(), 1),
                ("a2".to_string(), "replied".to_string(), 2),
                ("a3".to_string(), "mentioned".to_string(), 2),
            ],
            "the earlier reason wins and the second run changes nothing"
        );
    }
}
