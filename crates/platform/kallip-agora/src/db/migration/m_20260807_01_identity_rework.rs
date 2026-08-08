//! Identity / login rework -- the single schema delta of the identity rework
//! (login resolves by username; email becomes an optional contact channel;
//! OAuth and passwordless sign-in are added; the invite gate is removed).
//!
//! This is one migration (not a chain) because the granular steps were the
//! schema shadow of development commits that have since been squashed; folding
//! removes the intra-commit introduce-then-modify noise (e.g. the `emails`
//! table is born here with `verification_token_expires_at`, rather than having
//! it added in a later step). It additionally renames the now-stale
//! `webauthn_challenges.invite_code_hash` column to its true final purpose,
//! `pairing_code_hash` (the column only ever carries the device-pairing code
//! hash now that the invite gate is gone) -- a tidy folded in here rather than
//! as its own step. `init` (`m_20260718_01_init`) is NOT edited per the
//! incremental-migration rule: the columns/tables it created under now-dead
//! names are altered/dropped/renamed here.
//!
//! `emails` (1:N contact channel):
//! - Historically `users.email` was a single NOT NULL UNIQUE column overloaded as
//!   the WebAuthn `user.name`, the login lookup key, and the contact channel, and
//!   it was never updated. Email moves into its own 1:N table so an account may
//!   have several addresses (a primary plus backups), change them, and track
//!   verification -- while login resolves by `users.username`.
//! - Existing rows are backfilled from `users.email` as the primary address with
//!   `verified_at = NULL` (email was never verified today -- there is no SMTP --
//!   so no timestamp is fabricated). `id` derives from `users.id::uuid` (the
//!   `UserId` is a UUID newtype); avoids `gen_random_uuid()`, core only on PG>=13.
//! - `uniq_emails_address`: global unique on the canonical address (one account
//!   per address, mirroring the old single-email constraint).
//!   `uniq_emails_primary_per_account`: partial unique on `account_id WHERE
//!   is_primary` -- enforces at-most-one primary per account at storage level.
//!   `idx_emails_account`: list-by-owner. `idx_emails_verify_token`: partial index
//!   on the pending verification-token hash (the verify endpoint looks up by
//!   hash; cleared/NULL slots are not indexed).
//!
//! `users.email` is dropped (after the backfill) along with its unique index
//! `uniq_users_email`; the source column is gone now that nothing reads it.
//!
//! `webauthn_challenges`: the dead transient `email` column is dropped, and the
//! stale `invite_code_hash` column (it held the invite-code hash under the old
//! gate; now pairing-only) is renamed to `pairing_code_hash` (+ its index
//! rebuilt as `idx_webauthn_challenges_pairing_hash`). The Rust entity field
//! matches, so no `column_name` alias is needed.
//!
//! `passkeys.discoverable`: marks credentials enrolled via the discoverable
//! (resident-key) flow. Added NOT NULL with a transient DEFAULT false so existing
//! rows backfill, then the DEFAULT is dropped so the final column has no sentinel
//! (every insert supplies a real value). It is a server-side enrollment fact and
//! gates the "passwordless sign-in" UI affordance.
//!
//! `external_identities` + `oauth_states`: OAuth (GitHub, Google) as a first-class
//! sign-in method, symmetric with passkeys. `external_identities` links a
//! `(provider, subject)` pair to a `users` row; either credential kind may be the
//! sole one. `oauth_states` holds the Authorization-Code ceremony state between
//! begin and finish (the `state` CSRF token hash PK, provider, action, sanitized
//! return path, optional bound user_id, PKCE verifier); single-use, GC'd like
//! `webauthn_challenges`. No backfill (no OAuth identities pre-exist).
//!
//! `invite_codes` is dropped (it was created by `init`): signup is now open, so
//! the table, its indexes, and its `consumed_by -> users(id)` FK are all dead.

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Each migration redeclares its own `DeriveIden` enums (init's enums are private
// to it and cannot be `use`d across files).
#[derive(DeriveIden)]
enum Emails {
    Table,
    Id,
    AccountId,
    Address,
    IsPrimary,
    VerifiedAt,
    /// SHA-256 hash of the pending verification token; NULL once verified or
    /// when no verification is pending.
    VerificationTokenHash,
    VerificationTokenExpiresAt,
    AddedAt,
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
    Email,
}

#[derive(DeriveIden)]
enum WebauthnChallenges {
    Table,
    Email,
    InviteCodeHash,
    PairingCodeHash,
}

#[derive(DeriveIden)]
enum Passkeys {
    Table,
    Discoverable,
}

#[derive(DeriveIden)]
enum ExternalIdentities {
    Table,
    Id,
    UserId,
    Provider,
    Subject,
    DisplayName,
    CreatedAt,
    LastUsedAt,
}

#[derive(DeriveIden)]
enum OauthStates {
    Table,
    StateHash,
    Provider,
    Action,
    ReturnPath,
    UserId,
    PkceVerifier,
    CreatedAt,
    ExpiresAt,
}

#[derive(DeriveIden)]
enum InviteCodes {
    Table,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 1. emails table (born with verification_token_expires_at).
        manager
            .create_table(
                Table::create()
                    .table(Emails::Table)
                    .col(ColumnDef::new(Emails::Id).uuid().not_null().primary_key())
                    .col(ColumnDef::new(Emails::AccountId).text().not_null())
                    .col(ColumnDef::new(Emails::Address).text().not_null())
                    .col(ColumnDef::new(Emails::IsPrimary).boolean().not_null())
                    .col(ColumnDef::new(Emails::VerifiedAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(Emails::VerificationTokenHash).binary())
                    .col(
                        ColumnDef::new(Emails::VerificationTokenExpiresAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(Emails::AddedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_emails_account")
                            .from(Emails::Table, Emails::AccountId)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uniq_emails_address")
                    .table(Emails::Table)
                    .col(Emails::Address)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_emails_account")
                    .table(Emails::Table)
                    .col(Emails::AccountId)
                    .to_owned(),
            )
            .await?;
        // Partial unique index: at most one primary email per account. Raw SQL
        // because sea-orm's Index builder has no WHERE-clause support.
        manager
            .get_connection()
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "CREATE UNIQUE INDEX uniq_emails_primary_per_account \
                 ON emails (account_id) WHERE is_primary",
            ))
            .await?;
        // Index the pending verification-token hash (partial so cleared/NULL
        // slots are not indexed).
        manager
            .get_connection()
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "CREATE INDEX idx_emails_verify_token \
                 ON emails (verification_token_hash) \
                 WHERE verification_token_hash IS NOT NULL",
            ))
            .await?;

        // 2. Backfill emails from users.email (before dropping the source
        // column). verification_token_* default NULL -- no pending token, and
        // email was never verified today.
        manager
            .get_connection()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "INSERT INTO emails (id, account_id, address, is_primary, verified_at, added_at) \
                 SELECT id::uuid, id, email, TRUE, NULL, created_at \
                 FROM users WHERE email IS NOT NULL",
                [],
            ))
            .await?;

        // 3. Drop users.email (and its unique index) now that nothing reads it.
        manager
            .get_connection()
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "DROP INDEX IF EXISTS uniq_users_email",
            ))
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Users::Table)
                    .drop_column(Users::Email)
                    .to_owned(),
            )
            .await?;

        // 4. webauthn_challenges: drop the dead email column, rename the stale
        // invite_code_hash to its true pairing purpose, and rebuild its index
        // under the new name.
        manager
            .alter_table(
                Table::alter()
                    .table(WebauthnChallenges::Table)
                    .drop_column(WebauthnChallenges::Email)
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(WebauthnChallenges::Table)
                    .rename_column(
                        WebauthnChallenges::InviteCodeHash,
                        WebauthnChallenges::PairingCodeHash,
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .drop_index(
                Index::drop()
                    .name("idx_webauthn_challenges_invite_hash")
                    .table(WebauthnChallenges::Table)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_webauthn_challenges_pairing_hash")
                    .table(WebauthnChallenges::Table)
                    .col(WebauthnChallenges::PairingCodeHash)
                    .to_owned(),
            )
            .await?;

        // 5. passkeys.discoverable: NOT NULL with a transient default to
        // backfill existing rows, then drop the default so there is no sentinel.
        manager
            .alter_table(
                Table::alter()
                    .table(Passkeys::Table)
                    .add_column(
                        ColumnDef::new(Passkeys::Discoverable)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .get_connection()
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "ALTER TABLE passkeys ALTER COLUMN discoverable DROP DEFAULT",
            ))
            .await?;

        // 6. external_identities + oauth_states.
        manager
            .create_table(
                Table::create()
                    .table(ExternalIdentities::Table)
                    .col(
                        ColumnDef::new(ExternalIdentities::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ExternalIdentities::UserId).text().not_null())
                    .col(
                        ColumnDef::new(ExternalIdentities::Provider)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ExternalIdentities::Subject)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ExternalIdentities::DisplayName).text())
                    .col(
                        ColumnDef::new(ExternalIdentities::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ExternalIdentities::LastUsedAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_external_identities_user")
                            .from(ExternalIdentities::Table, ExternalIdentities::UserId)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        // One account per (provider, subject); the login resolution key.
        manager
            .create_index(
                Index::create()
                    .name("uniq_external_identities_provider_subject")
                    .table(ExternalIdentities::Table)
                    .col(ExternalIdentities::Provider)
                    .col(ExternalIdentities::Subject)
                    .unique()
                    .to_owned(),
            )
            .await?;
        // List-by-owner + the last-method guard's FOR UPDATE scan.
        manager
            .create_index(
                Index::create()
                    .name("idx_external_identities_user")
                    .table(ExternalIdentities::Table)
                    .col(ExternalIdentities::UserId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(OauthStates::Table)
                    .col(
                        ColumnDef::new(OauthStates::StateHash)
                            .binary()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(OauthStates::Provider).text().not_null())
                    .col(ColumnDef::new(OauthStates::Action).text().not_null())
                    .col(ColumnDef::new(OauthStates::ReturnPath).text())
                    .col(ColumnDef::new(OauthStates::UserId).text())
                    .col(ColumnDef::new(OauthStates::PkceVerifier).text())
                    .col(
                        ColumnDef::new(OauthStates::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(OauthStates::ExpiresAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    // No FK on user_id: it is only set for action=link and points
                    // at a user that exists; the link flow validates ownership in
                    // its own txn. Mirrors webauthn_challenges (whose user_id
                    // also references a not-yet-existing row at register begin).
                    .to_owned(),
            )
            .await?;
        // GC sweep efficiency: the 60s task deletes WHERE expires_at <= now.
        manager
            .create_index(
                Index::create()
                    .name("idx_oauth_states_expires_at")
                    .table(OauthStates::Table)
                    .col(OauthStates::ExpiresAt)
                    .to_owned(),
            )
            .await?;

        // 7. Drop invite_codes (created by init): signup is open, the gate is
        // gone for good.
        manager
            .drop_table(Table::drop().table(InviteCodes::Table).to_owned())
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Non-reversible: email is decoupled (cannot collapse back into one
        // NOT NULL UNIQUE column), OAuth-only accounts depend on
        // external_identities, and the invite gate is gone for good.
        Err(DbErr::Custom(format!(
            "{} is non-reversible (identity rework)",
            self.name()
        )))
    }
}
