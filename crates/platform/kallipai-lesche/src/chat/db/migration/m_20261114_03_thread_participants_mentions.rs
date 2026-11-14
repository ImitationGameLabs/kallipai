//! Record the mentions the earlier window left out of `thread_participants`.
//!
//! The write path records a thread's participants from the message it is
//! storing, mentions included, but the table only started taking rows with the
//! first send after it landed. A mention inside a thread sent in that window
//! reached its members when it was delivered; the table just never learned
//! about it. Delivery routes on that table and the badge counts from it, so
//! the residue is visible twice: a missed wake and a badge that undercounts.
//!
//! This migration reads the rows the SEPARATE statements cannot see -- the
//! mentioned members -- from the payload JSON the send already stored, which is
//! the same plaintext `RoomMessage` the write path decodes.
//!
//! What it deliberately does NOT do:
//!
//! - It never manufactures a row for a head message. A head reaches the whole
//!   room, so its mentions are not a narrowing fact, and the write path stores
//!   none for them either. Only rows INSIDE a thread are scanned.
//! - It never records someone who could not receive the thread today: a human
//!   (never a room participant row) and a member who has left the room are
//!   counted and skipped, not written. Mention rows are read by the delivery
//!   path, which intersects them with the live membership anyway, so a row for
//!   a departed member would be noise with a membership half-life.
//!
//! A mention only ever moves a member EARLIER. `joined_at_seq` is the position
//! a reader starts counting from, and `joined_via` is the first reason the
//! member entered the thread, so an existing row is rewritten only when this
//! message's position is strictly earlier than the stored one. That makes a
//! rerun a no-op: the second run writes zero rows and leaves every position
//! where the first run put it.
//!
//! The counters are the point of the run: how many rows decoded, how many would
//! not decode (a payload that is not this shape -- the window's own rows are
//! always plaintext `RoomMessage`, so a nonzero count means something else
//! wrote here), how many mentions named someone who could not receive them, and
//! how many participant rows were written.

use std::collections::HashSet;

use kallipai_lesche_common::message::RoomMessage;
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Rows decoded per round trip. The scan is keyset-paged on
/// `(room_id, seq)`, so a batch boundary cannot skip or repeat a row.
const BATCH: i64 = 500;

/// One run's numbers, for the log line that turns "probably nothing" into an
/// observation.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MentionBackfill {
    /// Rows inside a thread whose payload decoded.
    pub parsed: u64,
    /// Rows inside a thread whose payload did not decode as a `RoomMessage`.
    pub failed: u64,
    /// Mentions naming a human, a departed member, or a stranger.
    pub skipped: u64,
    /// Participant rows written (inserted, or moved earlier).
    pub joined: u64,
}

/// The room's current agent members, as the ids a payload mentions carry.
async fn agent_members(conn: &impl ConnectionTrait, room: &str) -> Result<HashSet<String>, DbErr> {
    let rows = conn
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT member_id FROM room_members WHERE room_id = $1 AND kind = 'agent'",
            [room.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|r| r.try_get::<String>("", "member_id"))
        .collect()
}

