//! `direct_messages` entity -- one payload row in a direct session's
//! append-only history.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "direct_messages")]
pub struct Model {
    /// `DirectSessionId`. Composite-PK half; cascades with the session.
    #[sea_orm(primary_key, column_type = "Text")]
    pub session_id: String,
    /// Per-session sequence number. Composite-PK half; assigned by the store
    /// from `direct_message_seq`.
    #[sea_orm(primary_key)]
    pub seq: i64,
    /// The sender's tagma id (the stable identity; direct members ARE tagmas,
    /// so unlike the room rows there is no kind column and no member-id
    /// indirection). The display handle is derived at read time from the
    /// registry, never persisted.
    #[sea_orm(column_type = "Text")]
    pub sender: String,
    /// The payload: plaintext `DirectMessage` JSON bytes, stored opaquely.
    /// The lesche is the store of record and is trusted to read content.
    pub payload: Vec<u8>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
