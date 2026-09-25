//! Programmatic sea-orm migrations for the lesche durable store.
//!
//! One `MigrationTrait` per file, registered in [`Migrator`]. Naming follows
//! the archeion convention: `m_YYYYMMDD_NN_slug`. Applied at boot via
//! `Migrator::up`.

pub use sea_orm_migration::prelude::*;

mod m_20260804_01_init;
mod m_20260901_01_read_cursors;
mod m_20260902_01_direct;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m_20260804_01_init::Migration),
            Box::new(m_20260901_01_read_cursors::Migration),
            Box::new(m_20260902_01_direct::Migration),
        ]
    }
}
