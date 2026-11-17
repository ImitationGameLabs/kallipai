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

use kallipai_archeion_common::ids::ParticipantKind;
use kallipai_lesche_common::message::ThreadFacts;
use kallipai_lesche_common::rooms::{MemberId, RoomMember, RoomMembership, ThreadChoice};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait, query::Condition,
};
use time::OffsetDateTime;

use super::Db;
use super::entity::{
    room_member_revocations, room_members, room_messages, room_read_cursors, rooms,
    thread_participants, thread_subscriptions,
};

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
    /// The discussion this row belongs to: the `seq` of its head row, or `None`
    /// when the row heads its own (`root_seq.unwrap_or(seq)` is the head).
    pub root_seq: Option<i64>,
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

/// The policy for a failed participant read, in one place: a send must never
/// fail because the audience could not be read. `None` rather than an empty
/// vector, because EMPTY is the ordinary answer -- a thread only humans
/// replied in records no participant row -- and a failed read must stay
/// distinguishable from it.
///
/// The two answers go opposite ways once the audience reads them. An empty set
/// leaves each agent on its default, which is out. A failed read counts every
/// agent as recorded, which is in: a wake costs an agent a re-pull, while a
/// message it was never woken for is a message the thread lost for it.
///
/// Both answers are reported HERE, where the table is read: the failure beside
/// the table that failed, the empty table as the fact it is. A reader of the log
/// should not have to infer either from how a wake came out -- an empty
/// participants table is the ordinary shape of a thread only humans replied in,
/// and it stays visible whether or not some member also recorded a choice.
fn participants_or_unknown(
    room: &str,
    root: i64,
    seq: i64,
    result: Result<Vec<ThreadParticipant>, sea_orm::DbErr>,
) -> Option<Vec<MemberId>> {
    match result {
        Ok(rows) => {
            // The line says this thread holds no participant row yet, which
            // is the ordinary shape of a thread only humans took part in --
            // not a fault. Debug, because a WARN per reply would teach readers
            // to ignore the level.
            if rows.is_empty() {
                tracing::debug!(
                    room_id = room,
                    root,
                    seq,
                    "thread participants table holds no rows for this reply; its agents fall to the default"
                );
            }
            Some(rows.into_iter().map(|p| p.participant_id).collect())
        }
        Err(e) => {
            tracing::warn!(
                room_id = room,
                root,
                error = %e,
                "thread participants read failed; the audience treats every agent as recorded"
            );
            None
        }
    }
}

/// The policy for a failed subscription read, next to [`participants_or_unknown`]
/// and for the same reason: a send must never fail because an audience input
/// could not be read.
///
/// `None` rather than an empty vector, because EMPTY is the ordinary answer
/// here -- every member is on the default -- so a failed read has to be
/// distinguishable. Collapsing the two would read a store hiccup as a room
/// that muted itself; the caller widens to the whole room instead.
fn subscriptions_or_unknown(
    room: &str,
    root: i64,
    result: Result<Vec<ThreadSubscription>, sea_orm::DbErr>,
) -> Option<Vec<ThreadSubscription>> {
    match result {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::warn!(
                room_id = room,
                root,
                error = %e,
                "thread subscriptions read failed; the audience falls back to the whole room"
            );
            None
        }
    }
}

/// What an append produced: the row's position, the thread it landed in, the
/// agents recorded in that thread (`None` when that read failed; empty for a
/// head row), and the choices its members recorded about it (empty for a head
/// row and for a thread nobody has chosen in yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appended {
    /// The assigned position in the room's log.
    pub seq: i64,
    /// The thread this row belongs to; `None` when it heads its own.
    pub root: Option<i64>,
    /// Agents recorded in `root`, deduplicated by the table's primary key, or
    /// `None` when the read failed -- see [`participants_or_unknown`] for why
    /// "failed" and "nobody recorded" cannot share one representation.
    pub participants: Option<Vec<MemberId>>,
    /// The thread's recorded choices ([`ThreadSubscription`] rows, raw), or
    /// `None` when the read failed -- see [`subscriptions_or_unknown`] for why
    /// "failed" and "nobody chose" cannot share one representation.
    pub subscriptions: Option<Vec<ThreadSubscription>>,
}

/// Append a room-message payload row to the room's history (the payload is
/// plaintext `RoomMessage` JSON stored opaquely -- the column name `ciphertext`
/// is a legacy shared-`Envelope` term). Returns [`Appended`]: the assigned
/// `seq`, the thread the row landed in, and everyone recorded in that thread.
/// Only the sender's stable identity (a `MemberId` + kind) is persisted; the
/// display handle is derived at read time, so none is taken here.
/// The sequence is advanced and the row inserted in the same transaction, so a
/// failed insert rolls the sequence back (no gaps in the log).
///
/// `facts` carries the thread write-path fact the payload declares (a position
/// in this room's log), resolved here rather than inferred from the payload's
/// `quote` -- a quote is a display gesture, while a thread decides which wake
/// scope a message belongs to; see [`resolve_thread_root`] for the
/// normalization, the NULL cases, and why a thread root never backfills. Its
/// mentions are already narrowed to current AGENT members by the caller (see
/// [`record_participants`] for why the store does not judge membership).
///
/// `mentioned` is that same payload list narrowed instead to every current
/// member, and it feeds [`record_mentions`]: one row per member named,
/// whatever its kind, because a mention is what pierces a mute and a human
/// can mute. A head records them like any other row -- the row states who a
/// message named, and a head naming somebody is the same fact.
#[allow(clippy::too_many_arguments)] // the write's own shape: the row, and the thread facts that ride it
pub async fn append(
    db: &Db,
    room: &str,
    sender_id: &MemberId,
    sender_kind: ParticipantKind,
    epoch: i64,
    ciphertext: &[u8],
    facts: &ThreadFacts,
    mentioned: &[MemberId],
) -> Result<Appended, sea_orm::DbErr> {
    let result = db
        .transaction::<_, (i64, Option<i64>), sea_orm::DbErr>(|txn| {
            let room = room.to_string();
            let kind = sender_kind.as_str().to_string();
            let id = sender_id.as_ref().to_string();
            let ciphertext = ciphertext.to_vec();
            let facts = facts.clone();
            let mentioned = mentioned.to_vec();
            Box::pin(async move {
                let seq = next_seq(txn, &room).await?;
                let root_seq = resolve_thread_root(txn, &room, seq, &id, facts.thread).await?;
                let now = OffsetDateTime::now_utc();
                // Same transaction as the message row below, so a thread's
                // participants exist exactly when the message that put them
                // there exists. Ordering inside the transaction is irrelevant:
                // the head this reads is an already-committed earlier row.
                if let Some(root) = root_seq {
                    record_participants(txn, &room, root, seq, &id, sender_kind, &facts).await?;
                }
                record_mentions(txn, &room, seq, &mentioned).await?;
                room_messages::ActiveModel {
                    room_id: Set(room),
                    seq: Set(seq),
                    sender_kind: Set(kind),
                    sender_id: Set(id),
                    epoch: Set(epoch),
                    ciphertext: Set(ciphertext),
                    root_seq: Set(root_seq),
                    created_at: Set(now),
                }
                .insert(txn)
                .await?;
                Ok((seq, root_seq))
            })
        })
        .await;
    // Flatten the TransactionError (the store surfaces only DbErr; there is no
    // business-rule rejection to carry as an ApiError at this layer).
    let (seq, root) = result.map_err(|e| match e {
        sea_orm::TransactionError::Connection(e) | sea_orm::TransactionError::Transaction(e) => e,
    })?;

    // The participants are read AFTER the transaction commits, and that is
    // deliberate, not an oversight:
    //
    // - a failed statement inside a Postgres transaction aborts the whole
    //   transaction, so tolerating a failed read in there would need a
    //   SAVEPOINT on every send, for an error path that must never fail the
    //   send anyway;
    // - the read sees the table as it stands WHEN IT RUNS, not as this row's
    //   own commit left it. That is the baseline every claim below is made
    //   against, and it is not the baseline the thread's future state gives:
    //   - a reply still uncommitted at this instant is MISSING from the set,
    //     and the agent that sent it is not woken for this row -- the fan of
    //     that reply, when it commits, wakes the rest;
    //   - a reply that commits between this row's commit and this read IS in
    //     the set, and its sender is woken for this row even though the row
    //     was not visible when this message landed.
    //   Both come from one source (the table at read time) and both are the
    //   same kind of lag; neither is a widening the code chose.
    // - what this read can never lose is this message's own rows: they commit
    //   with the row above, so the sender's participation is always visible.
    //   The set comes out empty when the thread holds nothing but the sender's
    //   own rows, because a sender is never in its own audience.
    //
    // A failed participant read is the other widening: it warns and hands back
    // `None`, which the audience reads as every agent being recorded here, so
    // the wake grows to the room minus whatever mute that read did return.
    // Delivery and the badge disagree inside this window, and the count is the
    // smaller number: the badge sees the thread's rows and cannot see the
    // widening. That is the direction to be wrong in -- an extra wake costs an
    // agent a re-pull, while a message it was never woken for is gone from its
    // view.
    //
    // Outside this window a send can widen, earlier and for a different reason:
    // a declared thread that resolves to no row (`resolve_thread_root`) lands
    // the message as a head, and a head wakes the room. A miss inside the
    // window is allowed to stand because the reply that was missed renders its
    // own fan when it commits (and that read sees this message's rows), and a
    // narrow wake costs an agent one re-pull, not a row.
    //
    // The thread's CHOICES are read the same way -- a second statement, right
    // after -- and they add a widening the participant table cannot: a `muted`
    // row that has not committed yet is missing from the read, the member falls
    // back to the default, and the default WAKES them. A miss can therefore
    // widen, the opposite of the participant read above; it is allowed because
    // the widened side is the DEFAULT (no choice was mistrusted) and a wake
    // costs an agent a re-pull, not a row.
    //
    // Two statements rather than one, deliberately: each table fails on its
    // own -- a failed statement aborts a Postgres transaction, so a shared one
    // could not keep `NoParticipants`, `UnreadableParticipants` and
    // `NoSubscriptions` apart -- and only the two reads together decide who
    let participants = match root {
        None => Some(Vec::new()),
        Some(root) => {
            participants_or_unknown(room, root, seq, participants_of(db, room, root).await)
        }
    };
    let subscriptions = match root {
        None => Some(Vec::new()),
        Some(root) => subscriptions_or_unknown(room, root, subscriptions_of(db, room, root).await),
    };

    Ok(Appended {
        seq,
        root,
        participants,
        subscriptions,
    })
}

