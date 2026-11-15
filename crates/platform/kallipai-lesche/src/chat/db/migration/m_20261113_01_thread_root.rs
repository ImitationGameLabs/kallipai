//! `room_messages.root_seq` -- the discussion a row belongs to (its thread root).
//! Introduced 2026-11-13 for the thread notification scope.
//!
//! Nullable and never backfilled: a NULL `root_seq` READS as the row heading its
//! own discussion (readers use `root_seq.unwrap_or(seq)`), so historical rows
//! need no rewrite and this stays a metadata-only addition.
//!
//! A batch that lists one thread asks for
//! `root_seq = X OR (seq = X AND root_seq IS NULL)`: the head row stores NULL,
//! so a predicate of just `root_seq = X` silently drops the head itself.
//!
//! No index here: the only read that resolves a discussion is the primary-key
//! lookup of the row a sender declared, which the composite `(room_id, seq)`
//! key already scopes.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum RoomMessages {
    Table,
    RootSeq,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RoomMessages::Table)
                    .add_column(ColumnDef::new(RoomMessages::RootSeq).big_integer().null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RoomMessages::Table)
                    .drop_column(RoomMessages::RootSeq)
                    .to_owned(),
            )
            .await
    }
}
