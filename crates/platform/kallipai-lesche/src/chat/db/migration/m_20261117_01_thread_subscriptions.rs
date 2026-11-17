//! `thread_subscriptions` -- one member's choice about one thread.
//!
//! Delivery and the badge both decide "did this row reach me?" from a single
//! predicate (see `chat::audience`, `store::unread_for_member`), and until now
//! that predicate had no per-thread input: a reply woke every human and every
//! agent recorded in the thread. This table is the input: `following` widens a
//! member to every reply in the thread, `muted` narrows it to none, and no row
//! means the default the predicate already had.
//!
//! The row records positions, not a flag, because a choice is an event:
//!
//! - `since_seq` -- the room's `max(seq)` when the choice was written. Rows at
//!   or before it keep the default, so following a thread does not pull its old
//!   replies into a badge, and muting one does not take back unread already
//!   counted.
//! - `muted_from_seq` -- the lower bound of the mute window the row remembers.
//!   It exists so that re-following a muted thread does not resurrect the
//!   messages muted inside that window: they were never delivered, and the
//!   badge counts what was delivered.
//!
//! Three writes keep the two columns consistent, and the second rule is what
//! makes them consistent AT ALL: a row's state only changes when the choice
//! changes, so a repeated click leaves both boundaries where the first click
//! put them. Three CHECK constraints state the result, because a write path
//! that drifts (a handler writing `muted` without a bound, a bound past
//! `since_seq`, or a state no reader can name) is otherwise visible only on
//! rows at particular positions:
//!
//! - `state IN ('following', 'muted')`
//! - `muted_from_seq IS NULL OR muted_from_seq <= since_seq`
//! - `state <> 'muted' OR (muted_from_seq IS NOT NULL AND muted_from_seq = since_seq)`
//!
//! The domain check is the cheap half of the same idea: the read path parses
//! `state` with the wire vocabulary and errors on a label it does not know, so
//! a value this schema allows but no reader can interpret would be a row that
//! only fails when somebody reads it.
//!
//! The second is only writable because a mute never advances `since_seq` after
//! the first one; advancing it (a natural reading of "last click wins") would
//! put `since_seq` past the bound the same row carries and lose the first
//! window.
//!
//! The bound is spelled as a pair of tests because `state <> 'muted' OR
//! muted_from_seq = since_seq` passes a mute with NO bound: the equality is NULL
//! there, and a CHECK only fails on FALSE. The pair rejects that write, which is
//! the one the prose above names first.
//!
//! No backfill, by design: no row means the default, and the default is what
//! every message sent before this table existed already got. The migration is
//! behaviour-neutral.
//!
//! The constraints are raw SQL because sea-query's table builder (0.32.x) has
//! no CHECK clause; a constructor-shaped rewrite would silently drop them.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ThreadSubscriptions::Table)
                    .col(
                        ColumnDef::new(ThreadSubscriptions::RoomId)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ThreadSubscriptions::RootSeq)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ThreadSubscriptions::MemberId)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ThreadSubscriptions::State).text().not_null())
                    .col(
                        ColumnDef::new(ThreadSubscriptions::SinceSeq)
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ThreadSubscriptions::MutedFromSeq).big_integer())
                    .col(
                        ColumnDef::new(ThreadSubscriptions::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ThreadSubscriptions::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(ThreadSubscriptions::RoomId)
                            .col(ThreadSubscriptions::RootSeq)
                            .col(ThreadSubscriptions::MemberId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_thread_subscriptions_room")
                            .from(ThreadSubscriptions::Table, ThreadSubscriptions::RoomId)
                            .to(Rooms::Table, Rooms::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE thread_subscriptions ADD CONSTRAINT thread_subscriptions_window \
                 CHECK (muted_from_seq IS NULL OR muted_from_seq <= since_seq)",
            )
            .await?;
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE thread_subscriptions ADD CONSTRAINT thread_subscriptions_muted_bound \
                 CHECK (state <> 'muted' OR (muted_from_seq IS NOT NULL AND muted_from_seq = since_seq))",
            )
            .await?;
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE thread_subscriptions ADD CONSTRAINT thread_subscriptions_state \
                 CHECK (state IN ('following', 'muted'))",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ThreadSubscriptions::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ThreadSubscriptions {
    Table,
    RoomId,
    RootSeq,
    MemberId,
    State,
    SinceSeq,
    MutedFromSeq,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum Rooms {
    Table,
    Id,
}
