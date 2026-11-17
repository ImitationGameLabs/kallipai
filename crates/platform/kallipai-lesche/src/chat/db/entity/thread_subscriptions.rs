//! `thread_subscriptions` entity -- one member's choice about one thread.
//!
//! One row per (room, thread head, member). The row records a POSITION, not a
//! flag: `since_seq` is where the current choice takes effect, and
//! `muted_from_seq` is the lower bound of the mute window the row still
//! remembers after a re-follow. The absence of a row is the third answer -- the
//! default -- which is why `state` has no "default" variant and why the table
//! carries no row for a member who never touched the thread.
//!
//! Three invariants are checked in the database itself (see
//! `m_20261117_01_thread_subscriptions`), not just maintained here: a mute
//! row carries a lower bound equal to its `since_seq`, no bound lies past
//! `since_seq`, and `state` holds `following` or `muted` and nothing else.
//!
//! The write path (`store::set_subscription`) changes `state` only when the
//! choice itself CHANGES, so a repeated click leaves both boundaries where
//! the first click put them and the first window of a mute survives it.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "thread_subscriptions")]
pub struct Model {
    /// `RoomId`. Composite-PK half; references `rooms(id)` (`ON DELETE CASCADE`).
    #[sea_orm(primary_key, column_type = "Text")]
    pub room_id: String,
    /// The `seq` of the row that heads this discussion: the value
    /// `room_messages.root_seq` carries. Composite-PK half.
    #[sea_orm(primary_key)]
    pub root_seq: i64,
    /// The member's `ParticipantId` (canonical hyphenated text, the same value
    /// `thread_participants.participant_id` carries). Composite-PK half.
    #[sea_orm(primary_key, column_type = "Text")]
    pub member_id: String,
    /// `following` or `muted`. The absence of a row is the default; see the
    /// module docs.
    #[sea_orm(column_type = "Text")]
    pub state: String,
    /// The position the current choice takes effect at: the room's `max(seq)`
    /// when the choice was written. Rows at or before it keep the default.
    pub since_seq: i64,
    /// The lower bound of the mute window this row remembers: equal to
    /// `since_seq` on a mute row, kept across a re-follow, NULL when the member
    /// never muted the thread.
    pub muted_from_seq: Option<i64>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
