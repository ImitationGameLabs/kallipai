//! Drop `user_providers` -- the per-account AI provider credential vault.
//! The `/me/providers` self-service surface and its web-client consumers are
//! retired (the model gateway serves provider credentials now), so the table
//! has no readers left. Rows die with the table; the vault was account-owned
//! data and nothing else references it.
//!
//! `down` recreates the table exactly as `m_20260826_01_user_providers` built
//! it (nine columns, the `fk_user_providers_user` cascade, the
//! `idx_user_providers_user_name` unique index); the recreation starts empty,
//! matching that migration's no-backfill rationale.

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
            .drop_table(Table::drop().table(UserProviders::Table).to_owned())
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
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
}
