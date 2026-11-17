//! `room_message_mentions` -- which members ONE message names.
//!
//! A mention pierces a muting choice: a member who muted a thread withdrew from
//! the discussion, not from being addressed inside it, so delivery wakes them
//! and the badge counts the row (see `chat::audience` and
//! `store::unread_for_member`). Both sides ask the same question, "did THIS
//! message name me", and neither can answer it from the payload: the mention
//! list lives inside `room_messages.ciphertext` (the `RoomMessage` JSON), so a
//! per-row answer would parse JSON bytes for every candidate row of a badge
//! count -- slow, and wrong as soon as one row's payload is unreadable.
//!
//! One row per (room, message, member) answers it with an index probe instead.
//! Rows land in the same transaction as the message they belong to, so a
//! mention exists exactly when the message carrying it exists, and a message
//! that names nobody writes nothing.
//!
//! Two access paths, both in use from the start: the primary key orders
//! (room, seq, member), which is what an insert and a by-message read have in
//! hand, and `(room_id, member_id)` serves the badge, which starts from the
//! member and the unread span and never knows the seq values in advance.
//!
//! No `kind` column: a human can be named and a human can mute, and the route
//! narrows the payload's list to current members before it gets here. No
//! backfill either: rows stored before this table existed carry no entry, their
//! delivery is already history, and an old payload's mention list is not a
//! trusted source of @ semantics. Nothing is lost by starting here.
//!
//! The foreign key is the room, matching `thread_participants`: a room's
//! deletion cascades, and messages are never deleted one by one.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(RoomMessageMentions::Table)
                    .col(
                        ColumnDef::new(RoomMessageMentions::RoomId)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomMessageMentions::Seq)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomMessageMentions::MemberId)
                            .text()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(RoomMessageMentions::RoomId)
                            .col(RoomMessageMentions::Seq)
                            .col(RoomMessageMentions::MemberId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_room_message_mentions_room")
                            .from(RoomMessageMentions::Table, RoomMessageMentions::RoomId)
                            .to(Rooms::Table, Rooms::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("room_message_mentions_member")
                    .table(RoomMessageMentions::Table)
                    .col(RoomMessageMentions::RoomId)
                    .col(RoomMessageMentions::MemberId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(RoomMessageMentions::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum RoomMessageMentions {
    Table,
    RoomId,
    Seq,
    MemberId,
}

#[derive(DeriveIden)]
enum Rooms {
    Table,
    Id,
}
