//! `tagmata` entity — a registered herald (a `kallip-daemon` instance) owned by
//! a user, with its pinned Ed25519 device public key.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tagmata")]
pub struct Model {
    /// `TagmaId` (opaque UUID-string newtype), stored as `TEXT`.
    #[sea_orm(primary_key, column_type = "Text")]
    pub id: String,
    /// `UserId` of the owner. References `users(id)` (`ON DELETE RESTRICT`).
    #[sea_orm(column_type = "Text")]
    pub owner_user_id: String,
    /// 32-byte Ed25519 verifying key pinned at enrollment (`Vec<u8>` maps to
    /// Postgres `BYTEA` by default).
    pub pinned_public_key: Vec<u8>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    #[sea_orm(column_type = "Text", nullable)]
    pub label: Option<String>,
    /// High-water-mark of the accepted herald tunnel-proof timestamp (unix
    /// seconds). The tunnel handler accepts a proof only when this is `NULL` or
    /// strictly less than the incoming timestamp, defeating replay across agora
    /// restarts. `None` until the first connect.
    pub last_tunnel_proof_ts: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
