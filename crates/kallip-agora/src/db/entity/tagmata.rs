//! `tagmata` entity — a herald (a `kallip-daemon` instance) owned by a user,
//! across its lifecycle: pending (an enrollment code minted, no device key
//! yet), enrolled (a herald connected and pinned its Ed25519 device key), or
//! revoked. `enrolled_at` is the phase marker (`None` = pending); the pending-
//! phase fields (`enrollment_code_hash`, `enrollment_code_masked`,
//! `expires_at`) are cleared on enroll, and `pinned_public_key` is `None` until
//! then. `revoked_at` is the unified revoke flag (checked in `resolve_bearer`
//! for enrolled tagmas and in the enroll handler for pending ones).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tagmata")]
pub struct Model {
    /// `TagmaId` (opaque UUID-string newtype), stored as `TEXT`. Stable from
    /// mint through enroll (a pending code's id becomes the enrolled tagma's
    /// id).
    #[sea_orm(primary_key, column_type = "Text")]
    pub id: String,
    /// `UserId` of the owner. References `users(id)` (`ON DELETE RESTRICT`).
    #[sea_orm(column_type = "Text")]
    pub owner_user_id: String,
    /// 32-byte Ed25519 verifying key pinned at enroll (`Vec<u8>` maps to
    /// Postgres `BYTEA`). `None` while pending.
    pub pinned_public_key: Option<Vec<u8>>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    #[sea_orm(column_type = "Text", nullable)]
    pub label: Option<String>,
    /// High-water-mark of the accepted herald tunnel-proof timestamp (unix
    /// seconds). The tunnel handler accepts a proof only when this is `NULL` or
    /// strictly less than the incoming timestamp, defeating replay across agora
    /// restarts. `None` until the first connect.
    pub last_tunnel_proof_ts: Option<i64>,
    /// Revocation timestamp; `None` = live. The single revoke flag for both the
    /// pending and enrolled phases, checked on every bearer-authed request (an
    /// enrolled revoke cuts the herald off on its next call) and at enroll (a
    /// pending revoke blocks redemption).
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub revoked_at: Option<OffsetDateTime>,
    /// Enrollment timestamp; `None` = pending (the row carries an unredeemed
    /// enrollment code). Set once when a herald connects.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub enrolled_at: Option<OffsetDateTime>,
    /// SHA-256 hash of the pending enrollment code; unique among live pending
    /// tagmas (partial unique index, NULLs excluded). Cleared on enroll.
    /// `Vec<u8>` maps to Postgres `BYTEA` (nullable).
    #[sea_orm(nullable)]
    pub enrollment_code_hash: Option<Vec<u8>>,
    /// Display-safe mask of the pending enrollment code (`sk-enroll-abc***xyz`),
    /// persisted at mint so the list endpoint can show a code without the
    /// unrecoverable plaintext. Cleared on enroll.
    #[sea_orm(column_type = "Text", nullable)]
    pub enrollment_code_masked: Option<String>,
    /// The pending code's expiry. Pending-only; cleared on enroll (an enrolled
    /// tagma does not expire).
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
