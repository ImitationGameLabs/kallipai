//! Programmatic sea-orm migrations.
//!
//! One `MigrationTrait` per file, registered in [`Migrator`]. Naming follows
//! ephemera-ai: `m_YYYYMMDD_NN_slug` (the `NN` disambiguates multiple migrations
//! on the same day). Applied at boot via `Migrator::up`.

pub use sea_orm_migration::prelude::*;

mod m_20260718_01_init;

/// The agora migrator. New migrations are appended to [`MigratorTrait::migrations`].
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m_20260718_01_init::Migration)]
    }
}
