//! `user_providers` -- a per-account AI provider credential entry (API key
//! plus optional base-URL override). Part of the mixed-mode provider vault:
//! each row carries its own `mode`, and `key_material` is either the
//! plaintext key (`plaintext`) or a client-encrypted blob (`encrypted`).
//! The server treats `key_material` as opaque in both modes -- encrypting and
//! decrypting happen in the web client; archeion never interprets the column.
//!
//! - `UNIQUE (user_id, name)` (`idx_user_providers_user_name`): `name` is the
//!   user-visible label, so one account cannot hold two entries with the
//!   same name (409 at the handler, mirroring the emails address conflict).
//! - `mode` is TEXT rather than a DB enum: this is the first mode-carrying
//!   column in archeion, and handler-side validation keeps the accepted value
//!   set to `plaintext` | `encrypted` without a migration per vocabulary change
//!   (no enum-column precedent to mirror).
//! - `fk_user_providers_user` ON DELETE CASCADE mirrors
//!   `fk_device_pairing_codes_user`: entries are account-owned data and die
//!   with the account.
//! - No backfill: the table starts empty (it lands together with its API).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Each migration redeclares its own `DeriveIden` enums (other migrations'
// enums are private to them and cannot be `use`d across files).
#[derive(DeriveIden)]
enum UserProviders {
    Table,
    Id,
    UserId,
    Name,
    Provider,
    BaseUrl,
    KeyMaterial,
    Mode,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(UserProviders::Table)
                    .col(
                        ColumnDef::new(UserProviders::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(UserProviders::UserId).text().not_null())
                    .col(ColumnDef::new(UserProviders::Name).text().not_null())
                    .col(ColumnDef::new(UserProviders::Provider).text().not_null())
                    .col(ColumnDef::new(UserProviders::BaseUrl).text())
                    .col(ColumnDef::new(UserProviders::KeyMaterial).text().not_null())
                    .col(ColumnDef::new(UserProviders::Mode).text().not_null())
                    .col(
                        ColumnDef::new(UserProviders::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UserProviders::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_user_providers_user")
                            .from(UserProviders::Table, UserProviders::UserId)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_user_providers_user_name")
                    .table(UserProviders::Table)
                    .col(UserProviders::UserId)
                    .col(UserProviders::Name)
                    .unique()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Reversible: the table owns its rows; dropping it drops the vault
        // entries with it (the same cascade that fires on account delete).
        manager
            .drop_table(Table::drop().table(UserProviders::Table).to_owned())
            .await
    }
}
