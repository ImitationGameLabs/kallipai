//! One credential table for both owners: `access_tokens` replaces
//! `tagma_tokens` and gains a permission set, a name, expiry, last use, and the
//! audit table `credential_events`.
//!
//! A tagma keeps exactly one live record (partial unique index on
//! `owner_kind = 'tagma'`); a user may hold several. Revocation stays on the
//! owning identity (`tagmata.revoked_at`) for a tagma, and `revoked_at` here
//! for a personal token.
//!
//! Existing `tagma_tokens` rows are carried across with their hash unchanged --
//! a running tagma keeps authenticating without re-enrolling -- and given the
//! tagma default set (the three `files:*` strings): a tagma reaches no registry
//! route, so those are the only calls its set can govern.
//! Their `masked` stays NULL: a carried row never recorded a plaintext tail, so
//! there is nothing to show.

use sea_orm::sea_query::Value;
use sea_orm::{ConnectionTrait, DatabaseBackend, FromQueryResult, Statement};
use sea_orm_migration::prelude::*;
use time::OffsetDateTime;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum AccessTokens {
    Table,
    Id,
    OwnerKind,
    OwnerId,
    Name,
    TokenHash,
    Masked,
    Perms,
    CreatedAt,
    CreatedBy,
    ExpiresAt,
    LastUsedAt,
    RevokedAt,
}

#[derive(DeriveIden)]
enum CredentialEvents {
    Table,
    Id,
    TokenId,
    At,
    ActorUserId,
    Kind,
    Added,
    Removed,
}

#[derive(DeriveIden)]
enum TagmaTokens {
    Table,
}

/// One carried-over `tagma_tokens` row plus its owner, read before the drop.
#[derive(FromQueryResult)]
struct LegacyTagmaToken {
    token_hash: Vec<u8>,
    tagma_id: String,
    issued_at: OffsetDateTime,
    owner_user_id: String,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AccessTokens::Table)
                    .col(
                        ColumnDef::new(AccessTokens::Id)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(AccessTokens::OwnerKind).text().not_null())
                    .col(ColumnDef::new(AccessTokens::OwnerId).text().not_null())
                    .col(ColumnDef::new(AccessTokens::Name).text().not_null())
                    .col(
                        ColumnDef::new(AccessTokens::TokenHash)
                            .binary()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(AccessTokens::Masked).text())
                    .col(ColumnDef::new(AccessTokens::Perms).json_binary().not_null())
                    .col(
                        ColumnDef::new(AccessTokens::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(AccessTokens::CreatedBy).text().not_null())
                    .col(ColumnDef::new(AccessTokens::ExpiresAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(AccessTokens::LastUsedAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(AccessTokens::RevokedAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(CredentialEvents::Table)
                    .col(
                        ColumnDef::new(CredentialEvents::Id)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(CredentialEvents::TokenId).text().not_null())
                    .col(
                        ColumnDef::new(CredentialEvents::At)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(CredentialEvents::ActorUserId).text())
                    .col(ColumnDef::new(CredentialEvents::Kind).text().not_null())
                    .col(
                        ColumnDef::new(CredentialEvents::Added)
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(CredentialEvents::Removed)
                            .json_binary()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(CredentialEvents::Table, CredentialEvents::TokenId)
                            .to(AccessTokens::Table, AccessTokens::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // sea-query 0.32 has no `Index::create().filter(...)`; a partial unique
        // index is the only way to say "one live credential per tagma", and the
        // WHERE is essential -- revoked rows must not collide.
        let conn = manager.get_connection();
        conn.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "CREATE UNIQUE INDEX uniq_access_tokens_one_live_tagma \
             ON access_tokens (owner_id) \
             WHERE owner_kind = 'tagma' AND revoked_at IS NULL"
                .to_owned(),
        ))
        .await?;
        conn.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "CREATE INDEX idx_access_tokens_owner ON access_tokens (owner_kind, owner_id)"
                .to_owned(),
        ))
        .await?;

        // Carry every existing tagma credential across, hash untouched: a
        // running tagma keeps authenticating. The carried set is the tagma
        // default: an `sk-tagma-` bearer was unrestricted before this table
        // existed, and a tagma reaches no registry route, so the file-service
        // strings are exactly what it could do. Registry strings here would be
        // checkboxes that grant nothing.
        let legacy = LegacyTagmaToken::find_by_statement(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT t.token_hash, t.tagma_id, t.issued_at, g.owner_user_id \
             FROM tagma_tokens t JOIN tagmata g ON g.id = t.tagma_id"
                .to_owned(),
        ))
        .all(conn)
        .await?;
        let perms = serde_json::json!(kallipai_archeion_common::perm::TAGMA_DEFAULTS);
        for row in legacy {
            let token_id = uuid::Uuid::new_v4().to_string();
            conn.execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "INSERT INTO access_tokens (id, owner_kind, owner_id, name, token_hash, masked, \
                 perms, created_at, created_by, expires_at, last_used_at, revoked_at) \
                 VALUES ($1, 'tagma', $2, 'enrollment', $3, NULL, $4, $5, $6, NULL, NULL, NULL)",
                [
                    Value::String(Some(Box::new(token_id.clone()))),
                    Value::String(Some(Box::new(row.tagma_id))),
                    Value::Bytes(Some(Box::new(row.token_hash))),
                    Value::Json(Some(Box::new(perms.clone()))),
                    Value::TimeDateTimeWithTimeZone(Some(Box::new(row.issued_at))),
                    Value::String(Some(Box::new(row.owner_user_id))),
                ],
            ))
            .await?;
            // The carried rows predate the audit table, but a record without a
            // `create` row reads as a broken history. The migration writes that
            // starting point itself: no actor (nobody signed in for it), the
            // set the row now carries, at the time it was issued.
            conn.execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "INSERT INTO credential_events (id, token_id, at, actor_user_id, kind, added, \
                 removed) VALUES ($1, $2, $3, NULL, 'create', $4, $5)",
                [
                    Value::String(Some(Box::new(uuid::Uuid::new_v4().to_string()))),
                    Value::String(Some(Box::new(token_id))),
                    Value::TimeDateTimeWithTimeZone(Some(Box::new(row.issued_at))),
                    Value::Json(Some(Box::new(perms.clone()))),
                    Value::Json(Some(Box::new(serde_json::json!([])))),
                ],
            ))
            .await?;
        }

        manager
            .drop_table(Table::drop().table(TagmaTokens::Table).to_owned())
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Non-reversible: the permission set, the audit trail, and the merged
        // owner have no home in the old shape.
        Err(DbErr::Custom(format!(
            "{} is non-reversible (tagma_tokens folded into access_tokens)",
            self.name()
        )))
    }
}
