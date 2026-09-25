//! Add `enrollment_tokens.token_masked`: a display-safe mask
//! (`sk-enroll-abc***xyz`) persisted at mint so the list endpoint can show a
//! code without re-fetching the unrecoverable plaintext. Existing rows backfill
//! to `sk-enroll-***` -- a masked placeholder that renders as "not copyable"
//! (its real mask is unrecoverable; only the hash was ever stored). New mints
//! overwrite it with the real masked value.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum EnrollmentTokens {
    Table,
    TokenMasked,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(EnrollmentTokens::Table)
                    .add_column(
                        ColumnDef::new(EnrollmentTokens::TokenMasked)
                            .text()
                            .not_null()
                            .default("sk-enroll-***"),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(EnrollmentTokens::Table)
                    .drop_column(EnrollmentTokens::TokenMasked)
                    .to_owned(),
            )
            .await
    }
}
