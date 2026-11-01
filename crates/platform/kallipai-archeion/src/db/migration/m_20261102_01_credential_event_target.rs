//! `credential_events.target_id` — what an event acted on, when that is not the
//! credential row itself.
//!
//! A `mint` event belongs to the token that asked for the enrollment code
//! (`token_id`) but concerns the pending tagma it created, and no other column
//! of the row names that tagma: this column is what lets the database answer
//! which token created which tagma. Nullable: `create`, `reset`, and `perms`
//! all act on their own credential and leave it NULL.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum CredentialEvents {
    Table,
    TargetId,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(CredentialEvents::Table)
                    .add_column(ColumnDef::new(CredentialEvents::TargetId).text())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(CredentialEvents::Table)
                    .drop_column(CredentialEvents::TargetId)
                    .to_owned(),
            )
            .await
    }
}
