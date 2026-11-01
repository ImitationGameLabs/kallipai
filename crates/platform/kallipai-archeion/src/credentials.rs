//! Credential records: one `access_tokens` row per credential, one
//! `credential_events` row per lifecycle action.
//!
//! A credential is a row, not a bare secret. The row carries the owner, the
//! name, the stored permission set, the expiry and the audit trail; a reset
//! swaps only the hash, so the id, the name and the history survive it. The
//! plaintext is minted here, handed back once to the caller, and never stored.
//!
//! Every function takes a [`ConnectionTrait`], so a handler composes several in
//! one transaction: a reset that wrote the new hash without its audit row is
//! not a state this module can produce.
//!
//! Three actions are events: `create`, `reset`, `perms`. Revocation is not one
//! of them -- it is a column (`revoked_at`), the record's terminal state, not an
//! edit to what the credential may do. Renaming writes no event for the same
//! reason: a name is a label the owner picks, not a capability.

use kallipai_archeion_common::perm::PermSet;
use kallipai_common::authtoken::{MintedToken, TokenKind, mask_token};
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::entity::{access_tokens, credential_events};
use crate::db::map_db_err;

/// Event kind: the record came into existence.
pub const EVENT_CREATE: &str = "create";

/// Event kind: the secret was replaced, the record kept.
pub const EVENT_RESET: &str = "reset";

/// Event kind: the permission set changed.
pub const EVENT_PERMS: &str = "perms";

/// `access_tokens.owner_kind` values.
const OWNER_TAGMA: &str = "tagma";
const OWNER_USER: &str = "user";

/// Whose credential a record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A tagma's single credential; one live record per tagma, enforced by a
    /// partial unique index.
    Tagma,
    /// One of a user's several personal access tokens.
    User,
}

impl Owner {
    fn as_str(self) -> &'static str {
        match self {
            Self::Tagma => OWNER_TAGMA,
            Self::User => OWNER_USER,
        }
    }
}

/// A credential about to be minted.
pub struct NewCredential<'a> {
    pub owner: Owner,
    /// The owning `tagmata.id` or `users.id`, per `owner`.
    pub owner_id: &'a str,
    pub name: &'a str,
    pub perms: &'a PermSet,
    /// The user the record belongs to on paper (the tagma's owner for an
    /// enrollment).
    pub created_by: &'a str,
    /// Who acted, for the audit trail; `None` when the tagma enrolled itself.
    pub actor: Option<&'a str>,
    /// `None` = never expires (a tagma credential).
    pub expires_at: Option<OffsetDateTime>,
}

/// A minted credential: the stored record plus its one-time plaintext.
pub struct Minted {
    pub record: access_tokens::Model,
    pub plaintext: String,
}

impl std::fmt::Debug for Minted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the plaintext: a `Debug` rendering ends up in logs and test
        // failure messages.
        f.debug_struct("Minted")
            .field("record", &self.record)
            .field("plaintext", &"<redacted>")
            .finish()
    }
}

/// The permission set stored on `row`, or `None` when the row carries a string
/// outside the vocabulary. That is a write-edge bug, so verification treats the
/// credential as unusable rather than reading it as narrower or wider than the
/// owner set it to.
pub fn perms_of(row: &access_tokens::Model) -> Option<PermSet> {
    let parsed: PermSet = serde_json::from_value(row.perms.clone()).ok()?;
    PermSet::from_known(parsed.iter().map(str::to_owned)).ok()
}

