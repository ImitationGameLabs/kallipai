//! The `room_messages` store: append + history read.
//!
//! The relay persists each fanned envelope as one payload row and serves
//! history pulls by reading rows back. `seq` is per-room, assigned race-free by
//! an `INSERT ... ON CONFLICT DO UPDATE ... RETURNING` against
//! `room_message_seq` (a single statement that seeds the row on first append
//! and increments it thereafter, under the row lock the upsert acquires -- so
//! concurrent appends to the same room serialize with no PK-violation loser).
//! A row stores the sender's STABLE identity only -- the `ParticipantId` + kind
//! -- never the display handle (a derived, mutable string resolved at read time
//! from the registry; see `member_identity`). The payload is the plaintext
//! `RoomMessage` JSON, stored opaquely -- the lesche is the room's store of
//! record and is trusted to read room content.

use std::collections::HashMap;

use kallip_agora_common::control_plane::RoomMembership;
use kallip_agora_common::ids::{MemberId, ParticipantKind};
use kallip_agora_common::participant::RoomMember;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use time::OffsetDateTime;

use super::Db;
use super::entity::{room_member_revocations, room_members, room_messages, rooms};

/// One stored message, as read back for a history pull. Carries the sender's
/// STABLE identity (`sender_id` + `sender_kind`) only; the display handle is
/// resolved by the caller from the registry (it is a function of this identity,
/// not a fact to persist).
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub seq: i64,
    pub sender_id: MemberId,
    pub sender_kind: ParticipantKind,
    pub epoch: i64,
    pub ciphertext: Vec<u8>,
    pub created_at: OffsetDateTime,
}

/// Atomically advance the per-room sequence and return the next `seq`. The
/// `INSERT ... ON CONFLICT DO UPDATE ... RETURNING` is a single statement that
/// seeds the row on first append and increments it thereafter, all under the
/// row lock the upsert acquires -- so concurrent appends to the same room
/// serialize with no PK-violation loser (a find-then-insert would race on the
/// seed; this does not).
async fn next_seq(txn: &impl ConnectionTrait, room: &str) -> Result<i64, sea_orm::DbErr> {
    let row = txn
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO room_message_seq (room_id, next_seq) VALUES ($1, 1) \
             ON CONFLICT (room_id) DO UPDATE SET \
             next_seq = room_message_seq.next_seq + 1 \
             RETURNING next_seq",
            [room.into()],
        ))
        .await?
        .ok_or_else(|| sea_orm::DbErr::Custom("room_message_seq upsert returned no row".into()))?;
    let seq: i64 = row.try_get("", "next_seq")?;
    Ok(seq)
}

/// Append a room-message payload row to the room's history (the payload is
/// plaintext `RoomMessage` JSON stored opaquely -- the column name `ciphertext`
/// is a legacy shared-`Envelope` term). Returns the assigned `seq`. Only the
/// sender's stable identity (a `MemberId` + kind) is persisted; the display
/// handle is derived at read time, so none is taken here.
/// The sequence is advanced and the row inserted in the same transaction, so a
/// failed insert rolls the sequence back (no gaps in the log).
pub async fn append(
    db: &Db,
    room: &str,
    sender_id: &MemberId,
    sender_kind: ParticipantKind,
    epoch: i64,
    ciphertext: &[u8],
) -> Result<i64, sea_orm::DbErr> {
    let result = db
        .transaction::<_, i64, sea_orm::DbErr>(|txn| {
            let room = room.to_string();
            let kind = sender_kind.as_str().to_string();
            let id = sender_id.as_ref().to_string();
            let ciphertext = ciphertext.to_vec();
            Box::pin(async move {
                let seq = next_seq(txn, &room).await?;
                let now = OffsetDateTime::now_utc();
                room_messages::ActiveModel {
                    room_id: Set(room),
                    seq: Set(seq),
                    sender_kind: Set(kind),
                    sender_id: Set(id),
                    epoch: Set(epoch),
                    ciphertext: Set(ciphertext),
                    created_at: Set(now),
                }
                .insert(txn)
                .await?;
                Ok(seq)
            })
        })
        .await;
    // Flatten the TransactionError (the store surfaces only DbErr; there is no
    // business-rule rejection to carry as an ApiError at this layer).
    let seq = result.map_err(|e| match e {
        sea_orm::TransactionError::Connection(e) | sea_orm::TransactionError::Transaction(e) => e,
    })?;
    Ok(seq)
}

