//! The gateway store's initial schema, in one migration.
//!
//! Three physically separated domains live in this schema (the design doc's
//! dual-face layering):
//!
//! - Registry domain (distribution-face data, no secrets): `profiles` (model
//!   profiles with a `parked` flag for the parking resource, plus the
//!   profile-total quota columns), `profile_sets` + `set_members` (ordered
//!   membership -- `position` carries the failover order and the
//!   distribution face returns it verbatim), `registry_meta` (key/value;
//!   holds the registry-level `default_set` marker).
//! - Secret domain (secret module only): `upstream_credentials`
//!   (per-profile provider endpoint prefix + API key -- never selected by
//!   distribution paths), `proxy_keys` (hash-only proxy keys: the
//!   `TokenHash` bytes in a Postgres `BYTEA`, keyed for lookup, plus the
//!   expiry pair), and `proxy_key_sets` (a key's allowed sets -- the
//!   set-granular authorization unit from the design doc).
//! - Identity and audit domain: `tagmas` (the identity row behind every
//!   proxy key, carrying the authorization matrix: several keys of one
//!   tagma share one quota share -- a per-key cap would multiply with the
//!   key count and break the share), `request_audits` (one row per
//!   forwarded request -- the request log and the per-key usage land in
//!   the same row, so the two audit views reconcile by construction),
//!   `key_lifecycle_events` (the issuance/revocation record,
//!   vocabulary CHECK-enforced), and `management_events` (one row per
//!   admin-face mutation, written inside the same transaction as the
//!   change it describes -- fail-closed, the deliberate opposite of the
//!   forwarding path's best-effort `request_audits` write).
//!
//! Type notes carried by the columns:
//!
//! - Quota and budget numbers are BIGINT micro-units (10^-6 of the account
//!   currency): integer money, no floating point, no decimal dependency.
//! - `tagmas.account_id` is the nullable cloud-account placeholder: the
//!   matrix key stays single-dimensional (tagma), the second dimension is
//!   semantic reservation only -- no account system is implemented.
//! - `proxy_keys.created_at` is NOT NULL with no default: every write
//!   supplies the issuance time explicitly.
//! - `key_lifecycle_events.event` is CHECK-enforced to the vocabulary
//!   ("issued" | "revoked" | "expired"): the column's only writer is the
//!   key admin face's fail-closed transaction, so a typo'd event name must
//!   fail that write, not linger as a row no reader can interpret.
//! - `management_events.action`/`entity` stay free text on purpose: the
//!   admin face is the only writer and the vocabulary grows with it.
//!
//! Two statements are raw SQL because the schema builder has no helper for
//! them: the CHECK constraint above, and the `proxy_keys -> tagmas`
//! foreign key (an unprepared statement beats a clever workaround -- the
//! files init migration note; the explicit constraint names survive in the
//! database).
//!
//! Secondary indexes are separate `create_index` calls rather than inline:
//! Postgres `CREATE TABLE` only accepts `UNIQUE`/`PRIMARY KEY` as table
//! constraints, so sea-query would emit invalid SQL for an inline non-unique
//! index (house note carried from the files init migration).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // --- tagmas ----------------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(Tagmas::Table)
                    .col(
                        ColumnDef::new(Tagmas::TagmaId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    // Cloud-account placeholder (design doc decision 5: tagma
                    // identity != user account in the cloud shape). Always
                    // NULL until an account system exists.
                    .col(ColumnDef::new(Tagmas::AccountId).text())
                    .col(ColumnDef::new(Tagmas::MaxBudget).big_integer())
                    .col(ColumnDef::new(Tagmas::TpmLimit).big_integer())
                    .col(ColumnDef::new(Tagmas::RpmLimit).big_integer())
                    .to_owned(),
            )
            .await?;

        // --- profiles --------------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(Profiles::Table)
                    .col(
                        ColumnDef::new(Profiles::ProfileId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Profiles::Family).text().not_null())
                    .col(ColumnDef::new(Profiles::Model).text().not_null())
                    .col(ColumnDef::new(Profiles::MaxContextWindow).big_integer())
                    .col(ColumnDef::new(Profiles::Effort).text())
                    .col(ColumnDef::new(Profiles::Modalities).text())
                    .col(
                        ColumnDef::new(Profiles::Parked)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    // The profile-total quota dimension: the proxy-wide cap
                    // every tagma's consumption counts against (nullable =
                    // unlimited by default).
                    .col(ColumnDef::new(Profiles::MaxBudget).big_integer())
                    .col(ColumnDef::new(Profiles::TpmLimit).big_integer())
                    .col(ColumnDef::new(Profiles::RpmLimit).big_integer())
                    .to_owned(),
            )
            .await?;

        // --- profile_sets ----------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(ProfileSets::Table)
                    .col(
                        ColumnDef::new(ProfileSets::Name)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ProfileSets::Description).text().not_null())
                    .to_owned(),
            )
            .await?;

        // --- set_members -----------------------------------------------------
        // `position` is the failover order (profiles[0] is the active
        // deployment); the distribution face must return it verbatim.
        // (set_name, position) is unique: two members cannot hold the
        // same failover slot in a set (one member holding two slots is the
        // (set_name, profile_id) primary key's case).
        manager
            .create_table(
                Table::create()
                    .table(SetMembers::Table)
                    .col(ColumnDef::new(SetMembers::SetName).text().not_null())
                    .col(ColumnDef::new(SetMembers::ProfileId).text().not_null())
                    .col(ColumnDef::new(SetMembers::Position).integer().not_null())
                    .primary_key(
                        Index::create()
                            .col(SetMembers::SetName)
                            .col(SetMembers::ProfileId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(SetMembers::Table, SetMembers::SetName)
                            .to(ProfileSets::Table, ProfileSets::Name)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(SetMembers::Table, SetMembers::ProfileId)
                            .to(Profiles::Table, Profiles::ProfileId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_set_members_set_name")
                    .table(SetMembers::Table)
                    .col(SetMembers::SetName)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uq_set_members_set_position")
                    .table(SetMembers::Table)
                    .col(SetMembers::SetName)
                    .col(SetMembers::Position)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // --- registry_meta ---------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(RegistryMeta::Table)
                    .col(
                        ColumnDef::new(RegistryMeta::Key)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(RegistryMeta::Value).text().not_null())
                    .to_owned(),
            )
            .await?;

        // --- upstream_credentials (secret domain) ---------------------------
        manager
            .create_table(
                Table::create()
                    .table(UpstreamCredentials::Table)
                    .col(
                        ColumnDef::new(UpstreamCredentials::ProfileId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(UpstreamCredentials::UpstreamBaseUrl)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UpstreamCredentials::UpstreamApiKey)
                            .text()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(UpstreamCredentials::Table, UpstreamCredentials::ProfileId)
                            .to(Profiles::Table, Profiles::ProfileId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // --- proxy_keys (secret domain) --------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(ProxyKeys::Table)
                    .col(
                        ColumnDef::new(ProxyKeys::KeyHash)
                            .binary()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ProxyKeys::TagmaId).text().not_null())
                    .col(ColumnDef::new(ProxyKeys::ExpiresAt).timestamp_with_time_zone())
                    .col(
                        ColumnDef::new(ProxyKeys::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE proxy_keys ADD CONSTRAINT fk_proxy_keys_tagma \
                 FOREIGN KEY (tagma_id) REFERENCES tagmas (tagma_id)",
            )
            .await?;

        // --- proxy_key_sets (secret domain) ----------------------------------
        manager
            .create_table(
                Table::create()
                    .table(ProxyKeySets::Table)
                    .col(ColumnDef::new(ProxyKeySets::KeyHash).binary().not_null())
                    .col(ColumnDef::new(ProxyKeySets::SetName).text().not_null())
                    .primary_key(
                        Index::create()
                            .col(ProxyKeySets::KeyHash)
                            .col(ProxyKeySets::SetName),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(ProxyKeySets::Table, ProxyKeySets::KeyHash)
                            .to(ProxyKeys::Table, ProxyKeys::KeyHash)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(ProxyKeySets::Table, ProxyKeySets::SetName)
                            .to(ProfileSets::Table, ProfileSets::Name)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_proxy_key_sets_key_hash")
                    .table(ProxyKeySets::Table)
                    .col(ProxyKeySets::KeyHash)
                    .to_owned(),
            )
            .await?;

        // --- request_audits --------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(RequestAudits::Table)
                    .col(
                        ColumnDef::new(RequestAudits::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(RequestAudits::KeyHash).binary().not_null())
                    .col(ColumnDef::new(RequestAudits::TagmaId).text().not_null())
                    .col(ColumnDef::new(RequestAudits::ProfileId).text().not_null())
                    // The served model is the profile's deployment model; the
                    // request body is never parsed, so the client's requested
                    // model name is not available to this row.
                    .col(ColumnDef::new(RequestAudits::Model).text().not_null())
                    .col(
                        ColumnDef::new(RequestAudits::StatusCode)
                            .integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RequestAudits::DurationMs)
                            .big_integer()
                            .not_null(),
                    )
                    // Token counts are NULL whenever the upstream response
                    // carried no usage block (streaming without the client's
                    // include_usage option).
                    .col(ColumnDef::new(RequestAudits::PromptTokens).big_integer())
                    .col(ColumnDef::new(RequestAudits::CompletionTokens).big_integer())
                    .col(ColumnDef::new(RequestAudits::TotalTokens).big_integer())
                    // NULL until the pricing face lands (micro-units, see the
                    // module doc); the max_budget check sums this column.
                    .col(ColumnDef::new(RequestAudits::CostMicros).big_integer())
                    .col(
                        ColumnDef::new(RequestAudits::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_request_audits_tagma_time")
                    .table(RequestAudits::Table)
                    .col(RequestAudits::TagmaId)
                    .col(RequestAudits::CreatedAt)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_request_audits_key_time")
                    .table(RequestAudits::Table)
                    .col(RequestAudits::KeyHash)
                    .col(RequestAudits::CreatedAt)
                    .to_owned(),
            )
            .await?;

        // --- key_lifecycle_events --------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(KeyLifecycleEvents::Table)
                    .col(
                        ColumnDef::new(KeyLifecycleEvents::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(KeyLifecycleEvents::KeyHash)
                            .binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(KeyLifecycleEvents::TagmaId)
                            .text()
                            .not_null(),
                    )
                    // Vocabulary: "issued" | "revoked" | "expired" (the key
                    // admin face is the only writer), enforced by the raw
                    // CHECK below.
                    .col(ColumnDef::new(KeyLifecycleEvents::Event).text().not_null())
                    .col(ColumnDef::new(KeyLifecycleEvents::Detail).text())
                    .col(
                        ColumnDef::new(KeyLifecycleEvents::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_key_lifecycle_events_key")
                    .table(KeyLifecycleEvents::Table)
                    .col(KeyLifecycleEvents::KeyHash)
                    .to_owned(),
            )
            .await?;
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE key_lifecycle_events ADD CONSTRAINT \
                 ck_key_lifecycle_events_event \
                 CHECK (event IN ('issued', 'revoked', 'expired'))",
            )
            .await?;

        // --- management_events ------------------------------------------------
        // `before`/`after` carry the full row state as JSONB (NULL on the
        // absent side of a create/delete). Secret material never enters this
        // table: the `upstream_credential` rows are written masked (the
        // plaintext-only-in contract covers the audit log too). The actor is
        // the static management token for now ("management-token"); a future
        // dynamic credential writes the resolved principal here.
        manager
            .create_table(
                Table::create()
                    .table(ManagementEvents::Table)
                    .col(
                        ColumnDef::new(ManagementEvents::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ManagementEvents::Action).text().not_null())
                    .col(ColumnDef::new(ManagementEvents::Entity).text().not_null())
                    .col(ColumnDef::new(ManagementEvents::EntityId).text().not_null())
                    .col(ColumnDef::new(ManagementEvents::Actor).text().not_null())
                    .col(ColumnDef::new(ManagementEvents::Before).json_binary())
                    .col(ColumnDef::new(ManagementEvents::After).json_binary())
                    .col(
                        ColumnDef::new(ManagementEvents::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_management_events_time")
                    .table(ManagementEvents::Table)
                    .col(ManagementEvents::CreatedAt)
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE key_lifecycle_events \
                 DROP CONSTRAINT IF EXISTS ck_key_lifecycle_events_event",
            )
            .await?;
        manager
            .drop_table(Table::drop().table(ManagementEvents::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(KeyLifecycleEvents::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(RequestAudits::Table).to_owned())
            .await?;
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE proxy_keys DROP CONSTRAINT IF EXISTS fk_proxy_keys_tagma",
            )
            .await?;
        manager
            .drop_table(Table::drop().table(ProxyKeySets::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(ProxyKeys::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(UpstreamCredentials::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(RegistryMeta::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(SetMembers::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(ProfileSets::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Profiles::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Tagmas::Table).to_owned())
            .await?;
        Ok(())
    }
}

// --- column/table identifiers -----------------------------------------------

#[derive(DeriveIden)]
enum Tagmas {
    Table,
    TagmaId,
    AccountId,
    MaxBudget,
    TpmLimit,
    RpmLimit,
}

#[derive(DeriveIden)]
enum Profiles {
    Table,
    ProfileId,
    Family,
    Model,
    MaxContextWindow,
    Effort,
    Modalities,
    Parked,
    MaxBudget,
    TpmLimit,
    RpmLimit,
}

#[derive(DeriveIden)]
enum ProfileSets {
    Table,
    Name,
    Description,
}

#[derive(DeriveIden)]
enum SetMembers {
    Table,
    SetName,
    ProfileId,
    Position,
}

#[derive(DeriveIden)]
enum RegistryMeta {
    Table,
    Key,
    Value,
}

#[derive(DeriveIden)]
enum UpstreamCredentials {
    Table,
    ProfileId,
    UpstreamBaseUrl,
    UpstreamApiKey,
}

#[derive(DeriveIden)]
enum ProxyKeys {
    Table,
    KeyHash,
    TagmaId,
    ExpiresAt,
    CreatedAt,
}

#[derive(DeriveIden)]
enum ProxyKeySets {
    Table,
    KeyHash,
    SetName,
}

#[derive(DeriveIden)]
enum RequestAudits {
    Table,
    Id,
    KeyHash,
    TagmaId,
    ProfileId,
    Model,
    StatusCode,
    DurationMs,
    PromptTokens,
    CompletionTokens,
    TotalTokens,
    CostMicros,
    CreatedAt,
}

#[derive(DeriveIden)]
enum KeyLifecycleEvents {
    Table,
    Id,
    KeyHash,
    TagmaId,
    Event,
    Detail,
    CreatedAt,
}

#[derive(DeriveIden)]
enum ManagementEvents {
    Table,
    Id,
    Action,
    Entity,
    EntityId,
    Actor,
    Before,
    After,
    CreatedAt,
}
