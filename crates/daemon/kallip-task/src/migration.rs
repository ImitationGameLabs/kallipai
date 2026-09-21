//! sea-orm-migration registration for the task store.
//! New schema changes are new `m_*` files appended to `migrations()`, never
//! in-place edits to an existing migration.

pub mod m_20260908_01_init;
pub mod m_20260908_02_archive;
pub mod m_20260921_01_confirmers;
use sea_orm_migration::prelude::*;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m_20260908_01_init::Migration),
            Box::new(m_20260908_02_archive::Migration),
            Box::new(m_20260921_01_confirmers::Migration),
        ]
    }
}