/// Read the room's history with `seq > after_seq`, ascending, up to `limit`.
/// `after_seq = 0` reads from the start. The relay serves backlog pulls and
/// new-member backfill from this. Each row carries the sender's stable identity
/// only; the caller resolves the display handle from the registry.
pub async fn read_since(
    db: &Db,
    room: &str,
    after_seq: i64,
    limit: u64,
) -> Result<Vec<StoredMessage>, sea_orm::DbErr> {
    let rows = room_messages::Entity::find()
        .filter(room_messages::Column::RoomId.eq(room))
        .filter(room_messages::Column::Seq.gt(after_seq))
        .order_by_asc(room_messages::Column::Seq)
        .limit(limit)
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| StoredMessage {
            seq: r.seq,
            sender_id: MemberId::from(r.sender_id),
            sender_kind: ParticipantKind::from_label(&r.sender_kind)
                .unwrap_or(ParticipantKind::Agent),
            epoch: r.epoch,
            ciphertext: r.ciphertext,
            created_at: r.created_at,
        })
        .collect())
}

/// Recover the registry-resolvable `source_id` (`user_id` / `tagma_id`) for a
/// set of room members, searching BOTH the live `room_members` membership AND
/// the append-only `room_member_revocations` audit. A sender who left the room
/// after sending is hard-deleted from the live table but retained in the audit,
/// so the union recovers departed senders too (every message sender was a
/// member at send time). Live rows win on conflict. The sender's `kind` is NOT
/// returned: the message row already carries it (the immutable send-time fact),
/// so the caller reads kind from the row.
pub async fn member_source_map(
    db: &Db,
    room: &str,
    member_ids: &[String],
) -> Result<HashMap<String, String>, sea_orm::DbErr> {
    if member_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ids = member_ids.to_vec();
    let mut map: HashMap<String, String> = HashMap::new();
    let live = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room))
        .filter(room_members::Column::MemberId.is_in(ids.clone()))
        .all(db)
        .await?;
    for m in live {
        map.insert(m.member_id, m.source_id);
    }
    let revoked = room_member_revocations::Entity::find()
        .filter(room_member_revocations::Column::RoomId.eq(room))
        .filter(room_member_revocations::Column::MemberId.is_in(ids))
        .all(db)
        .await?;
    for m in revoked {
        map.entry(m.member_id).or_insert(m.source_id);
    }
    Ok(map)
}

/// The room's live membership snapshot, read from the lesche-local graph.
/// `None` if the room does not exist (the existence gate the delivery routes
/// collapse to a uniform 404). The single live membership table holds only
/// active rows, so no status filter is needed. This is the local replacement
/// for the agora `/internal/room-membership` RPC + TTL cache: one SQL read on
/// the delivery hot path, strongly consistent with mutations in the same DB.
pub async fn room_membership(
    db: &Db,
    room: &str,
) -> Result<Option<RoomMembership>, sea_orm::DbErr> {
    let Some(room_row) = rooms::Entity::find_by_id(room).one(db).await? else {
        return Ok(None);
    };
    let members = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room))
        // Deterministic order so the fan-out snapshot does not churn across
        // reads (spurious diffs at consumers) and membership diffs stay stable.
        .order_by_asc(room_members::Column::MemberId)
        .all(db)
        .await?
        .into_iter()
        .map(|m| RoomMember {
            id: MemberId::from(m.member_id),
            kind: ParticipantKind::from_label(&m.kind).unwrap_or(ParticipantKind::Agent),
        })
        .collect();
    Ok(Some(RoomMembership {
        members,
        membership_epoch: room_row.membership_epoch,
    }))
}

#[cfg(test)]
mod tests {
    //! Append + read round-trip against ephemeral Postgres: the per-room
    //! sequence is race-free under concurrent appends and the sender
    //! `Participant` survives the column split/reconstruct.