/// Mint a credential and write its record plus the `create` event.
///
/// For a tagma this is what enrollment ends with; a tagma that already holds a
/// live record is a conflict. For a user it is one more personal token.
pub async fn create<C: ConnectionTrait>(
    conn: &C,
    kind: TokenKind,
    new: NewCredential<'_>,
) -> Result<Minted, ApiError> {
    if new.owner == Owner::Tagma
        && let Some(live) = live_tagma_record(conn, new.owner_id).await?
    {
        return Err(ApiError::conflict(format!(
            "this tagma already holds a live credential record ({})",
            live.id
        )));
    }
    let minted = MintedToken::generate(kind)
        .map_err(|e| ApiError::internal(format_args!("minting a credential: {e}")))?;
    let token_id = Uuid::new_v4().to_string();
    let now = OffsetDateTime::now_utc();
    let record = access_tokens::ActiveModel {
        id: Set(token_id.clone()),
        owner_kind: Set(new.owner.as_str().to_string()),
        owner_id: Set(new.owner_id.to_string()),
        name: Set(new.name.to_string()),
        token_hash: Set(minted.hash().as_bytes().to_vec()),
        masked: Set(Some(mask_token(minted.secret(), kind))),
        perms: Set(json_perms(new.perms)?),
        created_at: Set(now),
        created_by: Set(new.created_by.to_string()),
        expires_at: Set(new.expires_at),
        last_used_at: Set(None),
        revoked_at: Set(None),
    }
    .insert(conn)
    .await
    .map_err(map_db_err)?;
    record_event(
        conn,
        &token_id,
        now,
        new.actor,
        EVENT_CREATE,
        &new.perms.to_vec(),
        &[],
    )
    .await?;
    Ok(Minted {
        record,
        plaintext: minted.secret().to_string(),
    })
}

/// The owner's records, oldest first.
pub async fn list<C: ConnectionTrait>(
    conn: &C,
    owner: Owner,
    owner_id: &str,
) -> Result<Vec<access_tokens::Model>, ApiError> {
    access_tokens::Entity::find()
        .filter(access_tokens::Column::OwnerKind.eq(owner.as_str()))
        .filter(access_tokens::Column::OwnerId.eq(owner_id))
        .order_by_asc(access_tokens::Column::CreatedAt)
        .all(conn)
        .await
        .map_err(map_db_err)
}

/// The tagma's live record: what a reset or a permission edit acts on.
pub async fn live_tagma_record<C: ConnectionTrait>(
    conn: &C,
    tagma_id: &str,
) -> Result<Option<access_tokens::Model>, ApiError> {
    access_tokens::Entity::find()
        .filter(access_tokens::Column::OwnerKind.eq(OWNER_TAGMA))
        .filter(access_tokens::Column::OwnerId.eq(tagma_id))
        .filter(access_tokens::Column::RevokedAt.is_null())
        .one(conn)
        .await
        .map_err(map_db_err)
}

/// Change the label. No event: a name grants nothing.
pub async fn rename<C: ConnectionTrait>(
    conn: &C,
    record: access_tokens::Model,
    name: &str,
) -> Result<access_tokens::Model, ApiError> {
    let mut am: access_tokens::ActiveModel = record.into();
    am.name = Set(name.to_string());
    am.update(conn).await.map_err(map_db_err)
}

/// Replace the permission set and record both sides of the edit.
///
/// An edit that changes nothing writes no event: the trail is of permissions
/// actually granted or withdrawn. An empty set is accepted and is a real state:
/// the credential authenticates and passes no permission check.
pub async fn set_perms<C: ConnectionTrait>(
    conn: &C,
    record: access_tokens::Model,
    perms: &PermSet,
    actor: &str,
) -> Result<access_tokens::Model, ApiError> {
    let stored = perms_of(&record).ok_or_else(|| {
        ApiError::internal(format!(
            "credential {} carries a permission string outside the vocabulary",
            record.id
        ))
    })?;
    let added = perms.difference(&stored);
    let removed = stored.difference(perms);
    let token_id = record.id.clone();
    let mut am: access_tokens::ActiveModel = record.into();
    am.perms = Set(json_perms(perms)?);
    let record = am.update(conn).await.map_err(map_db_err)?;
    if !added.is_empty() || !removed.is_empty() {
        record_event(
            conn,
            &token_id,
            OffsetDateTime::now_utc(),
            Some(actor),
            EVENT_PERMS,
            &added.to_vec(),
            &removed.to_vec(),
        )
        .await?;
    }
    Ok(record)
}

