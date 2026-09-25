//! The kallipai-files metadata schema, created in one migration.
//!
//! Tables: `blob_rows` (the content-addressed catalog with reference
//! counts), `file_records` (one row per uploaded file a principal owns),
//! `delivery_events` (append-only delivery log).
//!
//! Reference-count invariant: `refcount` equals the number of
//! `file_records` rows pointing at the blob and never goes negative. A
//! row whose count hits zero records the moment in `freed_at` and is
//! reclaim-eligible once zero has held past the GC grace period; the
//! sweep deletes the row and unlinks the file. `file_records.blob_id`
//! carries a `RESTRICT` foreign key so a drift (deleting a
//! still-referenced catalog row) fails loudly in the database instead
//! of silently orphaning records.
//!
//! Secondary indexes are separate `create_index` calls rather than inline:
//! Postgres `CREATE TABLE` only accepts `UNIQUE`/`PRIMARY KEY` as table
//! constraints, so sea-query would emit invalid SQL for an inline
//! non-unique index (house note carried from the archeion init migration).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // --- blob_rows ------------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(BlobRows::Table)
                    .col(
                        ColumnDef::new(BlobRows::BlobId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(BlobRows::Size).big_integer().not_null())
                    .col(ColumnDef::new(BlobRows::Refcount).integer().not_null())
                    .col(
                        ColumnDef::new(BlobRows::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(BlobRows::FreedAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await?;

        // --- file_records ---------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(FileRecords::Table)
                    .col(
                        ColumnDef::new(FileRecords::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(FileRecords::SpacePath).text().not_null())
                    .col(ColumnDef::new(FileRecords::Owner).text().not_null())
                    .col(ColumnDef::new(FileRecords::BlobId).text().not_null())
                    .col(ColumnDef::new(FileRecords::Provenance).text())
                    .col(
                        ColumnDef::new(FileRecords::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_file_records_blob")
                            .from(FileRecords::Table, FileRecords::BlobId)
                            .to(BlobRows::Table, BlobRows::BlobId)
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                // Refcount lookups from the remove path resolve the record's
                // blob; reverse lookups ("who references this blob?") key on
                // `blob_id` alone.
                Index::create()
                    .name("idx_file_records_blob_id")
                    .table(FileRecords::Table)
                    .col(FileRecords::BlobId)
                    .to_owned(),
            )
            .await?;

        // --- delivery_events ------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(DeliveryEvents::Table)
                    .col(
                        ColumnDef::new(DeliveryEvents::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(DeliveryEvents::HappenedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DeliveryEvents::FromPrincipal)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DeliveryEvents::ToPrincipal)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(DeliveryEvents::BlobId).text().not_null())
                    .col(ColumnDef::new(DeliveryEvents::SourceRecordId).uuid())
                    .col(ColumnDef::new(DeliveryEvents::TargetRecordId).uuid())
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_delivery_events_blob_id")
                    .table(DeliveryEvents::Table)
                    .col(DeliveryEvents::BlobId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Children first: the foreign keys forbid dropping a parent table
        // that rows still reference.
        manager
            .drop_table(Table::drop().table(DeliveryEvents::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(FileRecords::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(BlobRows::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum BlobRows {
    Table,
    BlobId,
    Size,
    Refcount,
    CreatedAt,
    FreedAt,
}

#[derive(DeriveIden)]
enum FileRecords {
    Table,
    Id,
    SpacePath,
    Owner,
    BlobId,
    Provenance,
    CreatedAt,
}

#[derive(DeriveIden)]
enum DeliveryEvents {
    Table,
    Id,
    HappenedAt,
    FromPrincipal,
    ToPrincipal,
    BlobId,
    SourceRecordId,
    TargetRecordId,
}