    use super::*;
    use testcontainers_modules::postgres::Postgres;
    use testcontainers_modules::testcontainers::{ContainerAsync, runners::AsyncRunner};

    const PG_USER: &str = "postgres";
    const PG_PASSWORD: &str = "postgres";
    const PG_DB: &str = "postgres";

    /// Provision an ephemeral Postgres, connect + migrate, and return BOTH the
    /// container handle and the db. The container must stay alive for the
    /// test's duration (dropping it stops/kills Postgres), so the caller binds
    /// it to `_container`.
    async fn fresh_db() -> (ContainerAsync<Postgres>, Db) {
        let container = Postgres::default()
            .with_db_name(PG_DB)
            .with_user(PG_USER)
            .with_password(PG_PASSWORD)
            .start()
            .await
            .expect("start postgres");
        let port = container.get_host_port_ipv4(5432).await.expect("host port");
        let url = format!("postgres://{PG_USER}:{PG_PASSWORD}@127.0.0.1:{port}/{PG_DB}");
        let db = crate::db::connect_and_migrate(&url)
            .await
            .expect("connect + migrate");
        (container, db)
    }

    /// A test sender's stable identity (no display handle -- the store no longer
    /// persists one).
    fn user(handle: &str) -> (MemberId, ParticipantKind) {
        (
            MemberId::from(format!("u-{handle}")),
            ParticipantKind::Human,
        )
    }

    fn agent() -> (MemberId, ParticipantKind) {
        (MemberId::from("t-1".to_string()), ParticipantKind::Agent)
    }

