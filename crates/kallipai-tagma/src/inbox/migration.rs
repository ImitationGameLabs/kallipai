//! sea-orm-migration registration for the inbox store.
//! New schema changes are new `m_*` files appended to `migrations()`, never
//! in-place edits to an existing migration.

pub mod m_20250121_01_init;
pub mod m_20260814_02_columns;

use sea_orm_migration::prelude::*;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m_20250121_01_init::Migration),
            Box::new(m_20260814_02_columns::Migration),
        ]
    }
}