/// Walk every message inside a thread and record the members it named.
pub(crate) async fn backfill_mentions(
    conn: &impl ConnectionTrait,
) -> Result<MentionBackfill, DbErr> {
    let mut out = MentionBackfill::default();
    let mut after = (String::new(), 0i64);
    // Rooms are visited in id order, so a room's members are loaded once per
    // room and dropped when the scan moves on.
    let mut members: Option<(String, HashSet<String>)> = None;

    loop {
        let rows = conn
            .query_all(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT room_id, seq, root_seq, ciphertext FROM room_messages \
                 WHERE root_seq IS NOT NULL AND (room_id, seq) > ($1, $2) \
                 ORDER BY room_id, seq LIMIT $3",
                [after.0.clone().into(), after.1.into(), BATCH.into()],
            ))
            .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let room: String = row.try_get("", "room_id")?;
            let seq: i64 = row.try_get("", "seq")?;
            // The scan filters NULL roots, so this row IS inside a thread.
            let root: i64 = row.try_get("", "root_seq")?;
            let ciphertext: Vec<u8> = row.try_get("", "ciphertext")?;
            after = (room.clone(), seq);

            let Ok(message) = serde_json::from_slice::<RoomMessage>(&ciphertext) else {
                out.failed += 1;
                continue;
            };
            out.parsed += 1;
            if message.mentions.is_empty() {
                continue;
            }

            if members.as_ref().map(|(id, _)| id != &room).unwrap_or(true) {
                members = Some((room.clone(), agent_members(conn, &room).await?));
            }
            let room_members = &members.as_ref().expect("just loaded").1;

            for mention in message.mentions {
                if !room_members.contains(mention.as_ref()) {
                    out.skipped += 1;
                    continue;
                }
                let written = conn
                    .execute(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "INSERT INTO thread_participants \
                             (room_id, root_seq, participant_id, joined_via, joined_at_seq, \
                              created_at) \
                         VALUES ($1, $2, $3, 'mentioned', $4, now()) \
                         ON CONFLICT (room_id, root_seq, participant_id) DO UPDATE SET \
                             joined_at_seq = EXCLUDED.joined_at_seq, \
                             joined_via = 'mentioned' \
                         WHERE thread_participants.joined_at_seq > EXCLUDED.joined_at_seq",
                        [
                            room.clone().into(),
                            // The thread this message belongs to: the mention
                            // puts the member in that discussion.
                            root.into(),
                            mention.as_ref().to_string().into(),
                            seq.into(),
                        ],
                    ))
                    .await?;
                out.joined += written.rows_affected();
            }
        }
    }
    Ok(out)
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let stats = backfill_mentions(manager.get_connection()).await?;
        tracing::info!(
            parsed = stats.parsed,
            failed = stats.failed,
            skipped = stats.skipped,
            joined = stats.joined,
            "thread_participants mention backfill complete"
        );
        if stats.failed > 0 {
            // A payload that does not decode is counted, not fatal: the
            // migration stays applied and the rest of the log is backfilled.
            // Retrying those rows means deleting this migration's row from
            // `seaql_migrations` and restarting the service once the payload is
            // readable; a rerun is safe because it only ever moves a position
            // earlier.
            tracing::warn!(
                failed = stats.failed,
                "mention backfill left undecodable payloads; rerun needs this \
                 migration's seaql_migrations row deleted, then a restart"
            );
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Down is a no-op on purpose: these rows are indistinguishable from the
        // ones the write path stores, and deleting them would take real
        // participation with them. Schema rollback is the table's own `down`.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! The mention backfill against a real Postgres: the rows it invents, the
    //! ones it must skip, the position it may only move earlier, and the rerun
    //! that writes nothing.

    use super::*;
    use kallipai_lesche_common::rooms::MemberId;

    type Db = crate::chat::db::Db;

    /// Mentions are canonical participant ids; the payload's own normalization
    /// keeps a hyphenated lowercase UUID and drops anything else, so the tests
    /// name members the way the wire does.
    const A3: &str = "33333333-3333-4333-8333-333333333333";
    const A4: &str = "44444444-4444-4444-8444-444444444444";
    const H1: &str = "11111111-1111-4111-8111-111111111111";
    const A9: &str = "99999999-9999-4999-8999-999999999999";

    async fn seed_room(db: &Db, room: &str) {
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO rooms (id, created_by_user_id, created_at, membership_epoch) \
             VALUES ($1, $2, NOW(), 1)",
            [room.into(), "creator".into()],
        ))
        .await
        .expect("seed room");
    }

    async fn seed_member(db: &Db, room: &str, member: &str, kind: &str) {
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO room_members (room_id, member_id, kind, source_id, joined_at, added_by) \
             VALUES ($1, $2, $3, $4, NOW(), 'creator')",
            [
                room.into(),
                member.into(),
                kind.into(),
                format!("src-{member}").into(),
            ],
        ))
        .await
        .expect("seed member");
    }

    /// The plaintext payload a room send stores, naming `mentions`.
    fn payload(mentions: &[&str]) -> Vec<u8> {
        serde_json::to_vec(&RoomMessage {
            text: "hi".to_string(),
            attachment: None,
            quote: None,
            thread: None,
            mentions: mentions
                .iter()
                .map(|m| MemberId::from(String::from(*m)))
                .collect(),
        })
        .expect("payload")
    }

    /// One stored message row. `root` is the thread head's position; a head
    /// itself stores NULL, which is what keeps head mentions out of the scan.
    async fn seed_message(
        db: &Db,
        room: &str,
        seq: i64,
        kind: &str,
        sender: &str,
        root: Option<i64>,
        ciphertext: Vec<u8>,
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
                ciphertext.into(),
                root.into(),
            ],
        ))
        .await
        .expect("seed message");
    }

    /// `(participant_id, joined_via, joined_at_seq)` for one thread, in id
    /// order.
    async fn rows(db: &Db, room: &str, root: i64) -> Vec<(String, String, i64)> {
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

    /// A mention puts a current agent member into its thread at the position
    /// that named them; a human, a stranger and a head's mentions all stay out.
    #[tokio::test]
    async fn backfill_records_a_current_agent_member_and_skips_the_rest() {
        let db = crate::test_support::provision_test_db().await;
        seed_room(&db, "room-1").await;
        seed_member(&db, "room-1", A3, "agent").await;
        seed_member(&db, "room-1", A4, "agent").await;
        seed_member(&db, "room-1", H1, "human").await;

        // 1 heads a thread (its mention is never recorded), 2 replies inside it
        // naming an agent member, a human, and someone who is not here.
        seed_message(&db, "room-1", 1, "agent", "a1", None, payload(&[A4])).await;
        seed_message(
            &db,
            "room-1",
            2,
            "agent",
            "a2",
            Some(1),
            payload(&[A3, H1, A9]),
        )
        .await;

        let stats = backfill_mentions(&db).await.expect("backfill");
        assert_eq!(
            stats,
            MentionBackfill {
                parsed: 1,
                failed: 0,
                skipped: 2,
                joined: 1,
            },
            "one row scanned, one mention written, two who could not receive it"
        );
        assert_eq!(
            rows(&db, "room-1", 1).await,
            vec![(A3.to_string(), "mentioned".to_string(), 2)]
        );
        assert_eq!(
            rows(&db, "room-1", 4).await,
            Vec::new(),
            "a head's mention is not a narrowing fact and is never recorded"
        );
    }

    /// A mention may only move a member EARLIER: an existing later row becomes
    /// the mention, and a second run changes nothing.
    #[tokio::test]
    async fn backfill_moves_a_later_row_earlier_and_is_idempotent() {
        let db = crate::test_support::provision_test_db().await;
        seed_room(&db, "room-1").await;
        seed_member(&db, "room-1", A3, "agent").await;

        seed_message(&db, "room-1", 1, "agent", "a1", None, payload(&[])).await;
        seed_message(&db, "room-1", 2, "agent", "a2", Some(1), payload(&[A3])).await;
        // The write path already put a3 in this thread, later: their own reply
        // at position 7.
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO thread_participants \
             (room_id, root_seq, participant_id, joined_via, joined_at_seq, created_at) \
             VALUES ('room-1', 1, '33333333-3333-4333-8333-333333333333', 'replied', 7, NOW())",
            [],
        ))
        .await
        .expect("seed later row");

        let first = backfill_mentions(&db).await.expect("backfill");
        assert_eq!((first.joined, first.parsed), (1, 1));
        assert_eq!(
            rows(&db, "room-1", 1).await,
            vec![(A3.to_string(), "mentioned".to_string(), 2)],
            "the earlier position wins and carries the reason that reached it"
        );

        let second = backfill_mentions(&db).await.expect("rerun");
        assert_eq!(
            second,
            MentionBackfill {
                parsed: 1,
                failed: 0,
                skipped: 0,
                joined: 0,
            },
            "a rerun writes nothing and leaves the position where it was"
        );
        assert_eq!(
            rows(&db, "room-1", 1).await,
            vec![(A3.to_string(), "mentioned".to_string(), 2)]
        );
    }

    /// A payload that is not this shape is counted and skipped, not fatal: one
    /// unreadable row must not stop the rows behind it.
    #[tokio::test]
    async fn backfill_counts_a_payload_it_cannot_decode() {
        let db = crate::test_support::provision_test_db().await;
        seed_room(&db, "room-1").await;
        seed_member(&db, "room-1", A3, "agent").await;

        seed_message(&db, "room-1", 1, "agent", "a1", None, payload(&[])).await;
        seed_message(
            &db,
            "room-1",
            2,
            "agent",
            "a2",
            Some(1),
            b"not a room message".to_vec(),
        )
        .await;
        seed_message(&db, "room-1", 3, "agent", "a2", Some(1), payload(&[A3])).await;

        let stats = backfill_mentions(&db).await.expect("backfill");
        assert_eq!(
            stats,
            MentionBackfill {
                parsed: 1,
                failed: 1,
                skipped: 0,
                joined: 1,
            },
            "the row after the unreadable one is still scanned"
        );
        assert_eq!(
            rows(&db, "room-1", 1).await,
            vec![(A3.to_string(), "mentioned".to_string(), 3)]
        );
    }
    /// The backfill exists for one number: a member named late in a thread
    /// counts from the naming row, so recording the earliest mention moves where
    /// their badge starts. The residue here is a row written at the later
    /// mention only, which is the shape the mention pass leaves behind.
    #[tokio::test]
    async fn backfill_moves_the_badge_start_to_the_earliest_mention() {
        let db = crate::test_support::provision_test_db().await;
        seed_room(&db, "room-b").await;
        seed_member(&db, "room-b", A3, "agent").await;
        seed_member(&db, "room-b", A4, "agent").await;

        // A4 heads a thread at 1, names A3 at 2, replies at 3, names A3 again
        // at 4.
        seed_message(&db, "room-b", 1, "agent", A4, None, payload(&[])).await;
        seed_message(&db, "room-b", 2, "agent", A4, Some(1), payload(&[A3])).await;
        seed_message(&db, "room-b", 3, "agent", A4, Some(1), payload(&[])).await;
        seed_message(&db, "room-b", 4, "agent", A4, Some(1), payload(&[A3])).await;
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO thread_participants \
             (room_id, root_seq, participant_id, joined_via, joined_at_seq, created_at) \
             VALUES ($1, $2, $3, 'mentioned', $4, now())",
            ["room-b".into(), 1i64.into(), A3.into(), 4i64.into()],
        ))
        .await
        .expect("residue row");

        let a3 = MemberId::from(A3.to_string());
        // A3 has read the head.
        crate::chat::db::store::set_read_cursor(&db, "room-b", &a3, 1)
            .await
            .expect("cursor");
        let badge = || async {
            crate::chat::db::store::unread_for_member(&db, "room-b", &a3)
                .await
                .expect("badge")
                .unread
        };
        assert_eq!(badge().await, 1, "only the later mention counts");

        backfill_mentions(&db).await.expect("backfill");

        assert_eq!(
            rows(&db, "room-b", 1).await,
            vec![(A3.to_string(), "mentioned".to_string(), 2)],
            "the earliest mention wins"
        );
        assert_eq!(badge().await, 3, "the badge now starts at row 2");
    }
}