/// Normalize a sender's declared thread into the value stored on the row: the
/// position of the row that heads its discussion.
///
/// A declared position names a row in THIS room. The stored value is that row's
/// own thread root when it carries one (`root_seq.unwrap_or(seq)`), so every row
/// of one discussion names the same head instead of a chain of parents.
///
/// A position this room does not hold lands as NULL, which reads as the row
/// heading its own discussion: never assigned, assigned by a DIFFERENT room (the
/// lookup is scoped by `room_id`, so a same-numbered row elsewhere cannot be
/// mistaken for this one), or not written yet. That last case is a deliberate
/// divergence from `quote`, which resolves against rows that arrive later: a
/// thread decides who gets woken, so it never backfills and a late target does
/// not move a message into a discussion after the fact. The miss is logged
/// because a NULL thread is otherwise indistinguishable from an undeclared one.
async fn resolve_thread_root(
    txn: &impl ConnectionTrait,
    room: &str,
    seq: i64,
    sender_id: &str,
    declared: Option<i64>,
) -> Result<Option<i64>, sea_orm::DbErr> {
    let Some(declared) = declared.filter(|d| *d > 0) else {
        return Ok(None);
    };
    // A primary-key point lookup, NOT a range read: `read_before(before = d + 1)`
    // returns the nearest row BELOW a missing number, which would silently attach
    // this message to the wrong discussion.
    let row = txn
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT seq, root_seq FROM room_messages WHERE room_id = $1 AND seq = $2",
            [room.into(), declared.into()],
        ))
        .await?;
    let Some(row) = row else {
        tracing::warn!(
            room_id = room,
            declared_thread = declared,
            seq,
            sender = sender_id,
            "declared thread names no row in this room; storing none"
        );
        return Ok(None);
    };
    let target_seq: i64 = row.try_get("", "seq")?;
    let target_root: Option<i64> = row.try_get("", "root_seq")?;
    Ok(Some(target_root.unwrap_or(target_seq)))
}

/// Why a member is in a thread: the `joined_via` column, and its whole domain.
///
/// The FIRST reason a member lands in a thread is the recorded one; a later
/// message does not rewrite it, so the column answers "how did it get here"
/// rather than "what has it done since".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinedVia {
    /// Authored the thread's head row.
    Started,
    /// Posted a message inside the thread.
    Replied,
    /// A message inside the thread named it.
    Mentioned,
}

impl JoinedVia {
    /// The stored label.
    pub fn as_str(self) -> &'static str {
        match self {
            JoinedVia::Started => "started",
            JoinedVia::Replied => "replied",
            JoinedVia::Mentioned => "mentioned",
        }
    }

    /// Read a stored label back. A value outside the domain is `None` rather
    /// than a default: this table is written by the store alone, so a foreign
    /// label would mean the row did not come from here.
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "started" => Some(JoinedVia::Started),
            "replied" => Some(JoinedVia::Replied),
            "mentioned" => Some(JoinedVia::Mentioned),
            _ => None,
        }
    }
}

/// One member recorded in a thread, with the reason it first joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadParticipant {
    pub participant_id: MemberId,
    pub joined_via: JoinedVia,
    pub joined_at_seq: i64,
}

/// One member's choice about one thread, as stored.
///
/// This is the RAW row, deliberately: the audience needs `since_seq` to judge
/// a message at its own position, and `muted_from_seq` to keep a mute window
/// closed after a re-follow. Collapsing these rows into a "following" set and
/// a "muted" set drops both, and delivery would then answer for a different
/// position than the badge's SQL does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSubscription {
    pub member: MemberId,
    /// The member's choice, in the wire vocabulary the routes and the CLI
    /// speak: one type for the label, here and on the surface.
    pub state: ThreadChoice,
    /// The position the current choice takes effect at: rows at or before it
    /// keep the default.
    pub since_seq: i64,
    /// The lower bound of the mute window this row remembers, if any.
    pub muted_from_seq: Option<i64>,
}

/// Record who takes part in a thread, for a message that just landed in it.
///
/// Three sources, all of them AGENTS: the head row's author (`started`, joined
/// at the head), the sender (`replied`, joined at `seq`), and the members this
/// message named (`mentioned`, joined at `seq`). A human is in none of them --
/// the table's domain is agents, so a human head or a human reply writes
/// nothing, and `facts.mentions` arrives pre-narrowed by the caller. Judging
/// membership here would mean the store reading `room_members` for a fact the
/// route already holds: one predicate, one place.
///
/// Every insert is `ON CONFLICT DO NOTHING`, so the first reason wins and
/// replaying a message is a no-op rather than a rewrite. The whole set lands in
/// ONE statement: a message's participants appear together or not at all.
async fn record_participants(
    txn: &impl ConnectionTrait,
    room: &str,
    root: i64,
    seq: i64,
    sender_id: &str,
    sender_kind: ParticipantKind,
    facts: &ThreadFacts,
) -> Result<(), sea_orm::DbErr> {
    let mut rows: Vec<(String, JoinedVia, i64)> = Vec::with_capacity(2 + facts.mentions.len());
    // The head row, not the message being appended: a head may be older than
    // the row count this transaction touches, and it is the row whose author
    // started the discussion.
    let head = txn
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT sender_id, sender_kind FROM room_messages WHERE room_id = $1 AND seq = $2",
            [room.into(), root.into()],
        ))
        .await?;
    if let Some(head) = head {
        let head_kind: String = head.try_get("", "sender_kind")?;
        if head_kind == ParticipantKind::Agent.as_str() {
            rows.push((head.try_get("", "sender_id")?, JoinedVia::Started, root));
        }
    }
    if sender_kind == ParticipantKind::Agent {
        rows.push((sender_id.to_owned(), JoinedVia::Replied, seq));
    }
    for id in &facts.mentions {
        rows.push((id.as_ref().to_owned(), JoinedVia::Mentioned, seq));
    }
    if rows.is_empty() {
        return Ok(());
    }
    // Six placeholders per row; the row count is bounded by the head, the
    // sender and `MENTIONS_MAX` mentions, so the statement stays small.
    let now = OffsetDateTime::now_utc();
    let mut sql = String::from(
        "INSERT INTO thread_participants \
         (room_id, root_seq, participant_id, joined_via, joined_at_seq, created_at) VALUES ",
    );
    let mut values: Vec<sea_orm::Value> = Vec::with_capacity(rows.len() * 6);
    for (i, (participant, via, at)) in rows.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let base = i * 6;
        sql.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6
        ));
        values.push(room.into());
        values.push(root.into());
        values.push(participant.as_str().into());
        values.push(via.as_str().into());
        values.push((*at).into());
        values.push(now.into());
    }
    sql.push_str(" ON CONFLICT (room_id, root_seq, participant_id) DO NOTHING");
    txn.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;
    Ok(())
}

/// Who one message named, one row per member, in the message's own
/// transaction.
///
/// `mentioned` is the caller's list, already narrowed to current members and
/// deduplicated by `normalize_mentions` upstream; the primary key is the
/// second guard, so a replay is a no-op rather than a rewrite. The row is
/// what pierces a mute on both sides of the delivery (see
/// [`crate::chat::audience`] and [`unread_for_member`]), so it is written for
/// every message that names somebody -- a head included, since the row states
/// a fact about the message rather than a thread.
async fn record_mentions(
    txn: &impl ConnectionTrait,
    room: &str,
    seq: i64,
    mentioned: &[MemberId],
) -> Result<(), sea_orm::DbErr> {
    if mentioned.is_empty() {
        return Ok(());
    }
    // Three placeholders per row; the count is bounded by `MENTIONS_MAX`.
    let mut sql =
        String::from("INSERT INTO room_message_mentions (room_id, seq, member_id) VALUES ");
    let mut values: Vec<sea_orm::Value> = Vec::with_capacity(mentioned.len() * 3);
    for (i, member) in mentioned.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let base = i * 3;
        sql.push_str(&format!("(${}, ${}, ${})", base + 1, base + 2, base + 3));
        values.push(room.into());
        values.push(seq.into());
        values.push(member.as_ref().into());
    }
    sql.push_str(" ON CONFLICT (room_id, seq, member_id) DO NOTHING");
    txn.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;
    Ok(())
}

/// Who is in a thread, ordered by participant id.
///
/// The only read of the table on the DELIVERY path: a thread is addressed by
/// its head position. The badge's count reads the table a second time
/// ([`unread_for_member`]), because a badge answers what reached a member and
/// that question needs the same rows; the room tests bind the two readings
/// with `the_badge_moves_exactly_for_the_members_the_audience_named`.
///
/// Rows outlive a member leaving the room, so "in this thread" is this set
/// intersected with the CURRENT membership -- the predicate does the work no
/// cleanup job would.
pub async fn participants_of(
    db: &Db,
    room: &str,
    root: i64,
) -> Result<Vec<ThreadParticipant>, sea_orm::DbErr> {
    let rows = thread_participants::Entity::find()
        .filter(thread_participants::Column::RoomId.eq(room))
        .filter(thread_participants::Column::RootSeq.eq(root))
        .order_by_asc(thread_participants::Column::ParticipantId)
        .all(db)
        .await?;
    rows.into_iter()
        .map(|r| {
            let joined_via = JoinedVia::from_label(&r.joined_via).ok_or_else(|| {
                sea_orm::DbErr::Custom(format!(
                    "thread_participants row carries unknown joined_via {:?}",
                    r.joined_via
                ))
            })?;
            Ok(ThreadParticipant {
                participant_id: MemberId::from(r.participant_id),
                joined_via,
                joined_at_seq: r.joined_at_seq,
            })
        })
        .collect()
}

/// Every choice recorded in one thread, ordered by member id.
///
/// The delivery path's second view of the choice table (the first is the
/// badge's SQL); `the_badge_moves_exactly_for_the_members_the_audience_named`
/// in the room tests binds the two.
pub async fn subscriptions_of(
    db: &Db,
    room: &str,
    root: i64,
) -> Result<Vec<ThreadSubscription>, sea_orm::DbErr> {
    let rows = thread_subscriptions::Entity::find()
        .filter(thread_subscriptions::Column::RoomId.eq(room))
        .filter(thread_subscriptions::Column::RootSeq.eq(root))
        .order_by_asc(thread_subscriptions::Column::MemberId)
        .all(db)
        .await?;
    rows.into_iter().map(subscription_row_of).collect()
}

