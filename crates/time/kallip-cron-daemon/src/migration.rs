//! sea-orm-migration registration. New schema changes are new `m_*` files
//! appended to `migrations()`, never in-place edits to an existing migration.

pub mod m_20260801_01_init;

use sea_orm_migration::prelude::*;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m_20260801_01_init::Migration)]
    }
}
