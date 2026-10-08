//! `direct_sessions` / `direct_messages` / `direct_read_cursors` (+ the
//! `direct_message_seq` counter): the direct-session domain -- server-side
//! plaintext 1v1 chat between exactly two tagma agents.
//!
//! Deliberately a SEPARATE table family from `rooms*`: a direct session has
//! fixed, immutable membership (two `member_a`/`member_b` columns, no member
//! table), no invites, no visibility, no membership epoch -- carrying the
//! rooms columns would mean schema-level fields the domain never uses. The
//! mechanics mirror the room store: append-only message log with a per-session
//! monotonic sequence (race-free upsert counter), clamp-on-write read
//! cursors, and cascade delete with the session row.
//!
//! `member_a`/`member_b` store the canonical byte-ordered pair (the same
//! ordering the session-id derivation hashes -- see
//! `kallipai_lesche_common::direct`), so the pair is unique by construction and
//! the UNIQUE constraint documents it. Like `room_members.member_id`, member
//! columns are plain TEXT references (no FK to the archeion registry); the only
//! FKs are internal-to-lesche cascade links.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum DirectSessions {
    Table,
    Id,
    MemberA,
    MemberB,
    CreatedAt,
}

#[derive(DeriveIden)]
enum DirectMessageSeq {
    Table,
    SessionId,
    NextSeq,
}

#[derive(DeriveIden)]
enum DirectMessages {
    Table,
    SessionId,
    Seq,
    Sender,
    Payload,
    CreatedAt,
}

#[derive(DeriveIden)]
enum DirectReadCursors {
    Table,
    SessionId,
    Member,
    LastReadSeq,
    UpdatedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(DirectSessions::Table)
                    .col(
                        ColumnDef::new(DirectSessions::Id)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(DirectSessions::MemberA).text().not_null())
                    .col(ColumnDef::new(DirectSessions::MemberB).text().not_null())
                    .col(
                        ColumnDef::new(DirectSessions::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    // The derived id already implies pair uniqueness (same pair
                    // -> same id); the constraint documents it at the schema
                    // level and backstops any derivation drift.
                    .index(
                        Index::create()
                            .unique()
                            .name("uq_direct_sessions_pair")
                            .col(DirectSessions::MemberA)
                            .col(DirectSessions::MemberB),
                    )
                    .check(
                        Expr::col(DirectSessions::MemberA).ne(Expr::col(DirectSessions::MemberB)),
                    )
                    .to_owned(),
            )
            .await?;
        // "My sessions" reads filter on one member column; each column gets a
        // standalone btree (the pair-unique index does not serve single-column
        // lookups).
        manager
            .create_index(
                Index::create()
                    .name("idx_direct_sessions_member_a")
                    .table(DirectSessions::Table)
                    .col(DirectSessions::MemberA)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_direct_sessions_member_b")
                    .table(DirectSessions::Table)
                    .col(DirectSessions::MemberB)
                    .to_owned(),
            )
            .await?;

        // Per-session monotonic sequence counter; starts at 0, the first
        // append advances it to 1. Cascades with the session.
        manager
            .create_table(
                Table::create()
                    .table(DirectMessageSeq::Table)
                    .col(
                        ColumnDef::new(DirectMessageSeq::SessionId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(DirectMessageSeq::NextSeq)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_direct_message_seq_session")
                            .from(DirectMessageSeq::Table, DirectMessageSeq::SessionId)
                            .to(DirectSessions::Table, DirectSessions::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(DirectMessages::Table)
                    .col(ColumnDef::new(DirectMessages::SessionId).text().not_null())
                    .col(ColumnDef::new(DirectMessages::Seq).big_integer().not_null())
                    .col(ColumnDef::new(DirectMessages::Sender).text().not_null())
                    // The payload: plaintext `DirectMessage` JSON bytes,
                    // stored opaquely (the lesche is the store of record).
                    .col(ColumnDef::new(DirectMessages::Payload).binary().not_null())
                    .col(
                        ColumnDef::new(DirectMessages::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .primary_key(
                        Index::create()
                            .col(DirectMessages::SessionId)
                            .col(DirectMessages::Seq),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_direct_messages_session")
                            .from(DirectMessages::Table, DirectMessages::SessionId)
                            .to(DirectSessions::Table, DirectSessions::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    // The first seq is 1 (the counter advances before the
                    // insert): a 0/negative seq is never legitimate.
                    .check(Expr::col(DirectMessages::Seq).gt(0))
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(DirectReadCursors::Table)
                    .col(
                        ColumnDef::new(DirectReadCursors::SessionId)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(DirectReadCursors::Member).text().not_null())
                    .col(
                        ColumnDef::new(DirectReadCursors::LastReadSeq)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DirectReadCursors::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(DirectReadCursors::SessionId)
                            .col(DirectReadCursors::Member),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_direct_read_cursors_session")
                            .from(DirectReadCursors::Table, DirectReadCursors::SessionId)
                            .to(DirectSessions::Table, DirectSessions::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(DirectReadCursors::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(DirectMessages::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(DirectMessageSeq::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(DirectSessions::Table).to_owned())
            .await
    }
}
