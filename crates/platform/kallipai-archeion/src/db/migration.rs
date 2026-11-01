//! Programmatic sea-orm migrations.
//!
//! One `MigrationTrait` per file, registered in [`Migrator`]. Naming follows
//! ephemera-ai: `m_YYYYMMDD_NN_slug` (the `NN` disambiguates multiple migrations
//! on the same day). Applied at boot via `Migrator::up`.

pub use sea_orm_migration::prelude::*;

mod m_20260718_01_init;
mod m_20260720_01_enrollment_token_masked;
mod m_20260720_02_tagma_unified;
mod m_20260730_01_passkey_label_stepup_revocations;
mod m_20260730_02_device_pairing_codes;
mod m_20260731_01_passkey_last_used_at;
mod m_20260807_01_identity_rework;
mod m_20260808_01_oauth_signup_username;
mod m_20260826_01_user_providers;
mod m_20261004_01_drop_user_providers;
mod m_20261101_01_access_tokens;
mod m_20261102_01_credential_event_target;

/// The archeion migrator. New migrations are appended to [`MigratorTrait::migrations`].
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m_20260718_01_init::Migration),
            Box::new(m_20260720_01_enrollment_token_masked::Migration),
            Box::new(m_20260720_02_tagma_unified::Migration),
            Box::new(m_20260730_01_passkey_label_stepup_revocations::Migration),
            Box::new(m_20260730_02_device_pairing_codes::Migration),
            Box::new(m_20260731_01_passkey_last_used_at::Migration),
            Box::new(m_20260807_01_identity_rework::Migration),
            Box::new(m_20260808_01_oauth_signup_username::Migration),
            Box::new(m_20260826_01_user_providers::Migration),
            Box::new(m_20261004_01_drop_user_providers::Migration),
            Box::new(m_20261101_01_access_tokens::Migration),
            Box::new(m_20261102_01_credential_event_target::Migration),
        ]
    }
}

#[cfg(test)]
mod tests {
    //! The data move is the part a schema diff cannot show: a tagma's token,
    //! already in production, must keep authenticating afterwards with the
    //! rights it had.

    use super::*;
    use crate::db::entity::{access_tokens, credential_events, tagmata, users};
    use crate::test_helpers::create_test_db;
    use kallipai_common::authtoken::TokenHash;
    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseBackend,
        EntityTrait, QueryFilter, Statement,
    };
    use time::OffsetDateTime;

    /// The legacy schema is every migration before the one that folds
    /// `tagma_tokens` into `access_tokens`.
    const LEGACY_STEPS: u32 = 10;

    #[tokio::test]
    async fn tagma_token_carries_across_with_its_hash_and_the_tagma_default_set() {
        let db = create_test_db().await;
        Migrator::up(&db, Some(LEGACY_STEPS))
            .await
            .expect("apply the legacy schema");

        // Postgres keeps microseconds; a whole second survives the round trip
        // exactly, so the timestamp assertion compares like with like.
        let now = OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .expect("zero is a valid nanosecond");
        users::ActiveModel {
            id: Set("u1".to_string()),
            username: Set("owner".to_string()),
            display_name: Set(None),
            created_at: Set(now),
            disabled_at: Set(None),
        }
        .insert(&db)
        .await
        .expect("seed user");
        tagmata::ActiveModel {
            id: Set("t1".to_string()),
            owner_user_id: Set("u1".to_string()),
            pinned_public_key: Set(Some(vec![0u8; 32])),
            created_at: Set(now),
            label: Set(None),
            last_tunnel_proof_ts: Set(None),
            revoked_at: Set(None),
            enrolled_at: Set(Some(now)),
            enrollment_code_hash: Set(None),
            enrollment_code_masked: Set(None),
            expires_at: Set(None),
        }
        .insert(&db)
        .await
        .expect("seed tagma");

        let plaintext = "sk-tagma-legacy-secret";
        let hash = TokenHash::of(plaintext);
        db.execute(sea_orm::Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO tagma_tokens (token_hash, tagma_id, issued_at) VALUES ($1, 't1', $2)",
            [
                sea_orm::sea_query::Value::Bytes(Some(Box::new(hash.as_bytes().to_vec()))),
                sea_orm::sea_query::Value::TimeDateTimeWithTimeZone(Some(Box::new(now))),
            ],
        ))
        .await
        .expect("seed legacy token row");

        Migrator::up(&db, None)
            .await
            .expect("apply the credential-record migration");

        // The row followed the hash: a lookup by the token that was already in
        // production finds its record, with the tagma default set and no
        // invented display tail.
        let row = access_tokens::Entity::find()
            .filter(access_tokens::Column::TokenHash.eq(hash.as_bytes().to_vec()))
            .one(&db)
            .await
            .expect("query access_tokens")
            .expect("the live token carried across");
        assert_eq!(row.owner_kind, "tagma");
        assert_eq!(row.owner_id, "t1");
        assert_eq!(row.name, "enrollment");
        assert_eq!(row.created_by, "u1");
        assert_eq!(row.created_at, now);
        assert_eq!(row.masked, None);
        assert_eq!(row.expires_at, None);
        assert_eq!(row.revoked_at, None);
        assert_eq!(
            row.perms,
            serde_json::json!(kallipai_archeion_common::perm::TAGMA_DEFAULTS)
        );

        // The carried record opens its own history: a `create` row with no
        // actor, holding the set it arrived with.
        let trail = credential_events::Entity::find()
            .filter(credential_events::Column::TokenId.eq(&row.id))
            .all(&db)
            .await
            .expect("query credential_events");
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].kind, crate::credentials::EVENT_CREATE);
        assert_eq!(trail[0].actor_user_id, None);
        assert_eq!(trail[0].at, now);
        assert_eq!(
            trail[0].added,
            serde_json::json!(kallipai_archeion_common::perm::TAGMA_DEFAULTS)
        );
        assert_eq!(trail[0].removed, serde_json::json!([]));

        // The old table is gone, not shadowed.
        let legacy_tables = db
            .query_all(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = 'public' AND table_name = 'tagma_tokens'"
                    .to_owned(),
            ))
            .await
            .expect("query information_schema");
        assert!(legacy_tables.is_empty(), "tagma_tokens must be dropped");

        // The permission columns are jsonb, so a containment query answers the
        // way an equality check does.
        let plain_json_columns = db
            .query_all(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT table_name, column_name FROM information_schema.columns \
                 WHERE table_schema = 'public' AND data_type = 'json' \
                 AND table_name IN ('access_tokens', 'credential_events')"
                    .to_owned(),
            ))
            .await
            .expect("query information_schema");
        assert!(
            plain_json_columns.is_empty(),
            "access_tokens and credential_events store their sets as jsonb"
        );
    }
}
