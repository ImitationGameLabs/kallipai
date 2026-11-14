//! `thread_participants` -- who takes part in a thread, and why they joined it.
//! Introduced 2026-11-14 for the thread notification scope (batch 2 of the
//! thread work: this batch only records and judges, it does not route on it).
//!
//! One row per (room, thread head, agent): the head position is the stored
//! `room_messages.root_seq` every row of that discussion names, so a thread is
//! one value here if and only if it is one value in the log.
//!
//! `joined_via` records WHY a member is in the thread -- `started` (it authored
//! the head), `replied` (it posted inside the thread) or `mentioned` (a message
//! inside the thread named it). First reason wins and is never rewritten: a
//! member who replied after being mentioned keeps the earlier fact, so the
//! column answers "how did it first get here" rather than "what did it do".
//!
//! Two columns are deliberately absent. There is no `kind`: the table records
//! agents only (a human reply does not belong here, and a nullable or always-
//! `agent` column would invite a later writer to put a user in it). There is no
//! reverse index (`participant_id, ...`): the only read this batch has is
//! "everyone in this thread", which the primary key already serves.
//!
//! No deletion path. Rows survive a member leaving the room; readers intersect
//! with the CURRENT membership, so the predicate ("who is in this thread") holds
//! without a cleanup job.
//!
//! This batch wrote no history: the table starts counting from the first
//! message appended after it exists. Batch 3 backfills the window between the
//! `root_seq` column and this table, once, from `room_messages` itself (see
//! `m_20261114_02_thread_participants_backfill`), because delivery now reads
//! the table and a missing row is a missing wake.
//!
//! A thread nobody has replied to is the EMPTY SET here, and that is a real
//! answer, not missing data: the head author's `started` row is written by the
//! first reply, not by the head itself, so a head and its silence look the same
//! on purpose.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ThreadParticipants::Table)
                    .col(ColumnDef::new(ThreadParticipants::RoomId).text().not_null())
                    .col(
                        ColumnDef::new(ThreadParticipants::RootSeq)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ThreadParticipants::ParticipantId)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ThreadParticipants::JoinedVia)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ThreadParticipants::JoinedAtSeq)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ThreadParticipants::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(ThreadParticipants::RoomId)
                            .col(ThreadParticipants::RootSeq)
                            .col(ThreadParticipants::ParticipantId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_thread_participants_room")
                            .from(ThreadParticipants::Table, ThreadParticipants::RoomId)
                            .to(Rooms::Table, Rooms::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ThreadParticipants::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ThreadParticipants {
    Table,
    RoomId,
    RootSeq,
    ParticipantId,
    JoinedVia,
    JoinedAtSeq,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Rooms {
    Table,
    Id,
}