/// One member's choice in one thread, if they made one.
pub async fn subscription_of(
    db: &Db,
    room: &str,
    root: i64,
    member: &MemberId,
) -> Result<Option<ThreadSubscription>, sea_orm::DbErr> {
    let row = thread_subscriptions::Entity::find()
        .filter(thread_subscriptions::Column::RoomId.eq(room))
        .filter(thread_subscriptions::Column::RootSeq.eq(root))
        .filter(thread_subscriptions::Column::MemberId.eq(member.as_ref().to_string()))
        .one(db)
        .await?;
    row.map(subscription_row_of).transpose()
}

fn subscription_row_of(
    row: thread_subscriptions::Model,
) -> Result<ThreadSubscription, sea_orm::DbErr> {
    let state = ThreadChoice::parse(&row.state).ok_or_else(|| {
        sea_orm::DbErr::Custom(format!(
            "thread_subscriptions row carries unknown state {:?}",
            row.state
        ))
    })?;
    Ok(ThreadSubscription {
        member: MemberId::from(row.member_id),
        state,
        since_seq: row.since_seq,
        muted_from_seq: row.muted_from_seq,
    })
}

/// Record a member's choice about a thread at the room's current position.
///
/// One statement, for two reasons. The `max(seq)` this reads and the
/// `since_seq` it writes have to come from the same snapshot -- two
/// statements would let a reply land in between and be judged on the wrong
/// side of the boundary -- and the update rules belong in the same `ON
/// CONFLICT` clause that takes the row lock:
///
/// - the first write of a choice sets both boundaries to the current position;
/// - a write that CHANGES the state moves `since_seq`, and entering `muted`
///   sets the window's lower bound with it;
/// - a write that repeats the stored state keeps `since_seq` and the bound, so
///   the row's history does not depend on how many times a button was pressed.
///   Advancing `since_seq` on a repeat would strip the coverage of whatever the
///   member followed in between (a repeat follow) or forfeit the first mute
///   window (a repeat mute);
/// - a write never moves the boundary BACKWARD. Its `max(seq)` comes from its
///   own snapshot, so a request that read the room while a newer choice was
///   committing arrives with an older position than the row holds; taking it
///   would open a mute window over rows already counted (they leave the badge
///   unread by anyone) or re-open rows a mute had already silenced. The
///   boundary becomes the further of the two, and a mute's lower bound opens
///   where the boundary then is.
///
/// `updated_at` moves on every write, repeats included: it records the write,
/// not the decision.
pub async fn set_subscription(
    db: &Db,
    room: &str,
    root: i64,
    member: &MemberId,
    state: ThreadChoice,
) -> Result<ThreadSubscription, sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO thread_subscriptions \
             (room_id, root_seq, member_id, state, since_seq, muted_from_seq, created_at, updated_at) \
             SELECT $1, $2, $3, $4::text, COALESCE(MAX(seq), 0), \
                    CASE WHEN $4::text = $5::text THEN COALESCE(MAX(seq), 0) ELSE NULL END, \
                    NOW(), NOW() \
               FROM room_messages WHERE room_id = $1 \
             ON CONFLICT (room_id, root_seq, member_id) DO UPDATE SET \
             state = EXCLUDED.state, \
             since_seq = CASE WHEN thread_subscriptions.state = EXCLUDED.state \
                              THEN thread_subscriptions.since_seq \
                              ELSE GREATEST(thread_subscriptions.since_seq, EXCLUDED.since_seq) END, \
             muted_from_seq = CASE WHEN EXCLUDED.state = $5::text \
                                    AND thread_subscriptions.state <> $5::text \
                                   THEN GREATEST(thread_subscriptions.since_seq, EXCLUDED.muted_from_seq) \
                                   ELSE thread_subscriptions.muted_from_seq END, \
             updated_at = NOW() \
             RETURNING state, since_seq, muted_from_seq",
            [
                room.into(),
                root.into(),
                member.as_ref().into(),
                state.as_str().into(),
                ThreadChoice::Muted.as_str().into(),
            ],
        ))
        .await?
        .ok_or_else(|| {
            sea_orm::DbErr::Custom("thread_subscriptions upsert returned no row".into())
        })?;
    let stored = row.try_get::<String>("", "state")?;
    let stored = ThreadChoice::parse(&stored).ok_or_else(|| {
        sea_orm::DbErr::Custom(format!(
            "thread_subscriptions stored unknown state {stored:?}"
        ))
    })?;
    Ok(ThreadSubscription {
        member: member.clone(),
        state: stored,
        since_seq: row.try_get("", "since_seq")?,
        muted_from_seq: row.try_get("", "muted_from_seq")?,
    })
}

/// Drop a member's choice: the thread goes back to the default.
///
/// Idempotent -- a row that is not there is not an error -- and it forgets the
/// mute window with the row, which is the documented difference between "back
/// to the default" and "undo the mute": the default counts the rows the mute
/// left out, because that is what the default has always done.
pub async fn clear_subscription(
    db: &Db,
    room: &str,
    root: i64,
    member: &MemberId,
) -> Result<(), sea_orm::DbErr> {
    thread_subscriptions::Entity::delete_many()
        .filter(thread_subscriptions::Column::RoomId.eq(room))
        .filter(thread_subscriptions::Column::RootSeq.eq(root))
        .filter(thread_subscriptions::Column::MemberId.eq(member.as_ref().to_string()))
        .exec(db)
        .await?;
    Ok(())
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
    Ok(rows.into_iter().map(stored_of).collect())
}

/// Read one discussion forward: the head row plus every reply that names it,
/// `seq > after_seq`, ascending, up to `limit`.
///
/// A row names its discussion head in `root_seq` (NULL on the head itself), so
/// the predicate is `root_seq = thread OR (seq = thread AND root_seq IS NULL)`;
/// the bare `root_seq = thread` would drop the head row, and the head is the
/// opening message the panel renders.
///
/// Forward-only on purpose: the one reader of a discussion pages from the
/// room's start, and nothing pages a discussion backwards. A backward variant
/// would be a path with no caller today; add it when a caller appears.
///
/// A `thread` that names a REPLY rather than a head matches no row: every row
/// stores the head's seq in `root_seq` (see `resolve_thread_root`), so a reply
/// is never that value, and it carries a `root_seq` of its own. The empty page
/// is the honest answer: this reader does not reinterpret a position as a
/// different discussion, and it answers a position past the last row the same
/// way. Whether the discussion exists is the caller's question -- it opened it
/// from a row it already holds, and a head the room never had is the only way
/// to ask for nothing.
pub async fn read_thread(
    db: &Db,
    room: &str,
    thread: i64,
    after_seq: i64,
    limit: u64,
) -> Result<Vec<StoredMessage>, sea_orm::DbErr> {
    let rows = room_messages::Entity::find()
        .filter(room_messages::Column::RoomId.eq(room))
        .filter(room_messages::Column::Seq.gt(after_seq))
        .filter(
            Condition::any()
                .add(room_messages::Column::RootSeq.eq(thread))
                .add(
                    Condition::all()
                        .add(room_messages::Column::Seq.eq(thread))
                        .add(room_messages::Column::RootSeq.is_null()),
                ),
        )
        .order_by_asc(room_messages::Column::Seq)
        .limit(limit)
        .all(db)
        .await?;
    Ok(rows.into_iter().map(stored_of).collect())
}

/// Does `root` head a thread in `room`? A subscription key names a HEAD: the
/// row at `seq = root` carrying no `root_seq`. A reply, a position the log
/// never reached and a seq from another room all answer No, which is why the
/// write routes refuse them instead of storing a key nothing matches.
pub async fn is_thread_head(db: &Db, room: &str, root: i64) -> Result<bool, sea_orm::DbErr> {
    let row = room_messages::Entity::find()
        .filter(room_messages::Column::RoomId.eq(room))
        .filter(room_messages::Column::Seq.eq(root))
        .filter(room_messages::Column::RootSeq.is_null())
        .one(db)
        .await?;
    Ok(row.is_some())
}

/// Map one stored room-message row to the transport-neutral shape. Shared by
/// every reader, so none of them can drift apart.
fn stored_of(r: room_messages::Model) -> StoredMessage {
    StoredMessage {
        seq: r.seq,
        sender_id: MemberId::from(r.sender_id),
        sender_kind: ParticipantKind::from_label(&r.sender_kind).unwrap_or(ParticipantKind::Agent),
        epoch: r.epoch,
        ciphertext: r.ciphertext,
        root_seq: r.root_seq,
        created_at: r.created_at,
    }
}

/// Read the newest `limit` rows with `seq < before_seq`, returned ASCENDING
/// (the page's oldest row first), so a caller renders a page the same way in
/// both directions. The backward pager, and the single-row lookup behind a
/// quote: `read_before(before_seq = seq + 1, limit = 1)` is the one row a
/// quoted seq can mean.
///
/// Absence is decidable on this route: the log has no gaps (see [`append`] --
/// the sequence advances and the row inserts in one transaction, so a failed
/// insert rolls the sequence back). A caller may therefore compare the
/// returned seq with the wanted one and read a mismatch (or an empty page) as
/// "this room has that row nowhere", rather than as a failed fetch. That
/// verdict speaks for the log as it stands at this read and not forever: a
/// seq the room reaches later starts resolving, so the absence answer must
/// not be cached past the reply it was composed for.
pub async fn read_before(
    db: &Db,
    room: &str,
    before_seq: i64,
    limit: u64,
) -> Result<Vec<StoredMessage>, sea_orm::DbErr> {
    let mut rows = room_messages::Entity::find()
        .filter(room_messages::Column::RoomId.eq(room))
        .filter(room_messages::Column::Seq.lt(before_seq))
        .order_by_desc(room_messages::Column::Seq)
        .limit(limit)
        .all(db)
        .await?;
    // The SQL page is newest-first (that is what `limit` must cut); the
    // caller renders oldest-first, like every other history read.
    rows.reverse();
    Ok(rows.into_iter().map(stored_of).collect())
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
/// for the archeion `/internal/room-membership` RPC + TTL cache: one SQL read on
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

/// Clamp-advance a member's read cursor in a room. The
/// `INSERT ... ON CONFLICT DO UPDATE ... GREATEST` is a single statement: the
/// row lock the upsert takes serializes concurrent writers, and `GREATEST`
/// makes the watermark monotonic -- a stale write (a lagging device replaying
/// an old cursor) can never move it backwards. Returns the watermark as
/// stored (the clamped value).
pub async fn set_read_cursor(
    db: &Db,
    room: &str,
    member: &MemberId,
    last_read_seq: i64,
) -> Result<i64, sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO room_read_cursors (room_id, member_id, last_read_seq, updated_at) \
             VALUES ($1, $2, $3, NOW()) \
             ON CONFLICT (room_id, member_id) DO UPDATE SET \
             last_read_seq = GREATEST(room_read_cursors.last_read_seq, EXCLUDED.last_read_seq), \
             updated_at = NOW() \
             RETURNING last_read_seq",
            [room.into(), member.as_ref().into(), last_read_seq.into()],
        ))
        .await?
        .ok_or_else(|| sea_orm::DbErr::Custom("room_read_cursors upsert returned no row".into()))?;
    row.try_get("", "last_read_seq")
}

