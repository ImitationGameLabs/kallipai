//! The agora control-plane schema, created in one migration.
//!
//! Tables: `users`, `tagmata`, `tagma_tokens`, `enrollment_tokens`, `passkeys`,
//! `invite_codes`, `sessions`, `webauthn_challenges`.
//!
//! Public-id columns are `TEXT` (they hold opaque UUID-*string* newtypes from
//! `kallip_common::id_type!`); `UUID` is reserved for synthetic row ids that
//! never cross an id-newtype boundary.
//!
//! The durable/soft-state split: identity, credentials, and provisioning
//! (users, passkeys, invite/enrollment tokens, tagmata, tagma tokens,
//! sessions) live here; the data-plane soft state (presence, conversations,
//! app streams, tunnel-proof replay guard) stays in the bin's in-memory
//! `Registry` and is rebuilt on restart.
//!
//! First DB introduction: each table is created once with its full final column
//! set (no incremental `alter_table` adds), and nothing is guarded by
//! `if_not_exists` -- there is no production data to migrate. A dev DB that
//! recorded earlier migration names in `seaql_migrations` should be dropped and
//! re-migrated.
//!
//! Secondary (non-unique) indexes are separate `create_index` calls rather than
//! inline: Postgres `CREATE TABLE` only accepts `UNIQUE`/`PRIMARY KEY` (plus
//! CHECK/FOREIGN KEY/EXCLUDE) as table constraints, so sea-query would emit
//! invalid SQL for an inline non-unique index.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // --- users ----------------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(Users::Table)
                    .col(ColumnDef::new(Users::Id).text().not_null().primary_key())
                    .col(ColumnDef::new(Users::Username).text().not_null())
                    .col(ColumnDef::new(Users::Email).text().not_null())
                    .col(ColumnDef::new(Users::DisplayName).text())
                    .col(
                        ColumnDef::new(Users::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Users::DisabledAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uniq_users_username")
                    .table(Users::Table)
                    .col(Users::Username)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uniq_users_email")
                    .table(Users::Table)
                    .col(Users::Email)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // --- tagmata --------------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(Tagmata::Table)
                    .col(ColumnDef::new(Tagmata::Id).text().not_null().primary_key())
                    .col(ColumnDef::new(Tagmata::OwnerUserId).text().not_null())
                    .col(ColumnDef::new(Tagmata::PinnedPublicKey).binary().not_null())
                    .col(
                        ColumnDef::new(Tagmata::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Tagmata::Label).text())
                    // High-water-mark of the accepted herald tunnel-proof timestamp
                    // (unix seconds). NULL until the first connect; the tunnel handler
                    // accepts a proof only when this is NULL or strictly less than the
                    // incoming timestamp, defeating replay across agora restarts.
                    .col(ColumnDef::new(Tagmata::LastTunnelProofTs).big_integer())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_tagmata_owner_user")
                            .from(Tagmata::Table, Tagmata::OwnerUserId)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_tagmata_owner")
                    .table(Tagmata::Table)
                    .col(Tagmata::OwnerUserId)
                    .to_owned(),
            )
            .await?;

        // --- tagma_tokens ---------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(TagmaTokens::Table)
                    .col(
                        ColumnDef::new(TagmaTokens::TokenHash)
                            .binary()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(TagmaTokens::TagmaId).text().not_null())
                    .col(
                        ColumnDef::new(TagmaTokens::IssuedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TagmaTokens::RevokedAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_tagma_tokens_tagma")
                            .from(TagmaTokens::Table, TagmaTokens::TagmaId)
                            .to(Tagmata::Table, Tagmata::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_tagma_tokens_tagma")
                    .table(TagmaTokens::Table)
                    .col(TagmaTokens::TagmaId)
                    .to_owned(),
            )
            .await?;

        // --- enrollment_tokens ----------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(EnrollmentTokens::Table)
                    .col(
                        ColumnDef::new(EnrollmentTokens::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(EnrollmentTokens::TokenHash)
                            .binary()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(EnrollmentTokens::UserId).text().not_null())
                    .col(
                        ColumnDef::new(EnrollmentTokens::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EnrollmentTokens::ExpiresAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(EnrollmentTokens::ConsumedAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(EnrollmentTokens::ConsumedByTagma).text())
                    .col(ColumnDef::new(EnrollmentTokens::RevokedAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_enrollment_tokens_user")
                            .from(EnrollmentTokens::Table, EnrollmentTokens::UserId)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_enrollment_tokens_consumed_tagma")
                            .from(EnrollmentTokens::Table, EnrollmentTokens::ConsumedByTagma)
                            .to(Tagmata::Table, Tagmata::Id)
                            .on_delete(ForeignKeyAction::SetNull),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_enrollment_tokens_user")
                    .table(EnrollmentTokens::Table)
                    .col(EnrollmentTokens::UserId)
                    .to_owned(),
            )
            .await?;

        // --- passkeys -------------------------------------------------------
        // The full library `Passkey` is stored in a single `credential` JSONB
        // column (the documented storage model), with `cred_id` pulled out as
        // a UNIQUE indexed column so login resolves `credential id -> row`
        // without a scan. `compromised_at` is set when `webauthn-rs` reports a
        // signature-counter regression (possible clone); a compromised passkey
        // is filtered out of login and cannot authenticate.
        manager
            .create_table(
                Table::create()
                    .table(Passkeys::Table)
                    .col(ColumnDef::new(Passkeys::Id).uuid().not_null().primary_key())
                    .col(ColumnDef::new(Passkeys::UserId).text().not_null())
                    .col(ColumnDef::new(Passkeys::CredId).binary().not_null())
                    .col(ColumnDef::new(Passkeys::Credential).json().not_null())
                    .col(
                        ColumnDef::new(Passkeys::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Passkeys::CompromisedAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_passkeys_user")
                            .from(Passkeys::Table, Passkeys::UserId)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_passkeys_user")
                    .table(Passkeys::Table)
                    .col(Passkeys::UserId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uniq_passkeys_cred_id")
                    .table(Passkeys::Table)
                    .col(Passkeys::CredId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // --- invite_codes ---------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(InviteCodes::Table)
                    .col(
                        ColumnDef::new(InviteCodes::CodeHash)
                            .binary()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(InviteCodes::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(InviteCodes::ExpiresAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(InviteCodes::ConsumedAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(InviteCodes::ConsumedBy).text())
                    .col(ColumnDef::new(InviteCodes::Note).text())
                    .col(ColumnDef::new(InviteCodes::RevokedAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_invite_codes_consumed_by")
                            .from(InviteCodes::Table, InviteCodes::ConsumedBy)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::SetNull),
                    )
                    .to_owned(),
            )
            .await?;

        // --- sessions -------------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(Sessions::Table)
                    .col(
                        ColumnDef::new(Sessions::TokenHash)
                            .binary()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Sessions::UserId).text().not_null())
                    .col(
                        ColumnDef::new(Sessions::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Sessions::ExpiresAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_sessions_user")
                            .from(Sessions::Table, Sessions::UserId)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_sessions_user")
                    .table(Sessions::Table)
                    .col(Sessions::UserId)
                    .to_owned(),
            )
            .await?;

        // --- webauthn_challenges --------------------------------------------
        // In-flight ceremony (register or login) state, TTL-bounded. `user_id`
        // is deliberately NOT a foreign key: at register_begin it holds a
        // pre-generated candidate id for a user that does not exist yet (the
        // row is inserted in the finish transaction).
        manager
            .create_table(
                Table::create()
                    .table(WebauthnChallenges::Table)
                    .col(
                        ColumnDef::new(WebauthnChallenges::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(WebauthnChallenges::Kind).text().not_null())
                    .col(ColumnDef::new(WebauthnChallenges::State).json().not_null())
                    .col(ColumnDef::new(WebauthnChallenges::InviteCodeHash).binary())
                    .col(ColumnDef::new(WebauthnChallenges::UserId).text())
                    .col(ColumnDef::new(WebauthnChallenges::Email).text())
                    .col(ColumnDef::new(WebauthnChallenges::Username).text())
                    .col(
                        ColumnDef::new(WebauthnChallenges::ExpiresAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(WebauthnChallenges::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_webauthn_challenges_expires")
                    .table(WebauthnChallenges::Table)
                    .col(WebauthnChallenges::ExpiresAt)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_webauthn_challenges_invite_hash")
                    .table(WebauthnChallenges::Table)
                    .col(WebauthnChallenges::InviteCodeHash)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_webauthn_challenges_user")
                    .table(WebauthnChallenges::Table)
                    .col(WebauthnChallenges::UserId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Child-first.
        manager
            .drop_table(Table::drop().table(WebauthnChallenges::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Sessions::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(InviteCodes::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Passkeys::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(EnrollmentTokens::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(TagmaTokens::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Tagmata::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Users::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
    Username,
    Email,
    DisplayName,
    CreatedAt,
    DisabledAt,
}

#[derive(DeriveIden)]
enum Tagmata {
    Table,
    Id,
    OwnerUserId,
    PinnedPublicKey,
    CreatedAt,
    Label,
    LastTunnelProofTs,
}

#[derive(DeriveIden)]
enum TagmaTokens {
    Table,
    TokenHash,
    TagmaId,
    IssuedAt,
    RevokedAt,
}

#[derive(DeriveIden)]
enum EnrollmentTokens {
    Table,
    Id,
    TokenHash,
    UserId,
    CreatedAt,
    ExpiresAt,
    ConsumedAt,
    ConsumedByTagma,
    RevokedAt,
}

#[derive(DeriveIden)]
enum Passkeys {
    Table,
    Id,
    UserId,
    CredId,
    Credential,
    CreatedAt,
    CompromisedAt,
}

#[derive(DeriveIden)]
enum InviteCodes {
    Table,
    CodeHash,
    CreatedAt,
    ExpiresAt,
    ConsumedAt,
    ConsumedBy,
    Note,
    RevokedAt,
}

#[derive(DeriveIden)]
enum Sessions {
    Table,
    TokenHash,
    UserId,
    CreatedAt,
    ExpiresAt,
}

#[derive(DeriveIden)]
enum WebauthnChallenges {
    Table,
    Id,
    Kind,
    State,
    InviteCodeHash,
    UserId,
    Email,
    Username,
    ExpiresAt,
    CreatedAt,
}
