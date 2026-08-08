//! OAuth signup username step -- holds the resolved `(provider, subject)`
//! claim on `oauth_states` between finish and a new `signup/complete` call.
//!
//! Today an unlinked OAuth identity is auto-registered at finish with a
//! server-synthesized username. The rework moves creation behind an explicit
//! choose-username step: finish resolves the provider identity and, for an
//! unlinked subject, keeps the ceremony row alive holding the claim plus a
//! single-use `signup_token`; the user then submits a chosen username at
//! `POST /auth/oauth/signup/complete`, which creates the account and deletes
//! the row. These nullable columns are NULL for every other ceremony (login,
//! link) and for a signin row until finish transitions it to held.
//!
//! - `subject` / `claim_display_name`: the resolved `ProviderIdentity` (subject
//!   is the login-resolution key; `claim_display_name` is display-only and lands
//!   on `external_identities.display_name` at completion).
//! - `signup_token_hash`: SHA-256 of the opaque `sk-oauthsu-` token returned to
//!   the SPA. Mirrors `state_hash` / `sessions.token_hash`. NULL everywhere
//!   except a held signup row.
//! - `idx_oauth_states_signup_token`: UNIQUE partial index on
//!   `signup_token_hash WHERE signup_token_hash IS NOT NULL` -- the complete
//!   endpoint's lookup; NULL (login/link) rows are not indexed. UNIQUE pins the
//!   single-use intent (a hash is held by at most one row) and mirrors the
//!   `uniq_emails_verify_token` / `sessions.token_hash` pattern. Mirrors
//!   `idx_emails_verify_token`.
//!
//! No backfill: pre-re-release, no row carries a held claim.

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Each migration redeclares its own `DeriveIden` enums (other migrations'
// enums are private to them and cannot be `use`d across files).
#[derive(DeriveIden)]
enum OauthStates {
    Table,
    Subject,
    ClaimDisplayName,
    SignupTokenHash,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(OauthStates::Table)
                    .add_column(ColumnDef::new(OauthStates::Subject).text())
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(OauthStates::Table)
                    .add_column(ColumnDef::new(OauthStates::ClaimDisplayName).text())
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(OauthStates::Table)
                    .add_column(ColumnDef::new(OauthStates::SignupTokenHash).binary())
                    .to_owned(),
            )
            .await?;
        // UNIQUE partial index (sea-orm's Index builder has no WHERE clause):
        // only held signup rows carry a token hash, so the complete-endpoint
        // lookup stays tiny and login/link rows are never indexed. UNIQUE pins
        // the single-use intent (a hash is held by at most one row).
        manager
            .get_connection()
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "CREATE UNIQUE INDEX idx_oauth_states_signup_token \
                 ON oauth_states (signup_token_hash) \
                 WHERE signup_token_hash IS NOT NULL",
            ))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "DROP INDEX IF EXISTS idx_oauth_states_signup_token",
            ))
            .await?;
        for col in [
            OauthStates::SignupTokenHash,
            OauthStates::ClaimDisplayName,
            OauthStates::Subject,
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(OauthStates::Table)
                        .drop_column(col)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}
