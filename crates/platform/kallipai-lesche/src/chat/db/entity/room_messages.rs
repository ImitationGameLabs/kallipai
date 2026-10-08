//! `room_messages` entity -- one payload row in a room's append-only history.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "room_messages")]
pub struct Model {
    /// `RoomId`. Composite-PK half.
    #[sea_orm(primary_key, column_type = "Text")]
    pub room_id: String,
    /// Per-room sequence number. Composite-PK half; assigned by the store from
    /// `room_message_seq`.
    #[sea_orm(primary_key)]
    pub seq: i64,
    /// `ParticipantKind` variant tag: `human` or `agent`.
    #[sea_orm(column_type = "Text")]
    pub sender_kind: String,
    /// The sender's derived member id (a `ParticipantId` UUID -- the opaque
    /// room-layer v5 uuid, NOT the underlying user_id/tagma_id). This is the
    /// row's STABLE identity; the display handle is derived at read time from the
    /// registry (see `member_identity`), never persisted.
    #[sea_orm(column_type = "Text")]
    pub sender_id: String,
    /// Membership epoch at send time (the relay's cache marker).
    pub epoch: i64,
    /// The room payload: plaintext `RoomMessage` JSON bytes, stored opaquely.
    /// The lesche is the room's store of record and is trusted to read content.
    pub ciphertext: Vec<u8>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
