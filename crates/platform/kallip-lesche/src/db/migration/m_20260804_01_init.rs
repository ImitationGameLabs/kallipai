//! The lesche durable store: the multi-member chat domain, all in one schema.
//!
//! Lesche is the data-plane relay and the sole owner of the chat schema
//! (database-per-service). It stores and fans room message payloads; members
//! read them server-side (rooms are plaintext). Three concerns live here:
//!
//! - **Message history** (`room_messages` + `room_message_seq`): the
//!   canonical, append-only log of a room, with a per-room sequence
//!   assigned race-free by [`crate::db::store`]. The sender envelope metadata
//!   stored alongside is the sender's STABLE identity only
//!   (`sender_kind` / `sender_id` / `epoch`) -- the display handle is derived
//!   at read time from the registry (`crate::member_identity`), not
//!   persisted.
//! - **Membership graph** (`rooms`, `room_members`,
//!   `room_member_revocations`, `room_invites`): the room itself, live
//!   membership, an append-only audit of removals, and the pending
//!   invite/accept flow. The live/revocation-audit split applies: no status /
//!   soft-delete column on live rows; a removal is a hard-delete plus an
//!   append-only audit row.
//! - **Visibility** (`rooms.visibility`): private = invite-only, public =
//!   open-access join; both store payloads in plaintext.
//!
//! Identity boundary: `member_id` is a derived opaque conversation-layer id
//! (never a foreign key) -- it unifies users, platform-native tagmas, and (later)
//! external agents into one membership shape on the room surface, so the relay
//! never stores or learns the underlying `user_id` / `tagma_id`. Every
//! `*_user_id` / `passkey_id` is likewise a plain TEXT reference, NOT a FK,
//! because the `users` / `passkeys` tables live in the agora registry. Identity
//! existence is attested at write time via the agora `/internal/*` surface; the
//! auth layer already cuts disabled users off. The agent-free boundary is
//! preserved at the write path (`sender_kind ∈ {human, agent}`, never a
//! daemon-internal agent id). The only FKs here are internal-to-lesche:
//! `room_members` / `room_invites` / `room_member_revocations` -> `rooms`
//! CASCADE.
//!
//! Public-id columns are `TEXT` (opaque UUID-string newtypes from
//! `kallip_common::id_type!`); `UUID` is reserved for synthetic audit/invite-row
//! ids that never cross an id-newtype boundary.
//!
//! Non-unique secondary indexes are separate `create_index` calls (Postgres
//! `CREATE TABLE` rejects inline non-unique indexes).

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum RoomMessages {
    Table,
    RoomId,
    Seq,
    SenderKind,
    SenderId,
    Epoch,
    Ciphertext,
    CreatedAt,
}

#[derive(DeriveIden)]
enum RoomMessageSeq {
    Table,
    RoomId,
    NextSeq,
}

#[derive(DeriveIden)]
enum Rooms {
    Table,
    Id,
    CreatedByUserId,
    CreatedAt,
    MembershipEpoch,
    Visibility,
    Name,
    Description,
}

#[derive(DeriveIden)]
enum RoomMembers {
    Table,
    RoomId,
    MemberId,
    Kind,
    SourceId,
    JoinedAt,
    AddedBy,
}

#[derive(DeriveIden)]
enum RoomMemberRevocations {
    Table,
    Id,
    RoomId,
    MemberId,
    Kind,
    SourceId,
    RevokedBy,
    RevokedAt,
    Reason,
}

#[derive(DeriveIden)]
enum RoomInvites {
    Table,
    Id,
    RoomId,
    InviteeUserId,
    InvitedByUserId,
    CreatedAt,
    ExpiresAt,
    AcceptedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // --- room root -----------------------------------------------------

