//! `room_messages` thread-listing index: `(room_id, root_seq, seq)` over the
//! rows that name a discussion.
//!
//! The thread panel reads one discussion forward (`store::read_thread`): the
//! head row comes from the composite primary key, and every reply from
//! `root_seq = X`. The partial predicate leaves head rows out -- `root_seq` is
//! NULL there, they are found by the primary key, and keeping them would only
//! make the index larger.
//!
//! The statement is raw SQL because sea-query's `IndexCreateStatement` has no
//! partial predicate (0.32.7): a constructor-shaped rewrite would silently drop
//! `WHERE root_seq IS NOT NULL` and only make the index larger. `down` uses
//! `Index::drop()`, which needs no such spelling.
//!
//! `m_20261113_01_thread_root` shipped no index because the only read then was
//! the primary-key point lookup a sender's declared thread resolves against; it
//! named listing a whole discussion as the trigger for one, and this is it.
//!
//! The build takes a write lock for its duration: the runner wraps a migration
//! in one transaction, and `CREATE INDEX CONCURRENTLY` cannot run inside one.
//! A deployment whose `room_messages` is already large needs a maintenance
//! window for the build.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX idx_room_messages_thread_seq \
                 ON room_messages (room_id, root_seq, seq) \
                 WHERE root_seq IS NOT NULL",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_room_messages_thread_seq")
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
}
