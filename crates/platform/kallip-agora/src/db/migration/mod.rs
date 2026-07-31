//! Programmatic sea-orm migrations.
//!
//! One `MigrationTrait` per file, registered in [`Migrator`]. Naming follows
//! ephemera-ai: `m_YYYYMMDD_NN_slug` (the `NN` disambiguates multiple migrations
//! on the same day). Applied at boot via `Migrator::up`.

pub use sea_orm_migration::prelude::*;

mod m_20260718_01_init;
mod m_20260720_01_enrollment_token_masked;
mod m_20260720_02_tagma_unified;
mod m_20260730_01_passkey_label_stepup_revocations;
mod m_20260730_02_device_pairing_codes;
mod m_20260731_01_passkey_last_used_at;

/// The agora migrator. New migrations are appended to [`MigratorTrait::migrations`].
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m_20260718_01_init::Migration),
            Box::new(m_20260720_01_enrollment_token_masked::Migration),
            Box::new(m_20260720_02_tagma_unified::Migration),
            Box::new(m_20260730_01_passkey_label_stepup_revocations::Migration),
            Box::new(m_20260730_02_device_pairing_codes::Migration),
            Box::new(m_20260731_01_passkey_last_used_at::Migration),
        ]
    }
}
