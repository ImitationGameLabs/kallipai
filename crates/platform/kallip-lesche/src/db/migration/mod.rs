//! Programmatic sea-orm migrations for the lesche durable store.
//!
//! One `MigrationTrait` per file, registered in [`Migrator`]. Naming follows
//! the agora convention: `m_YYYYMMDD_NN_slug`. Applied at boot via
//! `Migrator::up`.

pub use sea_orm_migration::prelude::*;

mod m_20260804_01_init;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m_20260804_01_init::Migration)]
    }
}