/// Mint a fresh secret for an existing record.
///
/// The hash and the mask swap; the id, the name, the permission set and the
/// history stay. The replaced plaintext stops resolving at its next
/// verification. A running tagma holds its secret as a startup snapshot, so a
/// reset reaches it only once its credential file carries the new value and it
/// re-reads that file -- never by the registry pushing anything.
pub async fn reset<C: ConnectionTrait>(
    conn: &C,
    kind: TokenKind,
    record: access_tokens::Model,
    actor: &str,
) -> Result<Minted, ApiError> {
    let minted = MintedToken::generate(kind)
        .map_err(|e| ApiError::internal(format_args!("minting a credential: {e}")))?;
    let token_id = record.id.clone();
    let mut am: access_tokens::ActiveModel = record.into();
    am.token_hash = Set(minted.hash().as_bytes().to_vec());
    am.masked = Set(Some(mask_token(minted.secret(), kind)));
    let record = am.update(conn).await.map_err(map_db_err)?;
    record_event(
        conn,
        &token_id,
        OffsetDateTime::now_utc(),
        Some(actor),
        EVENT_RESET,
        &[],
        &[],
    )
    .await?;
    Ok(Minted {
        record,
        plaintext: minted.secret().to_string(),
    })
}

/// Revoke one record. Its history stays readable; the credential stops
/// authenticating at its next verification.
pub async fn revoke<C: ConnectionTrait>(
    conn: &C,
    record: access_tokens::Model,
) -> Result<access_tokens::Model, ApiError> {
    let mut am: access_tokens::ActiveModel = record.into();
    am.revoked_at = Set(Some(OffsetDateTime::now_utc()));
    am.update(conn).await.map_err(map_db_err)
}

/// One record's events, oldest first.
pub async fn events<C: ConnectionTrait>(
    conn: &C,
    token_id: &str,
) -> Result<Vec<credential_events::Model>, ApiError> {
    credential_events::Entity::find()
        .filter(credential_events::Column::TokenId.eq(token_id))
        .order_by_asc(credential_events::Column::At)
        .all(conn)
        .await
        .map_err(map_db_err)
}

fn json_perms(perms: &PermSet) -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(perms)
        .map_err(|e| ApiError::internal(format_args!("serializing permissions: {e}")))
}

