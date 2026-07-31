//! `device_pairing_codes` — short-lived, single-use, account-scoped codes minted
//! by an authenticated device so a NEW device can enroll its own passkey onto an
//! existing account (the cross-device bootstrap that "Add device" cannot cover,
//! since the new device has no session/passkey). The TTL is server-defined (see
//! `routes::device_pairing::PAIR_CODE_TTL`).
//!
//! Shape mirrors `invite_codes` (a hashed single-use redeem code) but the
//! lifecycle is the opposite on every axis: user-minted (not admin), a
//! sub-day TTL (not days), bound to a specific `user_id` at mint with
//! `ON DELETE CASCADE` (not `consumed_by ... SET NULL` audit), no `note`, no
//! operator-revoke. A separate table isolates this lifecycle from `invite_codes`
//! rather than overloading it with a discriminator (a forgotten `kind` filter
//! would silently mix the two redeem flows).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Each migration redeclares its own `DeriveIden` enums (the init file's enums
// are private to it and cannot be `use`d across files).
#[derive(DeriveIden)]
enum DevicePairingCodes {
    Table,
    CodeHash,
    UserId,
    CreatedAt,
    ExpiresAt,
    ConsumedAt,
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(DevicePairingCodes::Table)
                    .col(
                        ColumnDef::new(DevicePairingCodes::CodeHash)
                            .binary()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(DevicePairingCodes::UserId).text().not_null())
                    .col(
                        ColumnDef::new(DevicePairingCodes::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DevicePairingCodes::ExpiresAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(DevicePairingCodes::ConsumedAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_device_pairing_codes_user")
                            .from(DevicePairingCodes::Table, DevicePairingCodes::UserId)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_device_pairing_codes_user")
                    .table(DevicePairingCodes::Table)
                    .col(DevicePairingCodes::UserId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Reversible: pairing codes are ephemeral data, not a schema split.
        manager
            .drop_table(Table::drop().table(DevicePairingCodes::Table).to_owned())
            .await
    }
}
