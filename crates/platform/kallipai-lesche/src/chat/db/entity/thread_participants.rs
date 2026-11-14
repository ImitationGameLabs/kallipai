//! `thread_participants` entity -- who takes part in a thread, and why.
//!
//! One row per (room, thread head, agent). `root_seq` is the head position the
//! whole discussion names (`room_messages.root_seq` normalized, with the head's
//! own NULL read as its `seq`), and `joined_via` is the first reason the member
//! landed here. The table carries agents only, which is why there is no `kind`
//! column: a human reply never inserts a row.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "thread_participants")]
pub struct Model {
    /// `RoomId`. Composite-PK half; references `rooms(id)` (`ON DELETE CASCADE`).
    #[sea_orm(primary_key, column_type = "Text")]
    pub room_id: String,
    /// The `seq` of the row that heads this thread: `root_seq.unwrap_or(seq)` of
    /// any message in it. Composite-PK half.
    #[sea_orm(primary_key)]
    pub root_seq: i64,
    /// The agent's `ParticipantId` (canonical hyphenated text, the same value
    /// `room_messages.sender_id` carries). Composite-PK half.
    #[sea_orm(primary_key, column_type = "Text")]
    pub participant_id: String,
    /// How the member first joined the thread: `started` (authored the head),
    /// `replied` (posted a message inside it) or `mentioned` (a message inside
    /// it named them). First reason wins and is never rewritten.
    #[sea_orm(column_type = "Text")]
    pub joined_via: String,
    /// The `seq` of the message that put this member in the thread.
    pub joined_at_seq: i64,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