/// Read one member's cursors for a batch of rooms (the room-list read).
/// Rooms with no cursor row are simply absent from the map; the caller treats
/// absence as 0. One indexed scan on `idx_room_read_cursors_member`.
pub async fn read_cursors_for_member(
    db: &Db,
    member: &MemberId,
    room_ids: &[String],
) -> Result<HashMap<String, i64>, sea_orm::DbErr> {
    if room_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = room_read_cursors::Entity::find()
        .filter(room_read_cursors::Column::MemberId.eq(member.as_ref().to_string()))
        .filter(room_read_cursors::Column::RoomId.is_in(room_ids.to_vec()))
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.room_id, r.last_read_seq))
        .collect())
}

/// A member's badge in one room, with the watermark it was taken against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoomUnread {
    /// Rows past the cursor that reached the member, capped at 100. Display
    /// caps lower, so the cap is never the number a person sees.
    pub unread: i64,
    /// The member's stored cursor, `0` when they have no row.
    pub cursor: i64,
}

/// The member's badge in one room: how many rows past their cursor reached
/// them, plus the cursor that count was taken against. Both numbers come from
/// one statement, so the count and the watermark cannot disagree.
///
/// A row counts when all four hold: it is past the member's cursor, its sender
/// is not the member, the member's choice about its thread includes it, and --
/// when the choice is the default -- it reached the member: a head row
/// (`root_seq IS NULL`) reached the whole room and counts, and a row inside a
/// thread counts when the member is a human (humans never enter the participant
/// table and are woken by every reply) or when a participant row joins them to
/// that thread at or before the row's `seq`.
///
/// A mention outranks the choice, in one direction only: a row that names the
/// member counts even when their choice is a mute (`room_message_mentions`,
/// one row per (message, member)). The mute still silences every other row of
/// the thread, and a mention never narrows a row that counted anyway.
///
/// The choice is judged at the ROW's position, never at now: `following` counts
/// every reply after its `since_seq`, a mute counts nothing after its
/// `muted_from_seq`, and rows before the choice fall through to the default.
/// That is what keeps a mute from taking back unread already counted, and a
/// re-follow from resurrecting what the mute never delivered.
///
/// The badge counts what the fan-out delivers, so an agent's badge leaves out
/// threads it never joined and a human's counts every row; a badge that stayed
/// equal would be counting rows the agent was never sent.
///
/// The count is recomputed from the tables as they stand now, not from the
/// audience the rows were delivered to, so the two agree only while nothing
/// moves in between. Four ways they can differ:
///
/// - `seq` is assigned inside the transaction that stores the row, so two
///   concurrent replies can commit out of `seq` order. Inside the window
///   [`append`] documents, a row is read before a lower one commits, and the
///   count then disagrees with that delivery in either direction.
/// - A read that fails widens delivery, and this count cannot see it: a failed
///   choice read falls back to the whole room
///   ([`crate::chat::audience::Fallback::NoSubscriptions`]), and a failed
///   participant read treats every agent as recorded
///   ([`crate::chat::audience::Fallback::UnreadableParticipants`]). A read
///   that succeeds but is early widens too: a `muted` row it missed leaves the
///   member on the default, and the default wakes them.
/// - A member removed and added back keeps both their participant rows and
///   their cursor, so the rows of their absence count as reached. This one
///   leaves no log line, and it rests on exactly the rows an exit would clean.
/// - A member with no cursor row starts at 0 and counts the room's whole
///   history, including what was sent before they joined. No log line either.
///
/// The first two are deviations of the delivery window. The last two are
/// choices about membership history: keep an absence in the count, count a
/// room from its start. Any leaving-cleanup must revisit this function and
/// its callers in one change.
///
/// The count stops at 100 rows. The `LIMIT 100` subquery bounds the number
/// returned, not the work: proving there is no 101st row still walks every
/// row of the unread span through the `(room_id, seq)` scan, with each thread
/// row resolved through the `thread_participants` primary key, up to three
/// probes on `thread_subscriptions`, and one index probe on
/// `room_message_mentions`. The cursor
/// advances as the member reads, so the span is the rows they have not seen
/// yet; a member who never opened the room walks all of it on every
/// envelope. The display caps lower than the cap.
pub async fn unread_for_member(
    db: &Db,
    room: &str,
    member: &MemberId,
) -> Result<RoomUnread, sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "WITH cur AS ( \
                 SELECT COALESCE((SELECT last_read_seq FROM room_read_cursors \
                                   WHERE room_id = $1 AND member_id = $2), 0) AS last_read_seq, \
                        EXISTS (SELECT 1 FROM room_members me \
                                 WHERE me.room_id = $1 \
                                   AND me.member_id = $2 \
                                   AND me.kind = 'human') AS is_human \
             ) \
             SELECT cur.last_read_seq AS cursor, \
                    (SELECT count(*) FROM (\
                     SELECT 1 FROM room_messages m \
                      WHERE m.room_id = $1 \
                        AND m.seq > cur.last_read_seq \
                        AND m.sender_id <> $2 \
                        AND (m.root_seq IS NULL \
                             OR EXISTS (SELECT 1 FROM thread_subscriptions s \
                                         WHERE s.room_id = m.room_id \
                                           AND s.root_seq = m.root_seq \
                                           AND s.member_id = $2 \
                                           AND s.state = $3 \
                                           AND s.since_seq < m.seq) \
                             OR ((cur.is_human \
                                  OR EXISTS (SELECT 1 FROM thread_participants p \
                                              WHERE p.room_id = m.room_id \
                                                AND p.root_seq = m.root_seq \
                                                AND p.participant_id = $2 \
                                                AND p.joined_at_seq <= m.seq)) \
                                 AND NOT EXISTS (SELECT 1 FROM thread_subscriptions s \
                                                  WHERE s.room_id = m.room_id \
                                                    AND s.root_seq = m.root_seq \
                                                    AND s.member_id = $2 \
                                                    AND s.since_seq < m.seq) \
                                 AND NOT EXISTS (SELECT 1 FROM thread_subscriptions s \
                                                  WHERE s.room_id = m.room_id \
                                                    AND s.root_seq = m.root_seq \
                                                    AND s.member_id = $2 \
                                                    AND s.muted_from_seq IS NOT NULL \
                                                    AND s.muted_from_seq < m.seq)) \
                             OR EXISTS (SELECT 1 FROM room_message_mentions mm \
                                         WHERE mm.room_id = m.room_id \
                                           AND mm.seq = m.seq \
                                           AND mm.member_id = $2)) \
                     LIMIT 100) AS capped) AS unread \
             FROM cur",
            [
                room.into(),
                member.as_ref().into(),
                ThreadChoice::Following.as_str().into(),
            ],
        ))
        .await?
        .ok_or_else(|| sea_orm::DbErr::Custom("room unread read returned no row".into()))?;
    Ok(RoomUnread {
        unread: row.try_get("", "unread")?,
        cursor: row.try_get("", "cursor")?,
    })
}

#[cfg(test)]
mod tests {
    //! Append + read round-trip against ephemeral Postgres: the per-room
    //! sequence is race-free under concurrent appends and the sender
    //! `Participant` survives the column split/reconstruct.

    use super::*;

    /// Provision a fresh, migrated database on the shared test Postgres
    /// (see `crate::test_support::provision_test_db`).
    async fn fresh_db() -> Db {
        crate::test_support::provision_test_db().await
    }

    /// A test sender's stable identity (no display handle -- the store no longer
    /// persists one).
    fn user(handle: &str) -> (MemberId, ParticipantKind) {
        (
            MemberId::from(format!("u-{handle}")),
            ParticipantKind::Human,
        )
    }