        // `rooms` -- the room itself. Created FIRST so the ciphertext-history
        // tables and the membership graph can reference `rooms(id)` with
        // ON DELETE CASCADE FKs. Plain TEXT `created_by_user_id` reference (NOT
        // a FK): the `users` table lives in the agora registry.
        manager
            .create_table(
                Table::create()
                    .table(Rooms::Table)
                    .col(ColumnDef::new(Rooms::Id).text().not_null().primary_key())
                    .col(ColumnDef::new(Rooms::CreatedByUserId).text().not_null())
                    .col(
                        ColumnDef::new(Rooms::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    // Membership-version counter: bumped on every add/remove so
                    // the relay can authorize senders against the live
                    // membership. Starts at 1; insert code always supplies it.
                    .col(
                        ColumnDef::new(Rooms::MembershipEpoch)
                            .big_integer()
                            .not_null(),
                    )
                    // `visibility`: private (invite/accept) or public
                    // (open-access join). Immutable after create. TEXT rather
                    // than a Postgres enum: sea-query enum support is uneven and
                    // the entity boundary validates the value on read. Defaults
                    // to 'private' so a bare row insert back-fills cleanly.
                    .col(
                        ColumnDef::new(Rooms::Visibility)
                            .text()
                            .not_null()
                            .default("private"),
                    )
                    // Display metadata, immutable after create. Required at the
                    // API on create (the route rejects an empty name); the
                    // DEFAULT '' is for bare/administrative inserts.
                    .col(ColumnDef::new(Rooms::Name).text().not_null().default(""))
                    .col(
                        ColumnDef::new(Rooms::Description)
                            .text()
                            .not_null()
                            .default(""),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_rooms_created_by")
                    .table(Rooms::Table)
                    .col(Rooms::CreatedByUserId)
                    .to_owned(),
            )
            .await?;

        // --- room-message history ------------------------------------------

        // Per-room monotonic sequence counter. `next_seq` starts at 0; the
        // first append advances it to 1. Cascades with the room so a deleted
        // room leaves no seq row.
        manager
            .create_table(
                Table::create()
                    .table(RoomMessageSeq::Table)
                    .col(
                        ColumnDef::new(RoomMessageSeq::RoomId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(RoomMessageSeq::NextSeq)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_room_message_seq_room")
                            .from(RoomMessageSeq::Table, RoomMessageSeq::RoomId)
                            .to(Rooms::Table, Rooms::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(RoomMessages::Table)
                    .col(ColumnDef::new(RoomMessages::RoomId).text().not_null())
                    .col(ColumnDef::new(RoomMessages::Seq).big_integer().not_null())
                    // `sender_kind` is in {human, agent}; the store API derives
                    // it from a `Participant`, so the agent-free boundary ("no
                    // daemon-internal agent id") is enforced at the write path,
                    // not via a CHECK here.
                    .col(ColumnDef::new(RoomMessages::SenderKind).text().not_null())
                    .col(ColumnDef::new(RoomMessages::SenderId).text().not_null())
                    // No `sender_handle`: the display handle is a derived string
                    // (a function of the immutable `sender_id` + mutable registry
                    // state), resolved at read time from the registry, never
                    // persisted. See `crate::member_identity`.
                    .col(ColumnDef::new(RoomMessages::Epoch).big_integer().not_null())
                    // The room payload: plaintext `RoomMessage` JSON bytes,
                    // stored opaquely. BYTEA.
                    .col(ColumnDef::new(RoomMessages::Ciphertext).binary().not_null())
                    .col(
                        ColumnDef::new(RoomMessages::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(RoomMessages::RoomId)
                            .col(RoomMessages::Seq),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_room_messages_room")
                            .from(RoomMessages::Table, RoomMessages::RoomId)
                            .to(Rooms::Table, Rooms::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // --- membership graph ----------------------------------------------

        // `room_members` -- live membership, composite PK
        // `(room_id, member_id)`; `member_id` / `source_id` are plain TEXT. No
        // subject FK: `member_id` is a derived opaque id, and `source_id` is a
        // plain string (the underlying user / tagma tables live in the agora
        // registry). Member authenticity is enforced at auth time.
        manager
            .create_table(
                Table::create()
                    .table(RoomMembers::Table)
                    .col(ColumnDef::new(RoomMembers::RoomId).text().not_null())
                    .col(ColumnDef::new(RoomMembers::MemberId).text().not_null())
                    .col(ColumnDef::new(RoomMembers::Kind).text().not_null())
                    .col(ColumnDef::new(RoomMembers::SourceId).text().not_null())
                    .col(
                        ColumnDef::new(RoomMembers::JoinedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(RoomMembers::AddedBy).text().not_null())
                    .primary_key(
                        Index::create()
                            .col(RoomMembers::RoomId)
                            .col(RoomMembers::MemberId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_room_members_room")
                            .from(RoomMembers::Table, RoomMembers::RoomId)
                            .to(Rooms::Table, Rooms::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_room_members_source")
                    .table(RoomMembers::Table)
                    .col(RoomMembers::SourceId)
                    .col(RoomMembers::Kind)
                    .to_owned(),
            )
            .await?;
        // Standalone btree on `member_id`: the composite PK
        // `(room_id, member_id)` does not serve queries that filter on
        // `member_id` alone (no btree skip-scan), and two hot paths do exactly
        // that -- `list_rooms` (the caller's "my rooms" list) and the presence
        // fan-out (the rooms a member belongs to).
        manager
            .create_index(
                Index::create()
                    .name("idx_room_members_member")
                    .table(RoomMembers::Table)
                    .col(RoomMembers::MemberId)
                    .to_owned(),
            )
            .await?;

        // `room_member_revocations` -- append-only audit of removed members.
        // `revoked_by` holds a UserId but is intentionally NOT a foreign key:
        // it is an audit fact that must survive the revoker's own account
        // deletion.
        manager
            .create_table(
                Table::create()
                    .table(RoomMemberRevocations::Table)
                    .col(
                        ColumnDef::new(RoomMemberRevocations::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(RoomMemberRevocations::RoomId)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomMemberRevocations::MemberId)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomMemberRevocations::Kind)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomMemberRevocations::SourceId)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomMemberRevocations::RevokedBy)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomMemberRevocations::RevokedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomMemberRevocations::Reason)
                            .text()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_room_member_revocations_room")
                            .from(RoomMemberRevocations::Table, RoomMemberRevocations::RoomId)
                            .to(Rooms::Table, Rooms::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        // Composite index covers both the per-room audit list and a future
        // per-(room, member) re-admission denylist lookup in one index.
        manager
            .create_index(
                Index::create()
                    .name("idx_room_member_revocations_room_member")
                    .table(RoomMemberRevocations::Table)
                    .col(RoomMemberRevocations::RoomId)
                    .col(RoomMemberRevocations::MemberId)
                    .to_owned(),
            )
            .await?;

        // `room_invites` -- the pending invite/accept flow. No user FKs:
        // `invitee_user_id` / `invited_by_user_id` are plain TEXT references to
        // the agora registry's `users`.
        manager
            .create_table(
                Table::create()
                    .table(RoomInvites::Table)
                    .col(
                        ColumnDef::new(RoomInvites::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(RoomInvites::RoomId).text().not_null())
                    .col(ColumnDef::new(RoomInvites::InviteeUserId).text().not_null())
                    .col(
                        ColumnDef::new(RoomInvites::InvitedByUserId)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomInvites::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RoomInvites::ExpiresAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    // NULL until the invitee accepts; acceptance inserts a
                    // `room_members` row and stamps this. NULL therefore
                    // means "pending" without a status column.
                    .col(ColumnDef::new(RoomInvites::AcceptedAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_room_invites_room")
                            .from(RoomInvites::Table, RoomInvites::RoomId)
                            .to(Rooms::Table, Rooms::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_room_invites_invitee")
                    .table(RoomInvites::Table)
                    .col(RoomInvites::InviteeUserId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_room_invites_room")
                    .table(RoomInvites::Table)
                    .col(RoomInvites::RoomId)
                    .to_owned(),
            )
            .await?;

        // Partial unique index: one outstanding unaccepted invite per (room,
        // invitee). Race-free backstop for `create_invite`'s check-then-insert.
        // The predicate is `accepted_at IS NULL` only: Postgres evaluates it at
        // insert time, not as time passes, so an `expires_at` term would not
        // free a slot when an invite later expires. `create_invite` lazily
        // deletes an expired duplicate before inserting to re-open the slot.
        manager
            .get_connection()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "CREATE UNIQUE INDEX IF NOT EXISTS uniq_room_invites_pending \
                 ON room_invites (room_id, invitee_user_id) \
                 WHERE accepted_at IS NULL",
                [],
            ))
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Child-first, the strict reverse of `up()`. Drop the pending-invite
        // partial index and every child table, then the `rooms` root last (all
        // child tables hold ON DELETE CASCADE FKs to it). Tables use
        // `manager.drop_table` (matching the codebase convention); the partial
        // unique index has no sea-query builder here and is dropped with raw SQL.
        manager
            .get_connection()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "DROP INDEX IF EXISTS uniq_room_invites_pending",
                [],
            ))
            .await?;
        manager
            .drop_table(Table::drop().table(RoomInvites::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(RoomMemberRevocations::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(RoomMembers::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(RoomMessages::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(RoomMessageSeq::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Rooms::Table).to_owned())
            .await
    }
}
