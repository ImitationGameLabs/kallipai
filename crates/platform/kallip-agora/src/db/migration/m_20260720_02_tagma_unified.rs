//! Fold `enrollment_tokens` into `tagmata`: a tagma is born pending (carrying a
//! one-time enrollment code), transitions to enrolled when a herald connects
//! (code cleared, device key pinned), and may be revoked from either state.
//!
//! New `tagmata` columns: `revoked_at` (the unified revoke flag, enforced in
//! `resolve_bearer` and the enroll handler), `enrolled_at` (phase marker), and
//! the pending-phase `enrollment_code_hash` / `enrollment_code_masked` /
//! `expires_at` (nullable; cleared on enroll). `pinned_public_key` becomes
//! nullable (NULL while pending). A partial unique index on
//! `enrollment_code_hash` (NULLs excluded) keeps the pending lookup unique
//! without colliding on the cleared/enrolled NULLs.
//!
//! Live pending enrollment codes are backfilled into `tagmata` (id stable from
//! mint through enroll), then `enrollment_tokens` is dropped. The unused
//! `tagma_tokens.revoked_at` is dropped too -- the single revoke source of
//! truth is now `tagmata.revoked_at`. Non-reversible: consumed codes have no
//! target row to return to.

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Tagmata {
    Table,
    RevokedAt,
    EnrolledAt,
    EnrollmentCodeHash,
    EnrollmentCodeMasked,
    ExpiresAt,
}

#[derive(DeriveIden)]
enum TagmaTokens {
    Table,
    RevokedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 1. Add the unified-lifecycle columns to tagmata.
        manager
            .alter_table(
                Table::alter()
                    .table(Tagmata::Table)
                    .add_column(ColumnDef::new(Tagmata::RevokedAt).timestamp_with_time_zone())
                    .add_column(ColumnDef::new(Tagmata::EnrolledAt).timestamp_with_time_zone())
                    .add_column(ColumnDef::new(Tagmata::EnrollmentCodeHash).binary())
                    .add_column(ColumnDef::new(Tagmata::EnrollmentCodeMasked).text())
                    .add_column(ColumnDef::new(Tagmata::ExpiresAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await?;

        // A pending tagma carries no device key yet.
        let conn = manager.get_connection();
        conn.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "ALTER TABLE tagmata ALTER COLUMN pinned_public_key DROP NOT NULL",
        ))
        .await?;

        // 2. Partial unique index on the pending code hash. sea-query 0.32 has
        //    no `Index::create().filter(...)`, so raw SQL. The WHERE is
        //    essential: enrolled rows have NULL enrollment_code_hash and would
        //    collide on a plain UNIQUE.
        conn.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "CREATE UNIQUE INDEX uniq_tagmata_enrollment_code_hash \
             ON tagmata (enrollment_code_hash) \
             WHERE enrollment_code_hash IS NOT NULL",
        ))
        .await?;

        // 3. Backfill live pending enrollment codes as pending tagmas. The id is
        //    stable across the enroll transition (a herald later enrolls the
        //    same row). `ON CONFLICT DO NOTHING` guards the negligible UUID PK
        //    collision. `id::text` is hyphenated-lowercase, matching
        //    `TagmaId::random()` -> `Uuid::new_v4().to_string()`.
        conn.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "INSERT INTO tagmata (id, owner_user_id, created_at, expires_at, \
             enrollment_code_hash, enrollment_code_masked) \
             SELECT id::text, user_id, created_at, expires_at, token_hash, token_masked \
             FROM enrollment_tokens \
             WHERE consumed_at IS NULL AND revoked_at IS NULL \
             ON CONFLICT (id) DO NOTHING",
        ))
        .await?;

        // 4. The pending credential table is now redundant.
        manager
            .drop_table(Table::drop().table(EnrollmentTokens::Table).to_owned())
            .await?;

        // 5. Single source of truth for revocation: tagmata.revoked_at.
        manager
            .alter_table(
                Table::alter()
                    .table(TagmaTokens::Table)
                    .drop_column(TagmaTokens::RevokedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Non-reversible: consumed codes have no target enrollment_tokens row to
        // return to, and the fold discards the consumed/revoked audit trail.
        Err(DbErr::Custom(format!(
            "{} is non-reversible (enrollment_tokens folded into tagmata)",
            self.name()
        )))
    }
}

// Referenced only by `drop_table`; the column idens live in the init migration.
#[derive(DeriveIden)]
enum EnrollmentTokens {
    Table,
}