    /// The write-path facts a test append declares: a thread position, and
    /// nobody named.
    fn declaring(thread: Option<i64>) -> ThreadFacts {
        ThreadFacts {
            thread,
            mentions: Vec::new(),
        }
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

    /// `append` hands back [`Appended`]; these tests were written against the
    /// bare position it used to return, so they take the `seq` and keep their
    /// subject (the log, not the audience).
    async fn append_seq(
        db: &Db,
        room: &str,
        sender_id: &MemberId,
        sender_kind: ParticipantKind,
        epoch: i64,
        ciphertext: &[u8],
        facts: &ThreadFacts,
    ) -> Result<i64, sea_orm::DbErr> {
        append(
            db,
            room,
            sender_id,
            sender_kind,
            epoch,
            ciphertext,
            facts,
            &[],
        )
        .await
        .map(|a| a.seq)
    }

    #[tokio::test]
    async fn append_assigns_monotonic_seq_and_reads_back() {
        let db = fresh_db().await;
        seed_room(&db, "room-1").await;
        let (alice_pid, human) = user("alice");
        let (tagma_pid, agent_kind) = agent();
        let s1 = append_seq(
            &db,
            "room-1",
            &alice_pid,
            human,
            1,
            b"ct-1",
            &ThreadFacts::default(),
        )
        .await
        .unwrap();
        let s2 = append_seq(
            &db,
            "room-1",
            &tagma_pid,
            agent_kind,
            1,
            b"ct-2",
            &ThreadFacts::default(),
        )
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
        append_seq(
            &db,
            "room-1",
            &alice_pid,
            human,
            2,
            b"ct-3",
            &ThreadFacts::default(),
        )
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

    /// The thread a row lands with: an undeclared send heads its own
    /// discussion (NULL, read as its own `seq`), a declared head resolves to
    /// that row, a declared REPLY resolves to the SAME head, and anything the
    /// room cannot point at lands NULL rather than guessing.
    #[tokio::test]
    async fn a_declared_thread_resolves_to_the_rows_head() {
        let db = fresh_db().await;
        seed_room(&db, "room-t").await;
        seed_room(&db, "room-other").await;
        let (alice, human) = user("alice");

        // 1: undeclared -- the head of its own discussion.
        let head = append_seq(
            &db,
            "room-t",
            &alice,
            human,
            1,
            b"head",
            &ThreadFacts::default(),
        )
        .await
        .unwrap();
        // 2: declares the head; stores the head's position.
        append_seq(
            &db,
            "room-t",
            &alice,
            human,
            1,
            b"reply",
            &declaring(Some(head)),
        )
        .await
        .unwrap();
        // 3: declares row 2, which already names row 1 -- folds to row 1.
        append_seq(
            &db,
            "room-t",
            &alice,
            human,
            1,
            b"reply to reply",
            &declaring(Some(2)),
        )
        .await
        .unwrap();
        // 4: declares a position this room never assigned.
        append_seq(
            &db,
            "room-t",
            &alice,
            human,
            1,
            b"dangling",
            &declaring(Some(99)),
        )
        .await
        .unwrap();
        // 5: declares position 10, which only ANOTHER room holds -- the other
        // room really has ten rows, so dropping the room scope would find one.
        for _ in 0..10 {
            append_seq(
                &db,
                "room-other",
                &alice,
                human,
                1,
                b"o",
                &ThreadFacts::default(),
            )
            .await
            .unwrap();
        }
        append_seq(
            &db,
            "room-t",
            &alice,
            human,
            1,
            b"cross",
            &declaring(Some(10)),
        )
        .await
        .unwrap();
        // 6-7: no position is a declaration -- zero and a negative number both
        // read as undeclared and reach no lookup.
        append_seq(
            &db,
            "room-t",
            &alice,
            human,
            1,
            b"zero",
            &declaring(Some(0)),
        )
        .await
        .unwrap();
        append_seq(
            &db,
            "room-t",
            &alice,
            human,
            1,
            b"negative",
            &declaring(Some(-5)),
        )
        .await
        .unwrap();

        let rows = read_since(&db, "room-t", 0, 100).await.unwrap();
        let roots: Vec<Option<i64>> = rows.iter().map(|r| r.root_seq).collect();
        assert_eq!(
            roots,
            vec![None, Some(1), Some(1), None, None, None, None],
            "undeclared and unresolvable rows head themselves; a reply names its head"
        );
    }

    /// The unresolvable case leaves a trace: a NULL thread otherwise looks
    /// exactly like an undeclared one, so the miss names the room, the declared
    /// position and the row that failed to resolve it.
    #[tokio::test]
    async fn an_unresolvable_thread_names_itself_in_a_warning() {
        use std::io::Write as _;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Sink;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let db = fresh_db().await;
        seed_room(&db, "room-log").await;
        let (alice, human) = user("alice");
        append_seq(
            &db,
            "room-log",
            &alice,
            human,
            1,
            b"dangling",
            &declaring(Some(42)),
        )
        .await
        .unwrap();
        let _ = std::io::stdout().flush();

        let logged = String::from_utf8_lossy(&sink.0.lock().unwrap()).to_string();
        assert!(
            logged.contains("declared thread names no row"),
            "the miss is logged: {logged}"
        );
        assert!(
            logged.contains("declared_thread=42"),
            "the declared position: {logged}"
        );
        assert!(
            logged.contains("room_id=\"room-log\""),
            "the room: {logged}"
        );
    }

    /// Sequences are independent per room (each starts at 1).
    #[tokio::test]
    async fn seq_is_per_room() {
        let db = fresh_db().await;
        seed_room(&db, "room-a").await;
        seed_room(&db, "room-b").await;
        let (alice, human) = user("alice");
        let a = append_seq(
            &db,
            "room-a",
            &alice,
            human,
            1,
            b"a1",
            &ThreadFacts::default(),
        )
        .await
        .unwrap();
        let b = append_seq(
            &db,
            "room-b",
            &alice,
            human,
            1,
            b"b1",
            &ThreadFacts::default(),
        )
        .await
        .unwrap();
        let a2 = append_seq(
            &db,
            "room-a",
            &alice,
            human,
            2,
            b"a2",
            &ThreadFacts::default(),
        )
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
        let db = fresh_db().await;
        seed_room(&db, "room-c").await;
        let db = std::sync::Arc::new(db);
        let (alice, human) = user("alice");
        let alice = std::sync::Arc::new(alice);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let db = db.clone();
            let alice = alice.clone();
            handles.push(tokio::spawn(async move {
                append_seq(
                    &db,
                    "room-c",
                    &alice,
                    human,
                    1,
                    b"x",
                    &ThreadFacts::default(),
                )
                .await
                .unwrap()
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
        let db = fresh_db().await;
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
                append_seq(
                    &db,
                    "room-seed",
                    &alice,
                    human,
                    1,
                    b"s",
                    &ThreadFacts::default(),
                )
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
        let db = fresh_db().await;
        seed_room(&db, "room-gone").await;
        let (alice, human) = user("alice");
        append_seq(
            &db,
            "room-gone",
            &alice,
            human,
            1,
            b"ct-1",
            &ThreadFacts::default(),
        )
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

    /// A stale cursor write (a lagging device replaying an old watermark)
    /// never moves the cursor backwards: `set_read_cursor` clamps on
    /// `GREATEST`, so the stored watermark is the max ever written.
    #[tokio::test]
    async fn set_read_cursor_clamps_stale_writes() {
        let db = fresh_db().await;
        seed_room(&db, "room-cursor").await;
        let (alice, _) = user("alice");
        let v = set_read_cursor(&db, "room-cursor", &alice, 5)
            .await
            .unwrap();
        assert_eq!(v, 5);
        // A stale write (older watermark) is a no-op...
        let v = set_read_cursor(&db, "room-cursor", &alice, 3)
            .await
            .unwrap();
        assert_eq!(v, 5, "a stale write must not move the watermark back");
        // ...and a fresh write still advances it.
        let v = set_read_cursor(&db, "room-cursor", &alice, 7)
            .await
            .unwrap();
        assert_eq!(v, 7);
    }

    /// Concurrent writes to the same (room, member) cursor serialize on the
    /// upsert row lock and land on the max -- no lost update, no PK-violation
    /// loser. (4 tasks to stay under the default pool size under
    /// full-suite parallel-test container load.)
    #[tokio::test]
    async fn concurrent_set_read_cursor_lands_on_max() {
        let db = fresh_db().await;
        seed_room(&db, "room-race").await;
        let db = std::sync::Arc::new(db);
        let (alice, _) = user("alice");
        let alice = std::sync::Arc::new(alice);
        let mut handles = Vec::new();
        for seq in [3i64, 7, 5, 9] {
            let db = db.clone();
            let alice = alice.clone();
            handles.push(tokio::spawn(async move {
                set_read_cursor(&db, "room-race", &alice, seq)
                    .await
                    .unwrap()
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let stored = read_cursors_for_member(&db, &alice, &["room-race".to_string()])
            .await
            .unwrap();
        assert_eq!(stored.get("room-race"), Some(&9), "max wins under race");
    }

    /// The batch cursor read is scoped to ONE member and ONLY the requested
    /// rooms; rooms with no cursor row are absent from the map (the caller
    /// reads absence as 0).
    #[tokio::test]
    async fn read_cursors_for_member_scopes_to_member_and_rooms() {
        let db = fresh_db().await;
        seed_room(&db, "room-a").await;
        seed_room(&db, "room-b").await;
        seed_room(&db, "room-c").await;
        let (alice, _) = user("alice");
        let (bob, _) = user("bob");
        set_read_cursor(&db, "room-a", &alice, 4).await.unwrap();
        set_read_cursor(&db, "room-b", &alice, 2).await.unwrap();
        set_read_cursor(&db, "room-a", &bob, 6).await.unwrap();
        let rooms = vec![
            "room-a".to_string(),
            "room-b".to_string(),
            "room-c".to_string(),
        ];
        let alice_map = read_cursors_for_member(&db, &alice, &rooms).await.unwrap();
        assert_eq!(alice_map.len(), 2);
        assert_eq!(alice_map.get("room-a"), Some(&4));
        assert_eq!(alice_map.get("room-b"), Some(&2));
        assert!(!alice_map.contains_key("room-c"), "no row = absent, not 0");
        // Bob's cursors are independent of Alice's.
        let bob_map = read_cursors_for_member(&db, &bob, &["room-a".to_string()])
            .await
            .unwrap();
        assert_eq!(bob_map.get("room-a"), Some(&6));
    }

    /// The backward pager cuts the NEWEST `limit` rows below the cursor and
    /// returns them ascending, so a page renders the same way in both
    /// directions; `seq < before_seq` is exclusive and the directions never
    /// overlap.
    #[tokio::test]
    async fn read_before_pages_backwards_from_the_cursor() {
        let db = fresh_db().await;
        seed_room(&db, "room-back").await;
        let (alice, human) = user("alice");
        for i in 1..=5 {
            append_seq(
                &db,
                "room-back",
                &alice,
                human,
                1,
                format!("ct-{i}").as_bytes(),
                &ThreadFacts::default(),
            )
            .await
            .unwrap();
        }

        // Newest three below the end of the log, oldest row of the page first.
        let page = read_before(&db, "room-back", 6, 3).await.unwrap();
        let seqs: Vec<i64> = page.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![3, 4, 5]);
        assert_eq!(page[0].ciphertext, b"ct-3");

        // Exclusive: before_seq = 3 stops before row 3.
        let older = read_before(&db, "room-back", 3, 10).await.unwrap();
        let seqs: Vec<i64> = older.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![1, 2]);

        // before_seq = 1 opens an empty page: no row 0 exists.
        assert!(
            read_before(&db, "room-back", 1, 10)
                .await
                .unwrap()
                .is_empty()
        );

        // The forward pager resumes strictly after the page, ascending.
        let forward = read_since(&db, "room-back", 3, 10).await.unwrap();
        let seqs: Vec<i64> = forward.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![4, 5]);
    }

    /// The single-row lookup a quote resolves with:
    /// `read_before(before_seq = seq + 1, limit = 1)`. The page holds the
    /// wanted row only when it is non-empty AND its seq equals the wanted seq;
    /// the log has no gaps (see [`append`] -- the sequence advances and the row
    /// inserts in one transaction, so a failed insert rolls the sequence back),
    /// which is what makes a mismatch the "this room has that row nowhere"
    /// verdict rather than a failed fetch.
    #[tokio::test]
    async fn read_before_finds_one_row_or_none() {
        let db = fresh_db().await;
        seed_room(&db, "room-peek").await;
        let (alice, human) = user("alice");
        for i in 1..=2 {
            append_seq(
                &db,
                "room-peek",
                &alice,
                human,
                1,
                format!("c{i}").as_bytes(),
                &ThreadFacts::default(),
            )
            .await
            .unwrap();
        }

        // The wanted row exists: seq + 1 lands exactly on it.
        let peek = read_before(&db, "room-peek", 3, 1).await.unwrap();
        assert_eq!(peek.len(), 1);
        assert_eq!(peek[0].seq, 2);
        assert_eq!(peek[0].ciphertext, b"c2");

        // A seq past the end: the page is still the newest row BELOW the
        // cursor, so it comes back as a different row -- that mismatch (not an
        // empty page, on a room that has rows) is the absence verdict.
        let beyond = read_before(&db, "room-peek", 100, 1).await.unwrap();
        assert_eq!(beyond.len(), 1);
        assert_eq!(beyond[0].seq, 2, "the page falls back to the newest row");

        // An empty room has nothing to fall back to: there the empty page is
        // the absence verdict.
        seed_room(&db, "room-empty").await;
        assert!(
            read_before(&db, "room-empty", 1, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// One discussion forward: the head row (stored with no `root_seq`) and
    /// every reply naming it, in seq order, cut by `after_seq` and `limit` like
    /// any other page. The room's other rows never appear.
    #[tokio::test]
    async fn read_thread_returns_the_head_and_its_replies() {
        let db = fresh_db().await;
        seed_room(&db, "room-t").await;
        let (alice, human) = user("alice");

        // 1: head, 2: unrelated, 3: reply to 1, 4: reply to the reply (still
        // 1's discussion), 5: unrelated, 6: another reply to 1.
        for facts in [
            declaring(None),
            declaring(None),
            declaring(Some(1)),
            declaring(Some(3)),
            declaring(None),
            declaring(Some(1)),
        ] {
            append_seq(&db, "room-t", &alice, human, 1, b"ct", &facts)
                .await
                .unwrap();
        }

        let page = read_thread(&db, "room-t", 1, 0, 10).await.unwrap();
        let seqs: Vec<i64> = page.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![1, 3, 4, 6]);
        assert_eq!(page[0].root_seq, None, "the head is stored without a root");
        assert_eq!(
            page[3].root_seq,
            Some(1),
            "a reply to a reply names the head"
        );

        // after_seq cuts inside the discussion, ascending.
        let rest = read_thread(&db, "room-t", 1, 3, 10).await.unwrap();
        let seqs: Vec<i64> = rest.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![4, 6]);

        // limit cuts the page, oldest row first.
        let first = read_thread(&db, "room-t", 1, 0, 2).await.unwrap();
        let seqs: Vec<i64> = first.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![1, 3]);

        // A reply's seq is not a head: no row names it, so the page is empty.
        assert!(
            read_thread(&db, "room-t", 3, 0, 10)
                .await
                .unwrap()
                .is_empty()
        );

        // A discussion this room does not hold reads empty.
        assert!(
            read_thread(&db, "room-t", 99, 0, 10)
                .await
                .unwrap()
                .is_empty()
        );

        // The filter is (room, discussion), not the discussion alone: a head
        // numbered in another room stays out of this page.
        seed_room(&db, "room-u").await;
        append_seq(&db, "room-u", &alice, human, 1, b"other", &declaring(None))
            .await
            .unwrap();
        append_seq(
            &db,
            "room-u",
            &alice,
            human,
            1,
            b"other",
            &declaring(Some(1)),
        )
        .await
        .unwrap();
        let page = read_thread(&db, "room-t", 1, 0, 10).await.unwrap();
        let seqs: Vec<i64> = page.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![1, 3, 4, 6]);
    }

    /// The discussion read's index exists after migrations, and it is the
    /// partial one the read relies on: `WHERE root_seq IS NOT NULL` leaves head
    /// rows out, since the primary key already finds them. The statement is raw
    /// SQL (sea-query carries no partial predicate), so a constructor-shaped
    /// rewrite would silently widen the index and nothing else would notice.
    #[tokio::test]
    async fn the_thread_index_is_partial_on_named_roots() {
        let db = fresh_db().await;
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT indexdef FROM pg_indexes \
                 WHERE indexname = 'idx_room_messages_thread_seq'",
            ))
            .await
            .expect("the catalog answers")
            .expect("the migration created the thread index");
        let def: String = row.try_get("", "indexdef").expect("indexdef is text");
        assert!(
            def.contains("(room_id, root_seq, seq)"),
            "the index keys are the read's: {def}"
        );
        assert!(
            def.contains("root_seq IS NOT NULL"),
            "head rows are left out of the index: {def}"
        );
    }

    fn agent_of(tag: &str) -> (MemberId, ParticipantKind) {
        (MemberId::from(format!("t-{tag}")), ParticipantKind::Agent)
    }

    /// The facts of a send that declares a thread position and names members.
    fn naming(thread: i64, mentions: &[&MemberId]) -> ThreadFacts {
        ThreadFacts {
            thread: Some(thread),
            mentions: mentions.iter().map(|m| (*m).clone()).collect(),
        }
    }

    /// A thread's participants as `(member, reason, joined_at)`, sorted so the
    /// assertion does not depend on row order.
    async fn thread_rows(db: &Db, room: &str, root: i64) -> Vec<(String, &'static str, i64)> {
        let mut rows: Vec<(String, &'static str, i64)> = participants_of(db, room, root)
            .await
            .unwrap()
            .into_iter()
            .map(|p| {
                (
                    p.participant_id.as_ref().to_string(),
                    p.joined_via.as_str(),
                    p.joined_at_seq,
                )
            })
            .collect();
        rows.sort();
        rows
    }

    /// The three ways a member joins a thread, recorded with the reason and the
    /// position that put them there: the head's author as `started` (at the
    /// head, not at the reply), the sender as `replied`, the named member as
    /// `mentioned`. A message that joins no thread records nothing.
    #[tokio::test]
    async fn a_thread_records_started_replied_and_mentioned() {
        let db = fresh_db().await;
        seed_room(&db, "room-p").await;
        let (a, agent_kind) = agent_of("a");
        let (b, _) = agent_of("b");
        let (c, _) = agent_of("c");

        // 1: an undeclared send heads its own thread and records nobody.
        let head = append_seq(&db, "room-p", &a, agent_kind, 1, b"head", &declaring(None))
            .await
            .unwrap();
        assert!(thread_rows(&db, "room-p", head).await.is_empty());

        // 2: B replies in that thread and names C.
        append_seq(
            &db,
            "room-p",
            &b,
            agent_kind,
            1,
            b"reply",
            &naming(head, &[&c]),
        )
        .await
        .unwrap();
        assert_eq!(
            thread_rows(&db, "room-p", head).await,
            vec![
                (a.as_ref().to_string(), "started", head),
                (b.as_ref().to_string(), "replied", head + 1),
                (c.as_ref().to_string(), "mentioned", head + 1),
            ]
        );

        // A position that heads nothing is not a thread: nothing to record.
        assert!(thread_rows(&db, "room-p", head + 5).await.is_empty());
    }

    /// The first reason wins: a member named in one message and replying later
    /// stays `mentioned`, and a second mention does not rewrite the row.
    #[tokio::test]
    async fn a_member_keeps_the_first_reason_it_joined() {
        let db = fresh_db().await;
        seed_room(&db, "room-first").await;
        let (a, agent_kind) = agent_of("a");
        let (b, _) = agent_of("b");
        let (c, _) = agent_of("c");

        let head = append_seq(&db, "room-first", &a, agent_kind, 1, b"h", &declaring(None))
            .await
            .unwrap();
        append_seq(
            &db,
            "room-first",
            &b,
            agent_kind,
            1,
            b"m",
            &naming(head, &[&c]),
        )
        .await
        .unwrap();
        // C replies AND is named again; B names C once more.
        append_seq(
            &db,
            "room-first",
            &c,
            agent_kind,
            1,
            b"c-reply",
            &naming(head, &[]),
        )
        .await
        .unwrap();
        append_seq(
            &db,
            "room-first",
            &b,
            agent_kind,
            1,
            b"again",
            &naming(head, &[&c]),
        )
        .await
        .unwrap();

        let rows = thread_rows(&db, "room-first", head).await;
        assert_eq!(
            rows,
            vec![
                (a.as_ref().to_string(), "started", head),
                (b.as_ref().to_string(), "replied", head + 1),
                (c.as_ref().to_string(), "mentioned", head + 1),
            ],
            "one row per member, holding the reason it first landed"
        );
    }

    /// The table's domain is agents: a human head author is not `started`, and a
    /// human reply records nothing at all. The agent on the other side of the
    /// same thread is still recorded.
    #[tokio::test]
    async fn humans_never_enter_the_participant_table() {
        let db = fresh_db().await;
        seed_room(&db, "room-h").await;
        let (alice, human) = user("alice");
        let (a, agent_kind) = agent_of("a");

        let head = append_seq(&db, "room-h", &alice, human, 1, b"h", &declaring(None))
            .await
            .unwrap();
        append_seq(&db, "room-h", &a, agent_kind, 1, b"r", &naming(head, &[]))
            .await
            .unwrap();
        // Alice replies inside the same thread: no row for her.
        append_seq(&db, "room-h", &alice, human, 1, b"r2", &naming(head, &[]))
            .await
            .unwrap();

        assert_eq!(
            thread_rows(&db, "room-h", head).await,
            vec![(a.as_ref().to_string(), "replied", head + 1)],
            "a human head or human reply is outside the table's domain"
        );
    }
    /// Two shapes the guard has to hold. A message that declares no thread
    /// records nothing even when it names members: there is no head to key a
    /// row on, and a mention outside a thread has nowhere to belong. And a
    /// sender naming itself inside a thread lands as one row, not as a
    /// duplicate-key error from the multi-row statement.
    #[tokio::test]
    async fn a_mention_outside_a_thread_records_nothing() {
        let db = fresh_db().await;
        seed_room(&db, "room-loose").await;
        let (a, agent_kind) = agent_of("a");
        let (b, _) = agent_of("b");

        let loose = append_seq(
            &db,
            "room-loose",
            &a,
            agent_kind,
            1,
            b"loose",
            &ThreadFacts {
                thread: None,
                mentions: vec![b.clone(), a.clone()],
            },
        )
        .await
        .unwrap();
        assert!(
            thread_rows(&db, "room-loose", loose).await.is_empty(),
            "no thread, no rows, mentions included"
        );

        // The same send inside a thread, naming itself: one row for the sender,
        // and the statement survives its own duplicate key.
        let head = append_seq(&db, "room-loose", &a, agent_kind, 1, b"h", &declaring(None))
            .await
            .unwrap();
        append_seq(
            &db,
            "room-loose",
            &b,
            agent_kind,
            1,
            b"self",
            &naming(head, &[&b]),
        )
        .await
        .unwrap();
        assert_eq!(
            thread_rows(&db, "room-loose", head).await,
            vec![
                (a.as_ref().to_string(), "started", head),
                (b.as_ref().to_string(), "replied", head + 1),
            ],
            "a self-mention adds no second row"
        );
    }

    /// The write cost is bounded by what the message itself carries: the head,
    /// the sender, and the mention list at its cap.
    #[tokio::test]
    async fn a_full_mention_list_stays_within_the_row_bound() {
        let db = fresh_db().await;
        seed_room(&db, "room-cap").await;
        let (a, agent_kind) = agent_of("a");
        let named: Vec<MemberId> = (0..kallipai_lesche_common::message::MENTIONS_MAX)
            .map(|i| MemberId::from(format!("t-cap-{i}")))
            .collect();
        let refs: Vec<&MemberId> = named.iter().collect();

        // A DIFFERENT agent heads the thread, so the head's author and the
        // sender are two rows: the bound counts them separately.
        let (head_author, _) = agent_of("head");
        let head = append_seq(
            &db,
            "room-cap",
            &head_author,
            agent_kind,
            1,
            b"h",
            &declaring(None),
        )
        .await
        .unwrap();
        append_seq(
            &db,
            "room-cap",
            &a,
            agent_kind,
            1,
            b"m",
            &naming(head, &refs),
        )
        .await
        .unwrap();

        let rows = participants_of(&db, "room-cap", head).await.unwrap();
        assert_eq!(
            rows.len(),
            2 + kallipai_lesche_common::message::MENTIONS_MAX,
            "the head's author, the sender, and one row per named member"
        );
    }

    /// What an append hands back for a live-delivery decision: the position,
    /// the thread, and the thread's recorded agents -- the head author included,
    /// because a reply is delivered to the person whose thread it is.
    #[tokio::test]
    async fn an_append_returns_the_thread_and_its_agents() {
        let db = fresh_db().await;
        seed_room(&db, "room-aud").await;
        let (a, agent_kind) = agent_of("a");
        let (b, _) = agent_of("b");

        let head = append(
            &db,
            "room-aud",
            &a,
            agent_kind,
            1,
            b"h",
            &declaring(None),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(head.root, None, "a head row heads its own thread");
        assert!(
            head.participants
                .as_ref()
                .is_some_and(|rows| rows.is_empty()),
            "a head has no participants to scope its own wake"
        );

        let reply = append(
            &db,
            "room-aud",
            &b,
            agent_kind,
            1,
            b"r",
            &naming(head.seq, &[]),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(reply.root, Some(head.seq));
        let mut got: Vec<String> = reply
            .participants
            .as_ref()
            .expect("the participant read succeeded")
            .iter()
            .map(|m| m.as_ref().to_string())
            .collect();
        got.sort();
        assert_eq!(got, vec![a.as_ref().to_string(), b.as_ref().to_string()]);
    }

    /// A mention lands as its own row, in the message's own transaction: the
    /// audience and the badge both ask it per message, and neither parses a
    /// payload to answer.
    #[tokio::test]
    async fn an_append_records_who_the_message_named() {
        let db = fresh_db().await;
        seed_room(&db, "room-men").await;
        let (a, agent_kind) = agent_of("a");
        let (b, _) = agent_of("b");

        let head = append(
            &db,
            "room-men",
            &a,
            agent_kind,
            1,
            b"h",
            &declaring(None),
            &[],
        )
        .await
        .unwrap();
        let reply = append(
            &db,
            "room-men",
            &a,
            agent_kind,
            1,
            b"r",
            &naming(head.seq, &[&b]),
            std::slice::from_ref(&b),
        )
        .await
        .unwrap();
        let rows = db
            .query_all(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT member_id FROM room_message_mentions WHERE room_id = $1 AND seq = $2",
                ["room-men".into(), reply.seq.into()],
            ))
            .await
            .unwrap();
        let got: Vec<String> = rows
            .iter()
            .map(|row| row.try_get::<String>("", "member_id").unwrap())
            .collect();
        assert_eq!(got, vec![b.as_ref().to_string()]);
    }

    /// A failed participant read is not a failed send: the policy hands back
    /// `None`, which the audience reads as every agent being recorded, and
    /// the warning carries the reason.
    #[test]
    fn a_failed_participant_read_is_not_an_empty_one() {
        let got = participants_or_unknown(
            "room-x",
            7,
            9,
            Err(sea_orm::DbErr::Custom("read exploded".to_string())),
        );
        assert!(got.is_none());
    }

    /// Seed one `room_members` row: the badge's human check reads the member's
    /// kind from the local membership graph, which the message rows do not
    /// carry.
    async fn seed_member(db: &Db, room: &str, member: &MemberId, kind: ParticipantKind) {
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO room_members (room_id, member_id, kind, source_id, joined_at, added_by) \
             VALUES ($1, $2, $3, $4, NOW(), 'creator')",
            [
                room.into(),
                member.as_ref().into(),
                kind.as_str().into(),
                format!("src-{}", member.as_ref()).into(),
            ],
        ))
        .await
        .expect("seed member");
    }

    /// The badge counts rows past the cursor from someone else, with the cursor
    /// the count was taken against (0 for a member with no row).
    #[tokio::test]
    async fn unread_counts_rows_past_the_cursor_from_others() {
        let db = fresh_db().await;
        seed_room(&db, "room-u").await;
        let (a, agent_kind) = agent_of("a");
        let (b, _) = agent_of("b");
        for n in 1..=3 {
            append_seq(
                &db,
                "room-u",
                &a,
                agent_kind,
                1,
                format!("a-{n}").as_bytes(),
                &ThreadFacts::default(),
            )
            .await
            .unwrap();
        }
        append_seq(
            &db,
            "room-u",
            &b,
            agent_kind,
            1,
            b"b-1",
            &ThreadFacts::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            unread_for_member(&db, "room-u", &b).await.unwrap(),
            RoomUnread {
                unread: 3,
                cursor: 0
            }
        );
        // The member's own rows never count.
        assert_eq!(
            unread_for_member(&db, "room-u", &a).await.unwrap(),
            RoomUnread {
                unread: 1,
                cursor: 0
            }
        );

        set_read_cursor(&db, "room-u", &b, 2).await.unwrap();
        // Past the cursor: seq 3 and seq 4; seq 4 is B's own row.
        assert_eq!(
            unread_for_member(&db, "room-u", &b).await.unwrap(),
            RoomUnread {
                unread: 1,
                cursor: 2
            }
        );
    }

    /// The count stops at the cap: 101 rows past the cursor answer 100, a
    /// number the surface then caps lower still.
    #[tokio::test]
    async fn unread_stops_at_a_hundred_rows() {
        let db = fresh_db().await;
        seed_room(&db, "room-cap").await;
        let (a, agent_kind) = agent_of("a");
        let (b, _) = agent_of("b");
        for n in 1..=101 {
            append_seq(
                &db,
                "room-cap",
                &a,
                agent_kind,
                1,
                format!("a-{n}").as_bytes(),
                &ThreadFacts::default(),
            )
            .await
            .unwrap();
        }
        assert_eq!(
            unread_for_member(&db, "room-cap", &b).await.unwrap(),
            RoomUnread {
                unread: 100,
                cursor: 0
            }
        );
    }

    /// The badge counts what reached the member: the threads they are in, the
    /// head rows everyone got, and for a human every row.
    #[tokio::test]
    async fn unread_follows_who_reached_the_member() {
        let db = fresh_db().await;
        seed_room(&db, "room-th").await;
        let (a, agent_kind) = agent_of("a");
        let (b, _) = agent_of("b");
        let (c, _) = agent_of("c");
        let (h, human_kind) = user("h");

        // 1 A heads a thread, 2 B replies in it, 3 B heads a second thread,
        // 4 B replies there and names C. A is a participant of the first
        // thread only; B started and replied in both.
        let first = append_seq(
            &db,
            "room-th",
            &a,
            agent_kind,
            1,
            b"head-1",
            &declaring(None),
        )
        .await
        .unwrap();
        append_seq(
            &db,
            "room-th",
            &b,
            agent_kind,
            1,
            b"reply-1",
            &declaring(Some(first)),
        )
        .await
        .unwrap();
        let second = append_seq(
            &db,
            "room-th",
            &b,
            agent_kind,
            1,
            b"head-2",
            &declaring(None),
        )
        .await
        .unwrap();
        append_seq(
            &db,
            "room-th",
            &b,
            agent_kind,
            1,
            b"reply-2",
            &naming(second, &[&c]),
        )
        .await
        .unwrap();

        // A is in the first thread only: of B's three rows, seq 2 reached them
        // and the head at seq 3 arrived as a head row; seq 4 did not reach them.
        assert_eq!(
            unread_for_member(&db, "room-th", &a).await.unwrap().unread,
            2
        );
        // B's own rows never count; A's head at seq 1 reached everyone.
        assert_eq!(
            unread_for_member(&db, "room-th", &b).await.unwrap().unread,
            1
        );
        // C is named in the second thread: both heads plus that reply, not the
        // first thread's reply.
        assert_eq!(
            unread_for_member(&db, "room-th", &c).await.unwrap().unread,
            3
        );
        // A human counts every row: nothing narrows their audience, and they
        // never appear in the participant table.
        seed_member(&db, "room-th", &h, human_kind).await;
        assert_eq!(
            unread_for_member(&db, "room-th", &h).await.unwrap().unread,
            4
        );
    }

    /// The participant row's position is the whole of "this thread is mine from
    /// here": a member named late counts from the row that named them, not from the
    /// thread's head. Without the `joined_at_seq <= m.seq` term this member would
    /// read three rows here instead of one, so this test is what keeps the term
    /// alive.
    #[tokio::test]
    async fn unread_starts_at_the_participant_row_not_at_the_thread() {
        let db = fresh_db().await;
        seed_room(&db, "room-late").await;
        let (a, agent_kind) = agent_of("a");
        let (c, _) = agent_of("c");

        // 1 A heads a thread; 2 and 3 A replies naming nobody; 4 A replies and
        // names C. C has read the head, so only the naming row is past the cursor.
        let head = append_seq(
            &db,
            "room-late",
            &a,
            agent_kind,
            1,
            b"head",
            &declaring(None),
        )
        .await
        .unwrap();
        append_seq(
            &db,
            "room-late",
            &a,
            agent_kind,
            1,
            b"r-2",
            &declaring(Some(head)),
        )
        .await
        .unwrap();
        append_seq(
            &db,
            "room-late",
            &a,
            agent_kind,
            1,
            b"r-3",
            &declaring(Some(head)),
        )
        .await
        .unwrap();
        append_seq(
            &db,
            "room-late",
            &a,
            agent_kind,
            1,
            b"r-4",
            &naming(head, &[&c]),
        )
        .await
        .unwrap();
        set_read_cursor(&db, "room-late", &c, head).await.unwrap();

        assert_eq!(
            unread_for_member(&db, "room-late", &c).await.unwrap(),
            RoomUnread {
                unread: 1,
                cursor: head
            }
        );
    }

    /// Repeating a click is not a new decision: the second write of the same
    /// state leaves both boundaries where the first one put them, which is
    /// what keeps a repeated follow from stripping the coverage in between.
    #[tokio::test]
    async fn repeating_a_choice_leaves_both_boundaries_alone() {
        let db = fresh_db().await;
        seed_room(&db, "room-subs").await;
        let (a, agent_kind) = agent();
        let (c, _) = user("carol");
        let head = append_seq(
            &db,
            "room-subs",
            &a,
            agent_kind,
            1,
            b"head",
            &ThreadFacts::default(),
        )
        .await
        .unwrap();
        let first = set_subscription(&db, "room-subs", head, &c, ThreadChoice::Muted)
            .await
            .unwrap();
        assert_eq!(first.since_seq, head);
        assert_eq!(first.muted_from_seq, Some(head));
        append_seq(
            &db,
            "room-subs",
            &a,
            agent_kind,
            1,
            b"reply",
            &declaring(Some(head)),
        )
        .await
        .unwrap();
        let again = set_subscription(&db, "room-subs", head, &c, ThreadChoice::Muted)
            .await
            .unwrap();
        assert_eq!(again.since_seq, first.since_seq);
        assert_eq!(again.muted_from_seq, first.muted_from_seq);
    }

    /// A re-follow moves the start of the row and keeps the mute bound, so a
    /// reply muted inside the window stays out of the badge. The judgement is
    /// read back through `audience::choice`, the function delivery uses: one
    /// row, one answer, whichever side asks.
    #[tokio::test]
    async fn a_re_follow_keeps_the_mute_bound_and_moves_the_start() {
        let db = fresh_db().await;
        seed_room(&db, "room-window").await;
        let (a, agent_kind) = agent();
        let (c, _) = user("carol");
        let head = append_seq(
            &db,
            "room-window",
            &a,
            agent_kind,
            1,
            b"head",
            &ThreadFacts::default(),
        )
        .await
        .unwrap();
        set_subscription(&db, "room-window", head, &c, ThreadChoice::Muted)
            .await
            .unwrap();
        let inside = append_seq(
            &db,
            "room-window",
            &a,
            agent_kind,
            1,
            b"inside the mute",
            &declaring(Some(head)),
        )
        .await
        .unwrap();
        let followed = set_subscription(&db, "room-window", head, &c, ThreadChoice::Following)
            .await
            .unwrap();
        assert_eq!(followed.since_seq, inside);
        assert_eq!(followed.muted_from_seq, Some(head));
        let rows = subscriptions_of(&db, "room-window", head).await.unwrap();
        assert_eq!(
            crate::chat::audience::choice(&rows, &c, inside),
            Some(ThreadChoice::Muted)
        );
        assert_eq!(
            crate::chat::audience::choice(&rows, &c, inside + 1),
            Some(ThreadChoice::Following)
        );
    }

    /// The badge and the fan agree on a mute end to end: a muted human stops
    /// counting, a re-follow restarts the count after the window, and dropping
    /// the row returns the member to the default the mute had replaced.
    #[tokio::test]
    async fn the_badge_follows_a_mute_a_re_follow_and_a_clear() {
        let db = fresh_db().await;
        seed_room(&db, "room-badge").await;
        let (a, agent_kind) = agent();
        let (c, _) = user("carol");
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO room_members (room_id, member_id, kind, source_id, joined_at, \
             added_by) VALUES ($1, $2, 'human', $2, NOW(), $2)",
            ["room-badge".into(), c.as_ref().into()],
        ))
        .await
        .expect("seed carol as a human member");
        let head = append_seq(
            &db,
            "room-badge",
            &a,
            agent_kind,
            1,
            b"head",
            &ThreadFacts::default(),
        )
        .await
        .unwrap();
        let second = append_seq(
            &db,
            "room-badge",
            &a,
            agent_kind,
            1,
            b"reply",
            &declaring(Some(head)),
        )
        .await
        .unwrap();
        set_read_cursor(&db, "room-badge", &c, second)
            .await
            .unwrap();
        set_subscription(&db, "room-badge", head, &c, ThreadChoice::Muted)
            .await
            .unwrap();
        append_seq(
            &db,
            "room-badge",
            &a,
            agent_kind,
            1,
            b"muted reply",
            &declaring(Some(head)),
        )
        .await
        .unwrap();
        let muted = unread_for_member(&db, "room-badge", &c).await.unwrap();
        assert_eq!(muted.unread, 0);
        set_subscription(&db, "room-badge", head, &c, ThreadChoice::Following)
            .await
            .unwrap();
        append_seq(
            &db,
            "room-badge",
            &a,
            agent_kind,
            1,
            b"followed reply",
            &declaring(Some(head)),
        )
        .await
        .unwrap();
        let followed = unread_for_member(&db, "room-badge", &c).await.unwrap();
        assert_eq!(followed.unread, 1);
        clear_subscription(&db, "room-badge", head, &c)
            .await
            .unwrap();
        let cleared = unread_for_member(&db, "room-badge", &c).await.unwrap();
        assert_eq!(cleared.unread, 2);
        assert_eq!(cleared.cursor, second);
        clear_subscription(&db, "room-badge", head, &c)
            .await
            .unwrap();
    }

    /// Insert a choice row directly, standing in for one a newer write
    /// committed while this write's snapshot was still being read.
    async fn seed_choice(
        db: &Db,
        room: &str,
        root: i64,
        member: &MemberId,
        state: ThreadChoice,
        since_seq: i64,
        muted_from_seq: Option<i64>,
    ) {
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO thread_subscriptions \
             (room_id, root_seq, member_id, state, since_seq, muted_from_seq, \
              created_at, updated_at) \
             VALUES ($1, $2, $3, $4::text, $5, $6, NOW(), NOW())",
            [
                room.into(),
                root.into(),
                member.as_ref().to_string().into(),
                state.as_str().into(),
                since_seq.into(),
                muted_from_seq.into(),
            ],
        ))
        .await
        .expect("seed a choice row");
    }

