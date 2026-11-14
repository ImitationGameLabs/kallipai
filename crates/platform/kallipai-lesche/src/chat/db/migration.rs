//! Programmatic sea-orm migrations for the lesche durable store.
//!
//! One `MigrationTrait` per file, registered in [`Migrator`]. Naming follows
//! the archeion convention: `m_YYYYMMDD_NN_slug`. Applied at boot via
//! `Migrator::up`.

pub use sea_orm_migration::prelude::*;

mod m_20260804_01_init;
mod m_20260901_01_read_cursors;
mod m_20260902_01_direct;
mod m_20261113_01_thread_root;
mod m_20261114_01_thread_participants;
mod m_20261114_02_thread_participants_backfill;
mod m_20261114_03_thread_participants_mentions;

pub struct Migrator;

impl MigratorTrait for Migrator {
    /// The order of this vector IS the order the migrations run in, and two of
    /// them interact: `m_02` writes a head author as `started` at the head, and
    /// `m_03` only ever moves a member EARLIER, so it leaves that row alone --
    /// run them the other way round (or reorder this list) and a mention lands
    /// first, `m_02`'s `ON CONFLICT DO NOTHING` keeps the mention, and the head
    /// author's `joined_at_seq` drifts to their first reply.
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m_20260804_01_init::Migration),
            Box::new(m_20260901_01_read_cursors::Migration),
            Box::new(m_20260902_01_direct::Migration),
            Box::new(m_20261113_01_thread_root::Migration),
            Box::new(m_20261114_01_thread_participants::Migration),
            Box::new(m_20261114_02_thread_participants_backfill::Migration),
            Box::new(m_20261114_03_thread_participants_mentions::Migration),
        ]
    }
}
