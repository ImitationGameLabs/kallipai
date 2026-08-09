//! Programmatic sea-orm migrations for the tagma's chat-history SQLite store.
//!
//! One `MigrationTrait` per file, registered in [`Migrator`]. Applied at open
//! via `Migrator::up`. Naming mirrors agora's `m_YYYYMMDD_NN_slug` (the `NN`
//! disambiguates multiple migrations on the same day). New schema changes are
//! a new `m_*` file appended to [`MigratorTrait::migrations`], never an in-place
//! edit to an applied migration.

pub use sea_orm_migration::prelude::*;

mod m_20260726_01_init;
mod m_20260804_01_typed_history;
mod m_20260809_01_peer_keyed;

/// The chat-history migrator. New migrations are appended to
/// [`MigratorTrait::migrations`].
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m_20260726_01_init::Migration),
            Box::new(m_20260804_01_typed_history::Migration),
            Box::new(m_20260809_01_peer_keyed::Migration),
        ]
    }
}