    /// Insert a minimal `rooms` row so the `room_messages` / `room_message_seq`
    /// FK to `rooms.id` (declared inline in `m_20260804_01_init`) is satisfied.
    /// The store tests exercise the seq/message logic in isolation, bypassing
    /// the room-creation route, so they must seed the parent row themselves.
    async fn seed_room(db: &Db, room_id: &str) {
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO rooms (id, created_by_user_id, created_at, membership_epoch) \
             VALUES ($1, $2, NOW(), 1)",
            [room_id.into(), "creator".into()],
        ))
        .await
        .expect("seed room");
    }

    #[tokio::test]
    async fn append_assigns_monotonic_seq_and_reads_back() {
        let (_container, db) = fresh_db().await;
        seed_room(&db, "room-1").await;
        let (alice_pid, human) = user("alice");
        let (tagma_pid, agent_kind) = agent();
        let s1 = append(&db, "room-1", &alice_pid, human, 1, b"ct-1")
            .await
            .unwrap();
        let s2 = append(&db, "room-1", &tagma_pid, agent_kind, 1, b"ct-2")
            .await
            .unwrap();
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);

        let msgs = read_since(&db, "room-1", 0, 100).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].seq, 1);
        assert_eq!(msgs[0].ciphertext, b"ct-1");
        assert_eq!(msgs[0].epoch, 1);
        assert!(msgs[0].created_at <= OffsetDateTime::now_utc());
        assert_eq!(msgs[0].sender_id, alice_pid);
        assert_eq!(msgs[0].sender_kind, ParticipantKind::Human);
        assert_eq!(msgs[1].seq, 2);
        assert_eq!(msgs[1].epoch, 1);
        assert_eq!(msgs[1].sender_id, tagma_pid);
        assert_eq!(msgs[1].sender_kind, ParticipantKind::Agent);

        // after_seq is exclusive.
        let tail = read_since(&db, "room-1", 1, 100).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 2);

        // limit caps the window (insert a third, then cap at 2 from the start).
        append(&db, "room-1", &alice_pid, human, 2, b"ct-3")
            .await
            .unwrap();
        let capped = read_since(&db, "room-1", 0, 2).await.unwrap();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].seq, 1);
        assert_eq!(capped[1].seq, 2);
        // epoch round-trips for the third message (epoch 2).
        let after2 = read_since(&db, "room-1", 2, 100).await.unwrap();
        assert_eq!(after2.len(), 1);
        assert_eq!(after2[0].epoch, 2);
    }

    /// Sequences are independent per room (each starts at 1).
    #[tokio::test]
    async fn seq_is_per_room() {
        let (_container, db) = fresh_db().await;
        seed_room(&db, "room-a").await;
        seed_room(&db, "room-b").await;
        let (alice, human) = user("alice");
        let a = append(&db, "room-a", &alice, human, 1, b"a1")
            .await
            .unwrap();
        let b = append(&db, "room-b", &alice, human, 1, b"b1")
            .await
            .unwrap();
        let a2 = append(&db, "room-a", &alice, human, 2, b"a2")
            .await
            .unwrap();
        assert_eq!(a, 1);
        assert_eq!(b, 1);
        assert_eq!(a2, 2);
    }

    /// Concurrent appends to the SAME room never collide: every append gets a
    /// distinct seq. Proves the seq upsert serializes. (4 tasks to stay under
    /// the default pool size under full-suite parallel-test container load.)
    #[tokio::test]
    async fn concurrent_appends_get_distinct_seqs() {
        let (_container, db) = fresh_db().await;
        seed_room(&db, "room-c").await;
        let db = std::sync::Arc::new(db);
        let (alice, human) = user("alice");
        let alice = std::sync::Arc::new(alice);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let db = db.clone();
            let alice = alice.clone();
            handles.push(tokio::spawn(async move {
                append(&db, "room-c", &alice, human, 1, b"x").await.unwrap()
            }));
        }
        let mut seqs: Vec<i64> = Vec::with_capacity(handles.len());
        for h in handles {
            seqs.push(h.await.unwrap());
        }
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 4);
        assert_eq!(seqs[0], 1);
        assert_eq!(seqs[3], 4);
    }

    /// Concurrent FIRST appends to a brand-new room race the seed: the upsert
    /// (`ON CONFLICT DO UPDATE ... RETURNING`) absorbs it, so every concurrent
    /// first-append gets a distinct seq with no PK-violation loser. This is the
    /// case a find-then-insert design would get wrong.
    #[tokio::test]
    async fn concurrent_first_appends_seed_race_upserted() {
        let (_container, db) = fresh_db().await;
        seed_room(&db, "room-seed").await;
        let db = std::sync::Arc::new(db);
        let (alice, human) = user("alice");
        let alice = std::sync::Arc::new(alice);
        // `room-seed` has no seq row yet -- all 4 appends hit the seed path.
        let mut handles = Vec::new();
        for _ in 0..4 {
            let db = db.clone();
            let alice = alice.clone();
            handles.push(tokio::spawn(async move {
                append(&db, "room-seed", &alice, human, 1, b"s")
                    .await
                    .unwrap()
            }));
        }
        let mut seqs: Vec<i64> = Vec::new();
        for h in handles {
            seqs.push(h.await.unwrap());
        }
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(
            seqs.len(),
            4,
            "no loser; all 4 first-appends got distinct seqs"
        );
        assert_eq!(seqs[0], 1);
        assert_eq!(seqs[3], 4);
    }

    #[tokio::test]
    async fn deleting_a_room_cascades_to_its_messages_and_seq() {
        // The init migration declares ON DELETE CASCADE FKs from `room_messages`
        // + `room_message_seq` to `rooms.id`. A future room-deletion route (or
        // direct maintenance) must not leave orphan ciphertext rows.
        let (_container, db) = fresh_db().await;
        seed_room(&db, "room-gone").await;
        let (alice, human) = user("alice");
        append(&db, "room-gone", &alice, human, 1, b"ct-1")
            .await
            .unwrap();
        // Sanity: the row + its seq exist.
        assert_eq!(read_since(&db, "room-gone", 0, 10).await.unwrap().len(), 1);

        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "DELETE FROM rooms WHERE id = $1",
            ["room-gone".into()],
        ))
        .await
        .unwrap();

        // Both the message and its seq row cascaded away.
        assert_eq!(read_since(&db, "room-gone", 0, 10).await.unwrap().len(), 0);
        let seq = db
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT next_seq FROM room_message_seq WHERE room_id = $1",
                ["room-gone".into()],
            ))
            .await
            .unwrap();
        assert!(
            seq.is_none(),
            "the seq row should have been cascaded away with the room"
        );
    }
}