async fn record_event<C: ConnectionTrait>(
    conn: &C,
    token_id: &str,
    at: OffsetDateTime,
    actor: Option<&str>,
    kind: &str,
    added: &[String],
    removed: &[String],
) -> Result<(), ApiError> {
    credential_events::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        token_id: Set(token_id.to_string()),
        at: Set(at),
        actor_user_id: Set(actor.map(str::to_owned)),
        kind: Set(kind.to_string()),
        added: Set(serde_json::json!(added)),
        removed: Set(serde_json::json!(removed)),
    }
    .insert(conn)
    .await
    .map(|_| ())
    .map_err(map_db_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_archeion_common::perm::{self, ARCHEION_ACCOUNT_READ, ARCHEION_TAGMA_READ};

    use crate::test_helpers::setup_test_db;
    use crate::token::{PAT, TAGMA};

    fn personal<'a>(owner_id: &'a str, name: &'a str, perms: &'a PermSet) -> NewCredential<'a> {
        NewCredential {
            owner: Owner::User,
            owner_id,
            name,
            perms,
            created_by: owner_id,
            actor: Some(owner_id),
            expires_at: None,
        }
    }

    fn tagma<'a>(owner_id: &'a str, perms: &'a PermSet) -> NewCredential<'a> {
        NewCredential {
            owner: Owner::Tagma,
            owner_id,
            name: "enrollment",
            perms,
            created_by: "owner-1",
            actor: None,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn create_stores_the_record_and_its_create_event() {
        let db = setup_test_db().await;
        let perms = perm::pat_defaults();
        let minted = create(&db, PAT, personal("u1", "cli-laptop", &perms))
            .await
            .expect("create");

        assert_eq!(minted.record.owner_kind, "user");
        assert_eq!(minted.record.owner_id, "u1");
        assert_eq!(minted.record.name, "cli-laptop");
        assert_eq!(minted.record.created_by, "u1");
        assert!(
            minted
                .record
                .masked
                .as_deref()
                .is_some_and(|masked| masked.starts_with("sk-pat-")),
            "the mask carries the token prefix: {:?}",
            minted.record.masked
        );
        assert!(minted.plaintext.starts_with("sk-pat-"));
        assert_eq!(
            minted.record.token_hash,
            kallipai_common::authtoken::TokenHash::of(&minted.plaintext)
                .as_bytes()
                .to_vec()
        );
        assert!(minted.record.expires_at.is_none());
        assert!(minted.record.revoked_at.is_none());
        assert_eq!(perms_of(&minted.record), Some(perms.clone()));

        let trail = events(&db, &minted.record.id).await.expect("events");
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].kind, EVENT_CREATE);
        assert_eq!(trail[0].actor_user_id.as_deref(), Some("u1"));
        assert_eq!(trail[0].added, serde_json::json!(perms.to_vec()));
        assert_eq!(trail[0].removed, serde_json::json!([]));
    }

    #[tokio::test]
    async fn create_refuses_a_second_live_tagma_credential() {
        let db = setup_test_db().await;
        let perms = perm::tagma_defaults();
        let first = create(&db, TAGMA, tagma("t1", &perms))
            .await
            .expect("create");

        let err = create(&db, TAGMA, tagma("t1", &perms))
            .await
            .expect_err("a tagma holds one live credential");
        assert_eq!(err.status, 409);

        revoke(&db, first.record).await.expect("revoke");
        create(&db, TAGMA, tagma("t1", &perms))
            .await
            .expect("a revoked record leaves the slot free");
    }

    /// The one-live-record rule lives in the partial unique index, not in
    /// `create`'s read-then-insert: this writes a second live row straight
    /// through the entity, so removing the index DDL turns this test red while
    /// the 409 path stays green.
    #[tokio::test]
    async fn the_partial_unique_index_refuses_a_second_live_tagma_row() {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};

        let db = setup_test_db().await;
        let perms = perm::tagma_defaults();
        let first = create(&db, TAGMA, tagma("t1", &perms))
            .await
            .expect("create");

        let err = raw_tagma_row("t1", &perms)
            .insert(&db)
            .await
            .expect_err("a tagma holds one live row");
        assert!(
            matches!(
                err.sql_err(),
                Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
            ),
            "the index, not the pre-check, must refuse it: {err:?}"
        );

        // A revoked row does not occupy the slot.
        let mut am: access_tokens::ActiveModel = first.record.into();
        am.revoked_at = Set(Some(OffsetDateTime::now_utc()));
        am.update(&db).await.expect("revoke");
        raw_tagma_row("t1", &perms)
            .insert(&db)
            .await
            .expect("a revoked row leaves the slot free");
    }

    /// A second live tagma row written the way a racing writer would: no
    /// pre-check, just the insert.
    fn raw_tagma_row(tagma_id: &str, perms: &PermSet) -> access_tokens::ActiveModel {
        use sea_orm::ActiveValue::Set;

        access_tokens::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            owner_kind: Set("tagma".to_string()),
            owner_id: Set(tagma_id.to_string()),
            name: Set("enrollment".to_string()),
            token_hash: Set(Uuid::new_v4().as_bytes().to_vec()),
            masked: Set(None),
            perms: Set(serde_json::json!(perms.to_vec())),
            created_at: Set(OffsetDateTime::now_utc()),
            created_by: Set("owner-1".to_string()),
            expires_at: Set(None),
            last_used_at: Set(None),
            revoked_at: Set(None),
        }
    }

    #[tokio::test]
    async fn reset_swaps_the_secret_and_keeps_the_record() {
        let db = setup_test_db().await;
        let perms = perm::tagma_defaults();
        let first = create(&db, TAGMA, tagma("t1", &perms))
            .await
            .expect("create");

        let record_id = first.record.id.clone();
        let reset_record = reset(&db, TAGMA, first.record, "owner-1")
            .await
            .expect("reset");

        assert_eq!(reset_record.record.id, record_id);
        assert_eq!(reset_record.record.name, "enrollment");
        assert_ne!(reset_record.plaintext, first.plaintext);
        assert_eq!(
            reset_record.record.token_hash,
            kallipai_common::authtoken::TokenHash::of(&reset_record.plaintext)
                .as_bytes()
                .to_vec()
        );
        assert_eq!(
            reset_record.record.masked.as_deref(),
            Some(mask_token(&reset_record.plaintext, TAGMA).as_str())
        );
        assert_eq!(perms_of(&reset_record.record), Some(perms));

        let trail = events(&db, &reset_record.record.id).await.expect("events");
        assert_eq!(
            trail.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            vec![EVENT_CREATE, EVENT_RESET]
        );
        assert_eq!(trail[1].actor_user_id.as_deref(), Some("owner-1"));
    }

    #[tokio::test]
    async fn set_perms_records_both_sides_and_accepts_an_empty_set() {
        let db = setup_test_db().await;
        let start = perm::pat_defaults();
        let minted = create(&db, PAT, personal("u1", "ci", &start))
            .await
            .expect("create");

        let only_files = PermSet::from_known([kallipai_archeion_common::perm::FILES_RECORD_READ])
            .expect("known");
        let record = set_perms(&db, minted.record, &only_files, "admin-1")
            .await
            .expect("set");
        assert_eq!(perms_of(&record), Some(only_files.clone()));

        let trail = events(&db, &record.id).await.expect("events");
        assert_eq!(trail.len(), 2);
        assert_eq!(trail[1].kind, EVENT_PERMS);
        assert_eq!(trail[1].actor_user_id.as_deref(), Some("admin-1"));
        assert_eq!(trail[1].added, serde_json::json!([]));
        assert_eq!(
            trail[1].removed,
            serde_json::json!([ARCHEION_ACCOUNT_READ, ARCHEION_TAGMA_READ])
        );

        let record = set_perms(&db, record, &PermSet::default(), "admin-1")
            .await
            .expect("empty the set");
        assert!(perms_of(&record).expect("stored").is_empty());
        assert!(record.revoked_at.is_none(), "an empty set is not a revoke");

        let trail = events(&db, &record.id).await.expect("events");
        assert_eq!(trail.len(), 3);
        assert_eq!(
            trail[2].removed,
            serde_json::json!([kallipai_archeion_common::perm::FILES_RECORD_READ])
        );
    }

    #[tokio::test]
    async fn unchanged_perms_write_no_event() {
        let db = setup_test_db().await;
        let perms = perm::pat_defaults();
        let minted = create(&db, PAT, personal("u1", "ci", &perms))
            .await
            .expect("create");

        let record = set_perms(&db, minted.record, &perms, "admin-1")
            .await
            .expect("set");

        let trail = events(&db, &record.id).await.expect("events");
        assert_eq!(
            trail.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            vec![EVENT_CREATE]
        );
    }

    #[tokio::test]
    async fn rename_changes_the_label_and_writes_no_event() {
        let db = setup_test_db().await;
        let perms = perm::pat_defaults();
        let minted = create(&db, PAT, personal("u1", "ci", &perms))
            .await
            .expect("create");

        let record = rename(&db, minted.record, "ci-runner")
            .await
            .expect("rename");

        assert_eq!(record.name, "ci-runner");
        let trail = events(&db, &record.id).await.expect("events");
        assert_eq!(trail.len(), 1);
    }

    #[tokio::test]
    async fn list_is_owner_scoped_and_keeps_revoked_records() {
        let db = setup_test_db().await;
        let perms = perm::pat_defaults();
        let mine = create(&db, PAT, personal("u1", "laptop", &perms))
            .await
            .expect("create");
        create(&db, PAT, personal("u2", "laptop", &perms))
            .await
            .expect("create");

        let listed = list(&db, Owner::User, "u1").await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, mine.record.id);

        revoke(&db, mine.record).await.expect("revoke");
        let listed = list(&db, Owner::User, "u1").await.expect("list");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].revoked_at.is_some());
        assert!(
            live_tagma_record(&db, "u1")
                .await
                .expect("lookup")
                .is_none(),
            "a user record never answers a tagma lookup"
        );
    }
}