    /// The CHECKs catch the writes the upsert cannot produce: a mute with
    /// no bound, a bound past the row's own start, and a state no reader
    /// can name.
    #[tokio::test]
    async fn the_checks_reject_a_row_that_drifts() {
        let db = fresh_db().await;
        seed_room(&db, "room-checks").await;
        for (state, bound) in [
            ("muted", None),
            ("following", Some(9i64)),
            ("napping", None),
        ] {
            let drifted = db
                .execute(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "INSERT INTO thread_subscriptions \
                     (room_id, root_seq, member_id, state, since_seq, muted_from_seq, \
                      created_at, updated_at) \
                     VALUES ($1, $2, $3, $4::text, $5, $6, NOW(), NOW())",
                    [
                        "room-checks".into(),
                        1i64.into(),
                        "u-carol".into(),
                        state.into(),
                        5i64.into(),
                        bound.into(),
                    ],
                ))
                .await;
            assert!(
                drifted.is_err(),
                "a row written as {state} with bound {bound:?} must violate a check"
            );
        }
    }

    /// A write whose snapshot predates the row keeps the further boundary.
    ///
    /// The row holds a position this statement cannot see -- one a newer choice
    /// committed while this request was reading the room -- so its own
    /// `max(seq)` is the smaller number. Taking it would judge the rows between
    /// the two positions by the wrong rule: a mute window opened over them
    /// drops them from the badge unread by anyone, and a re-follow re-opens
    /// rows a mute had already silenced.
    #[tokio::test]
    async fn a_write_whose_snapshot_predates_the_row_keeps_the_further_boundary() {
        let db = fresh_db().await;
        seed_room(&db, "room-race").await;
        let (alice, human) = user("alice");
        let head = append_seq(
            &db,
            "room-race",
            &alice,
            human,
            1,
            b"head",
            &ThreadFacts::default(),
        )
        .await
        .unwrap();
        let end = append_seq(
            &db,
            "room-race",
            &alice,
            human,
            1,
            b"reply",
            &declaring(Some(head)),
        )
        .await
        .unwrap();
        let ahead = end + 20;

        // A newer choice committed `following` past this write's snapshot.
        let stale_mute = MemberId::from("u-stale-mute".to_string());
        seed_choice(
            &db,
            "room-race",
            head,
            &stale_mute,
            ThreadChoice::Following,
            ahead,
            None,
        )
        .await;
        let muted = set_subscription(&db, "room-race", head, &stale_mute, ThreadChoice::Muted)
            .await
            .unwrap();
        assert_eq!(muted.state, ThreadChoice::Muted);
        assert_eq!(
            muted.since_seq, ahead,
            "the boundary keeps the further position"
        );
        assert_eq!(
            muted.muted_from_seq,
            Some(ahead),
            "the mute window opens where the boundary is"
        );

        // The same the other way: a stale re-follow keeps the window closed.
        let stale_follow = MemberId::from("u-stale-follow".to_string());
        seed_choice(
            &db,
            "room-race",
            head,
            &stale_follow,
            ThreadChoice::Muted,
            ahead,
            Some(ahead),
        )
        .await;
        let followed = set_subscription(
            &db,
            "room-race",
            head,
            &stale_follow,
            ThreadChoice::Following,
        )
        .await
        .unwrap();
        assert_eq!(followed.state, ThreadChoice::Following);
        assert_eq!(followed.since_seq, ahead);
        assert_eq!(
            followed.muted_from_seq,
            Some(ahead),
            "the window the row remembers stays closed"
        );

        // A boundary behind the room's end still moves up to it.
        let plain = MemberId::from("u-plain".to_string());
        seed_choice(
            &db,
            "room-race",
            head,
            &plain,
            ThreadChoice::Following,
            head,
            None,
        )
        .await;
        let advanced = set_subscription(&db, "room-race", head, &plain, ThreadChoice::Muted)
            .await
            .unwrap();
        assert_eq!(
            advanced.since_seq, end,
            "the boundary follows the room's end forward"
        );
    }
}
