//! `passkeys.last_used_at` — when each passkey was last used.
//!
//! NOT NULL. Seeded to `now` at enrollment (a registration/pair ceremony is
//! itself a user-verification event, so "first use" == the enrollment instant)
//! and stamped to `now` on every subsequent `login_finish`. This avoids an
//! `Option`/NULL whose `None` case would just duplicate `created_at`.
//!
//! Existing rows are backfilled from `created_at` (the best proxy available --
//! prior sign-ins were not recorded). No DB default is kept: every insert
//! (register, pair-bind) and every login update supplies the value explicitly.

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Passkeys {
    Table,
    LastUsedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add nullable, backfill from created_at (best proxy for legacy rows),
        // then set NOT NULL. No default is kept.
        manager
            .alter_table(
                Table::alter()
                    .table(Passkeys::Table)
                    .add_column(ColumnDef::new(Passkeys::LastUsedAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await?;
        let conn = manager.get_connection();
        conn.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "UPDATE passkeys SET last_used_at = created_at",
        ))
        .await?;
        conn.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "ALTER TABLE passkeys ALTER COLUMN last_used_at SET NOT NULL",
        ))
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Passkeys::Table)
                    .drop_column(Passkeys::LastUsedAt)
                    .to_owned(),
            )
            .await
    }
}
