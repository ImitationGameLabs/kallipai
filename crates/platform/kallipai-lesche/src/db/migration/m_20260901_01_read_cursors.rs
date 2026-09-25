//! `room_read_cursors` -- a member's per-room read watermark (the unread
//! backbone). One row per (room, member); rows persist across membership
//! cascade away with the room. `last_read_seq` is a clamp-on-write watermark:
//! a stale write never moves it backwards (see the store's single-statement
//! upsert). Introduced 2026-09-01 for the unread-badge feature.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(RoomReadCursors::Table)
                    .col(ColumnDef::new(RoomReadCursors::RoomId).text().not_null())
                    .col(ColumnDef::new(RoomReadCursors::MemberId).text().not_null())
                    // The member's read watermark: every message with
                    // `seq <= last_read_seq` is read. Clamp semantics live in
                    // the store's upsert, not here.
                    .col(
                        ColumnDef::new(RoomReadCursors::LastReadSeq)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomReadCursors::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(RoomReadCursors::RoomId)
                            .col(RoomReadCursors::MemberId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_room_read_cursors_room")
                            .from(RoomReadCursors::Table, RoomReadCursors::RoomId)
                            .to(Rooms::Table, Rooms::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        // Standalone btree on `member_id`: the composite PK `(room_id,
        // member_id)` does not serve queries that filter on `member_id` alone
        // (no btree skip-scan), and the room-list read does exactly that --
        // the caller's cursors across all their rooms in one indexed scan
        // (same shape as `idx_room_members_member`).
        manager
            .create_index(
                Index::create()
                    .name("idx_room_read_cursors_member")
                    .table(RoomReadCursors::Table)
                    .col(RoomReadCursors::MemberId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(RoomReadCursors::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum RoomReadCursors {
    Table,
    RoomId,
    MemberId,
    LastReadSeq,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum Rooms {
    Table,
    Id,
}
