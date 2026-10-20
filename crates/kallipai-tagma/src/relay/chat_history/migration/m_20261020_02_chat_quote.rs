//! Add the nullable `quote` column to `chat_history`: the id of the row this
//! message quotes, or `NULL`. Additive — `ALTER TABLE ... ADD COLUMN` keeps
//! every existing row (they replay as quote-less), matching the wire contract
//! where the field is `serde(default)` both ways.
//!
//! The column carries an id only, never the quoted text: the id space is the
//! store's own (`chat_history.id`), so a quote survives the quoted row being
//! rewritten and a peer can resolve it against its own history. What the id
//! points at is resolved at read time, and a deleted target degrades to a
//! placeholder rather than stale content.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE chat_history ADD COLUMN quote INTEGER;")
            .await?;
        Ok(())
    }
}
