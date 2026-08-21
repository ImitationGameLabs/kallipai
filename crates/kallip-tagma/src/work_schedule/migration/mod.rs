//! sea-orm-migration registration for the work-schedule store.
//! New schema changes are new `m_*` files appended to `migrations()`, never
//! in-place edits to an existing migration.

pub mod m_20250120_01_init;
pub mod m_20260821_02_native_spec;
pub mod m_20260821_03_cleanup;
pub mod m_20260821_04_always_seed;

use sea_orm_migration::prelude::*;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m_20250120_01_init::Migration),
            Box::new(m_20260821_02_native_spec::Migration),
            Box::new(m_20260821_03_cleanup::Migration),
            Box::new(m_20260821_04_always_seed::Migration),
        ]
    }
}
