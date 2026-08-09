//! The sea-orm entity for the `chat_history` table, flattened out of an inline
//! nested module. Owned by the parent store module, which builds `ActiveModel`s
//! on append and maps `Model` rows to the decoded-read shape.

use sea_orm::entity::prelude::*;

/// One authored chat frame: outbound (agent -> user) or inbound (user ->
/// agent). `id` is a monotonic row id (AUTOINCREMENT: never reused, even
/// after GC), stamped onto the wire as `history_id` so the app can
/// dedup/order across batch replay and live delivery.
///
/// A row is keyed by its **peer**: `user_id` is the conversation partner's
/// `ParticipantId` (a UUID) on the relay path, or `NULL` on the direct path —
/// where `NULL` denotes the operator (the singular, implicit party of the
/// local conversation; the operator carries no identity of their own).
/// `username` is the peer's handle (relay) or `NULL` (direct). Multi-user relay
/// is one partition per `user_id`; the direct path is the single `NULL`
/// partition.
///
/// `direction` (`inbound`/`outbound`) is both the replay discriminator (it
/// selects the wire reply shape) and the author variant (`inbound` is
/// user-authored, `outbound` is agent-authored) — so no separate `sender_kind`
/// is stored. The agent's identity is never persisted: it is the tagma itself,
/// reconstructed at read time via `agent_sender()`. The agent-free boundary is
/// therefore structural: no column can hold an agent id.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "chat_history")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    /// The peer's `ParticipantId` (relay), or `NULL` (direct = the operator).
    #[sea_orm(column_type = "Text", nullable)]
    pub user_id: Option<String>,
    /// The peer's handle (relay), or `NULL` (direct = the operator).
    #[sea_orm(column_type = "Text", nullable)]
    pub username: Option<String>,
    /// `outbound` (agent -> user) or `inbound` (user -> agent). The replay
    /// loop reads `direction` to decide which wire reply shape to emit, and
    /// it doubles as the author variant.
    #[sea_orm(column_type = "Text")]
    pub direction: String,
    /// The message content (the assistant `content` for outbound, the user
    /// `text` for inbound).
    #[sea_orm(column_type = "Text")]
    pub text: String,
    /// Unix seconds. Indexed indirectly via the id ordering; GC keys off
    /// this. i64 (not OffsetDateTime) to avoid time-format drift in SQLite
    /// and keep GC a plain integer compare.
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
