//! Passkey device labels, one-shot step-up freshness, and the live/revoked
//! split.
//!
//! Three changes, all in service of multi-device passkey management with no
//! historical baggage:
//!
//! 1. `passkeys.label` — a user-supplied device label ("iPhone", "MacBook").
//!    Added `NOT NULL DEFAULT ''` so existing beta rows backfill, then the
//!    default is DROPPED so the final schema has a NOT NULL label with no
//!    sentinel default (new code always supplies a real label).
//! 2. `passkeys.compromised_at` DROPPED. The live `passkeys` table now holds
//!    ONLY active credentials; revoked / cloned credentials are hard-deleted and
//!    recorded in the new `passkey_revocations` audit table (see below). Every
//!    query on `passkeys` is therefore filter-free.
//! 3. `sessions.authed_at` — a nullable freshness timestamp for the one-shot
//!    step-up that gates "add a passkey". Set by login/register finish; consumed
//!    (set NULL) by the add-passkey begin txn. `NULL` = consumed or not freshly
//!    authed.
//!
//! `passkey_revocations` is an append-only audit log: one row per revoked or
//! clone-detected credential. `cred_id` is indexed so it doubles as a denylist
//! at add-passkey finish (refuses re-binding a previously-revoked credential).

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Each migration redeclares its own `DeriveIden` enums (the init file's enums
// are private to it and cannot be `use`d across files).
#[derive(DeriveIden)]
enum Passkeys {
    Table,
    Label,
    CompromisedAt,
}

#[derive(DeriveIden)]
enum Sessions {
    Table,
    AuthedAt,
}

#[derive(DeriveIden)]
enum PasskeyRevocations {
    Table,
    Id,
    UserId,
    CredId,
    Reason,
    RevokedBy,
    RevokedAt,
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 1. passkeys.label: NOT NULL with a transient default to backfill
        //    existing rows, then DROP the default so the column has no sentinel.
        manager
            .alter_table(
                Table::alter()
                    .table(Passkeys::Table)
                    .add_column(
                        ColumnDef::new(Passkeys::Label)
                            .text()
                            .not_null()
                            .default(""),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .get_connection()
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "ALTER TABLE passkeys ALTER COLUMN label DROP DEFAULT",
            ))
            .await?;

        // 2. passkeys.compromised_at: dropped (live/revoked split).
        manager
            .alter_table(
                Table::alter()
                    .table(Passkeys::Table)
                    .drop_column(Passkeys::CompromisedAt)
                    .to_owned(),
            )
            .await?;

        // 3. sessions.authed_at: nullable freshness timestamp.
        manager
            .alter_table(
                Table::alter()
                    .table(Sessions::Table)
                    .add_column(ColumnDef::new(Sessions::AuthedAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await?;

        // 4. passkey_revocations audit table (append-only; cred_id doubles as a
        //    denylist at add-passkey finish).
        manager
            .create_table(
                Table::create()
                    .table(PasskeyRevocations::Table)
                    .col(
                        ColumnDef::new(PasskeyRevocations::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(PasskeyRevocations::UserId).text().not_null())
                    .col(
                        ColumnDef::new(PasskeyRevocations::CredId)
                            .binary()
                            .not_null(),
                    )
                    .col(ColumnDef::new(PasskeyRevocations::Reason).text().not_null())
                    .col(
                        ColumnDef::new(PasskeyRevocations::RevokedBy)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PasskeyRevocations::RevokedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_passkey_revocations_user")
                            .from(PasskeyRevocations::Table, PasskeyRevocations::UserId)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_passkey_revocations_user")
                    .table(PasskeyRevocations::Table)
                    .col(PasskeyRevocations::UserId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_passkey_revocations_cred_id")
                    .table(PasskeyRevocations::Table)
                    .col(PasskeyRevocations::CredId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Non-reversible: revoked/cloned passkeys are hard-deleted at revoke
        // time and live only in `passkey_revocations`, so the live/revoked split
        // cannot be undone -- there is no `compromised_at` value to restore and
        // the audit history would be destroyed. Mirrors the `tagma_unified`
        // migration's non-reversible `down`.
        Err(DbErr::Custom(format!(
            "{} is non-reversible (passkeys live/revoked split)",
            self.name()
        )))
    }
}
