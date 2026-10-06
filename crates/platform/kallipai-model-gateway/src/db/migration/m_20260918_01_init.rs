//! The gateway store's initial schema: the terminal shape, in one
//! migration.
//!
//! Three physically separated domains live in this schema (the dual-face
//! layering: registry data serves distribution, secrets stay isolated):
//!
//! - Registry domain (distribution-face data, no secrets): `profiles`
//!   (model profiles with a `parked` flag for the parking resource and
//!   the `store` mirror), `profile_sets` + `set_members` (ordered
//!   membership -- `position` carries the failover order and the
//!   distribution face returns it verbatim; a set is born anchored to
//!   its `collection_name`, the single membership authority), the
//!   publishable `collections` above sets (with the `default_set_name`
//!   anchor), the audience family `groups`/`group_members` plus the
//!   platform-side `platform_groups`/`platform_group_members`, the
//!   publication rows `collection_publications`/`platform_publications`
//!   (publication is the row: no state column), and `gateway_selections`
//!   (one row per tagma naming the collection it pulls through).
//! - Secret domain (secret module only): `providers` (the pool row:
//!   family and endpoint) and `provider_credentials` (the API key half,
//!   keyed to its provider) -- never selected by distribution paths.
//! - Audit domain: `management_events` (one row per management-face
//!   mutation, written inside the same transaction as the change it
//!   describes).
//!
//! Ownership lives in archeion: the enrollment lookup answers which
//! account a tagma belongs to, and this store keeps no identity row of
//! its own. The space key `owner` is the catalog space `system` for
//! admin-face rows and the account id for user-space rows. Set names
//! and profile ids are globally unique (`uq_profile_sets_name_global`,
//! `uq_profiles_profile_id_global`), so the by-name and by-id lookups
//! resolve to exactly one row. The reserved audience `everyone` exists
//! as a sentinel row on both group tables (the visibility predicates
//! answer for it directly), and the `baseline` collection is seeded
//! published to it -- the platform's default audience bundle. New
//! catalog sets join `baseline` through the admin face's collection
//! anchoring, not through any migration-side gather.
//!
//! `gateway_selections` carries no foreign key on purpose: the
//! selection is a pointer, not a reference -- a collection the operator
//! renames or deletes leaves the row naming a missing target, and the
//! readers answer an honest 404 instead of a silent cascade loss.
//!
//! Quota and metering tables do not exist here: usage accounting is
//! suspended until a metrics service batch (declared in main.rs).
//!
//! Type notes carried by the columns: the `owner` defaults (`system`)
//! date from the space split and exist so bare maintenance SQL lands
//! in the catalog space; every API write supplies the owner explicitly.
//! `management_events.created_at` is NOT NULL with no default: every
//! write supplies the time explicitly.
//!
//! Down drops every table: the store is disposable (no production
//! deployment exists for this chain).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // --- providers (the pool row) ----------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(Providers::Table)
                    .col(ColumnDef::new(Providers::Owner).text().not_null())
                    .col(ColumnDef::new(Providers::ProviderId).text().not_null())
                    .col(ColumnDef::new(Providers::Family).text().not_null())
                    .col(ColumnDef::new(Providers::BaseUrl).text())
                    .col(
                        ColumnDef::new(Providers::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(Providers::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .primary_key(
                        Index::create()
                            .col(Providers::Owner)
                            .col(Providers::ProviderId),
                    )
                    .to_owned(),
            )
            .await?;

        // --- profiles ---------------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(Profiles::Table)
                    .col(
                        ColumnDef::new(Profiles::Owner)
                            .text()
                            .not_null()
                            .default("system"),
                    )
                    .col(ColumnDef::new(Profiles::ProfileId).text().not_null())
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
                    .col(ColumnDef::new(Profiles::Store).boolean())
                    .col(ColumnDef::new(Profiles::ProviderId).text().not_null())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_profiles_provider")
                            .from(Profiles::Table, (Profiles::ProviderId, Profiles::Owner))
                            .to(Providers::Table, (Providers::ProviderId, Providers::Owner))
                            .on_update(ForeignKeyAction::Cascade)
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .primary_key(
                        Index::create()
                            .col(Profiles::Owner)
                            .col(Profiles::ProfileId),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uq_profiles_profile_id_global")
                    .table(Profiles::Table)
                    .col(Profiles::ProfileId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // --- collections --------------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(Collections::Table)
                    .col(ColumnDef::new(Collections::Owner).text().not_null())
                    .col(ColumnDef::new(Collections::Name).text().not_null())
                    .col(ColumnDef::new(Collections::Description).text().not_null())
                    .col(ColumnDef::new(Collections::DefaultSetName).text())
                    .primary_key(
                        Index::create()
                            .col(Collections::Owner)
                            .col(Collections::Name),
                    )
                    .to_owned(),
            )
            .await?;

        // --- profile_sets ---------------------------------------------------------
        // A set is born anchored: `collection_name` is the single
        // authority, set at creation with no mirror rows anywhere.
        // The composite foreign key keeps the anchor inside
        // the set's own space, so a cross-space anchor cannot exist.
        manager
            .create_table(
                Table::create()
                    .table(ProfileSets::Table)
                    .col(
                        ColumnDef::new(ProfileSets::Owner)
                            .text()
                            .not_null()
                            .default("system"),
                    )
                    .col(ColumnDef::new(ProfileSets::Name).text().not_null())
                    .col(ColumnDef::new(ProfileSets::Description).text().not_null())
                    .col(
                        ColumnDef::new(ProfileSets::CollectionName)
                            .text()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(ProfileSets::Owner)
                            .col(ProfileSets::Name),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("profile_sets_owner_collection_name_fkey")
                            .from(
                                ProfileSets::Table,
                                (ProfileSets::Owner, ProfileSets::CollectionName),
                            )
                            .to(Collections::Table, (Collections::Owner, Collections::Name))
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uq_profile_sets_name_global")
                    .table(ProfileSets::Table)
                    .col(ProfileSets::Name)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // --- set_members ----------------------------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(SetMembers::Table)
                    .col(
                        ColumnDef::new(SetMembers::Owner)
                            .text()
                            .not_null()
                            .default("system"),
                    )
                    .col(ColumnDef::new(SetMembers::SetName).text().not_null())
                    .col(ColumnDef::new(SetMembers::ProfileId).text().not_null())
                    .col(ColumnDef::new(SetMembers::Position).integer().not_null())
                    .primary_key(
                        Index::create()
                            .col(SetMembers::Owner)
                            .col(SetMembers::SetName)
                            .col(SetMembers::ProfileId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_set_members_set")
                            .from(SetMembers::Table, (SetMembers::Owner, SetMembers::SetName))
                            .to(ProfileSets::Table, (ProfileSets::Owner, ProfileSets::Name))
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_set_members_profile")
                            .from(
                                SetMembers::Table,
                                (SetMembers::Owner, SetMembers::ProfileId),
                            )
                            .to(Profiles::Table, (Profiles::Owner, Profiles::ProfileId))
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_set_members_owner_set_name")
                    .table(SetMembers::Table)
                    .col(SetMembers::Owner)
                    .col(SetMembers::SetName)
                    .to_owned(),
            )
            .await?;
        // The position uniqueness is per set (inside its space): one row
        // per (owner, set, position), the failover order's carrier.
        manager
            .create_index(
                Index::create()
                    .name("uq_set_members_set_position")
                    .table(SetMembers::Table)
                    .col(SetMembers::Owner)
                    .col(SetMembers::SetName)
                    .col(SetMembers::Position)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // --- groups (the user-side audience family) -------------------------------
        manager
            .create_table(
                Table::create()
                    .table(Groups::Table)
                    .col(
                        ColumnDef::new(Groups::GroupId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Groups::Owner).text().not_null())
                    .col(ColumnDef::new(Groups::Name).text().not_null())
                    .col(
                        ColumnDef::new(Groups::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uq_groups_owner_name")
                    .table(Groups::Table)
                    .col(Groups::Owner)
                    .col(Groups::Name)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(GroupMembers::Table)
                    .col(ColumnDef::new(GroupMembers::GroupId).text().not_null())
                    .col(
                        ColumnDef::new(GroupMembers::MemberAccount)
                            .text()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(GroupMembers::GroupId)
                            .col(GroupMembers::MemberAccount),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(GroupMembers::Table, GroupMembers::GroupId)
                            .to(Groups::Table, Groups::GroupId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(CollectionPublications::Table)
                    .col(
                        ColumnDef::new(CollectionPublications::Owner)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(CollectionPublications::CollectionName)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(CollectionPublications::GroupId)
                            .text()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(CollectionPublications::Owner)
                            .col(CollectionPublications::CollectionName)
                            .col(CollectionPublications::GroupId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(
                                CollectionPublications::Table,
                                (
                                    CollectionPublications::Owner,
                                    CollectionPublications::CollectionName,
                                ),
                            )
                            .to(Collections::Table, (Collections::Owner, Collections::Name))
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(
                                CollectionPublications::Table,
                                CollectionPublications::GroupId,
                            )
                            .to(Groups::Table, Groups::GroupId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // --- the platform-side audience family ------------------------------------
        manager
            .create_table(
                Table::create()
                    .table(PlatformGroups::Table)
                    .col(
                        ColumnDef::new(PlatformGroups::GroupId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(PlatformGroups::Owner).text().not_null())
                    .col(ColumnDef::new(PlatformGroups::Name).text().not_null())
                    .col(
                        ColumnDef::new(PlatformGroups::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uq_platform_groups_owner_name")
                    .table(PlatformGroups::Table)
                    .col(PlatformGroups::Owner)
                    .col(PlatformGroups::Name)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(PlatformGroupMembers::Table)
                    .col(
                        ColumnDef::new(PlatformGroupMembers::GroupId)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PlatformGroupMembers::MemberAccount)
                            .text()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(PlatformGroupMembers::GroupId)
                            .col(PlatformGroupMembers::MemberAccount),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(PlatformGroupMembers::Table, PlatformGroupMembers::GroupId)
                            .to(PlatformGroups::Table, PlatformGroups::GroupId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(PlatformPublications::Table)
                    .col(
                        ColumnDef::new(PlatformPublications::Owner)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PlatformPublications::CollectionName)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PlatformPublications::GroupId)
                            .text()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(PlatformPublications::Owner)
                            .col(PlatformPublications::CollectionName)
                            .col(PlatformPublications::GroupId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(
                                PlatformPublications::Table,
                                (
                                    PlatformPublications::Owner,
                                    PlatformPublications::CollectionName,
                                ),
                            )
                            .to(Collections::Table, (Collections::Owner, Collections::Name))
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(PlatformPublications::Table, PlatformPublications::GroupId)
                            .to(PlatformGroups::Table, PlatformGroups::GroupId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // --- provider credentials (the secret half of the pool) ---------------------
        manager
            .create_table(
                Table::create()
                    .table(ProviderCredentials::Table)
                    .col(ColumnDef::new(ProviderCredentials::Owner).text().not_null())
                    .col(
                        ColumnDef::new(ProviderCredentials::ProviderId)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ProviderCredentials::ApiKey)
                            .text()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(ProviderCredentials::Owner)
                            .col(ProviderCredentials::ProviderId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(
                                ProviderCredentials::Table,
                                (ProviderCredentials::Owner, ProviderCredentials::ProviderId),
                            )
                            .to(Providers::Table, (Providers::Owner, Providers::ProviderId))
                            .on_update(ForeignKeyAction::Cascade)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // --- gateway selections (the per-tagma pointer; no FK on purpose) -----------
        manager
            .create_table(
                Table::create()
                    .table(GatewaySelections::Table)
                    .col(
                        ColumnDef::new(GatewaySelections::TagmaId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(GatewaySelections::Owner).text().not_null())
                    .col(
                        ColumnDef::new(GatewaySelections::CollectionName)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(GatewaySelections::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        // --- management events (the audit domain) -----------------------------------
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

        // --- seeds: the everyone sentinels and the baseline bundle ------------------
        // Idempotent (`ON CONFLICT DO NOTHING`): a rerun or a fresh
        // database converges to the same shape.
        let conn = manager.get_connection();
        conn.execute_unprepared(
            "INSERT INTO groups (group_id, owner, name, created_at) \
             VALUES ('everyone', 'system', 'everyone', now()) \
             ON CONFLICT (group_id) DO NOTHING",
        )
        .await?;
        conn.execute_unprepared(
            "INSERT INTO platform_groups (group_id, owner, name, created_at) \
             VALUES ('everyone', 'system', 'everyone', now()) \
             ON CONFLICT (group_id) DO NOTHING",
        )
        .await?;
        conn.execute_unprepared(
            "INSERT INTO collections (owner, name, description) \
             VALUES ('system', 'baseline', 'the platform default audience') \
             ON CONFLICT DO NOTHING",
        )
        .await?;
        conn.execute_unprepared(
            "INSERT INTO platform_publications (owner, collection_name, group_id) \
             VALUES ('system', 'baseline', 'everyone') \
             ON CONFLICT DO NOTHING",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Children before parents (the foreign keys drop with their
        // tables); publication and member rows go before the audience
        // rows they reference.
        manager
            .drop_table(Table::drop().table(SetMembers::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(ProviderCredentials::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(ProfileSets::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Profiles::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(GroupMembers::Table).to_owned())
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(CollectionPublications::Table)
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(PlatformGroupMembers::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(PlatformPublications::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(GatewaySelections::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(ManagementEvents::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Collections::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Groups::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(PlatformGroups::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Providers::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Providers {
    Table,
    Owner,
    ProviderId,
    Family,
    BaseUrl,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum Profiles {
    Table,
    Owner,
    ProfileId,
    Model,
    MaxContextWindow,
    Effort,
    Modalities,
    Parked,
    Store,
    ProviderId,
}

#[derive(DeriveIden)]
enum Collections {
    Table,
    Owner,
    Name,
    Description,
    DefaultSetName,
}

#[derive(DeriveIden)]
enum ProfileSets {
    Table,
    Owner,
    Name,
    Description,
    CollectionName,
}

#[derive(DeriveIden)]
enum SetMembers {
    Table,
    Owner,
    SetName,
    ProfileId,
    Position,
}

#[derive(DeriveIden)]
enum Groups {
    Table,
    GroupId,
    Owner,
    Name,
    CreatedAt,
}

#[derive(DeriveIden)]
enum GroupMembers {
    Table,
    GroupId,
    MemberAccount,
}

#[derive(DeriveIden)]
enum CollectionPublications {
    Table,
    Owner,
    CollectionName,
    GroupId,
}

#[derive(DeriveIden)]
enum PlatformGroups {
    Table,
    GroupId,
    Owner,
    Name,
    CreatedAt,
}

#[derive(DeriveIden)]
enum PlatformGroupMembers {
    Table,
    GroupId,
    MemberAccount,
}

#[derive(DeriveIden)]
enum PlatformPublications {
    Table,
    Owner,
    CollectionName,
    GroupId,
}

#[derive(DeriveIden)]
enum ProviderCredentials {
    Table,
    Owner,
    ProviderId,
    ApiKey,
}

#[derive(DeriveIden)]
enum GatewaySelections {
    Table,
    TagmaId,
    Owner,
    CollectionName,
    UpdatedAt,
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
