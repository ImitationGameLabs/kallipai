//! The DB-backed [`ControlPlane`] impl: the single source of truth for
//! credential verification and tagma metadata, consumed by the data-plane relay
//! (`kallip-lesche`) through the `/internal/*` HTTP API (each handler wraps this
//! impl). The lesche never touches these tables directly.

use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control_plane::{
    ControlPlane, ControlPlaneError, TagmaProfile, UserIdentity, VerifiedSession,
};
use kallip_agora_common::ids::{TagmaId, UserId};
use kallip_agora_common::principal::Principal;
use kallip_common::authtoken::TokenHash;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;

use crate::db::Db;
use crate::db::entity::{sessions, tagma_tokens, tagmata, users};

/// The registry, DB-backed. Cheap to construct (a cloned `Db` handle + the admin
/// hash), so the agora control-plane's own `AuthPrincipal` extractor and the
/// `/internal/*` HTTP handlers can each make one.
#[derive(Clone)]
pub struct DbControlPlane {
    db: Db,
    admin_token_hash: TokenHash,
}

impl DbControlPlane {
    pub fn new(db: Db, admin_token_hash: TokenHash) -> Self {
        Self {
            db,
            admin_token_hash,
        }
    }
}

fn map_err(e: sea_orm::DbErr) -> ControlPlaneError {
    ControlPlaneError::Backend(e.to_string())
}

#[async_trait::async_trait]
impl ControlPlane for DbControlPlane {
    async fn verify_session(
        &self,
        cookie_value: &str,
    ) -> Result<Option<VerifiedSession>, ControlPlaneError> {
        let hash = TokenHash::of(cookie_value);
        let row = sessions::Entity::find()
            .filter(sessions::Column::TokenHash.eq(hash.as_bytes().to_vec()))
            .one(&self.db)
            .await
            .map_err(map_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.expires_at <= OffsetDateTime::now_utc() {
            return Ok(None);
        }
        // Owner-disabled re-check: disabling a user takes effect immediately on
        // every authenticated request, not just at next login. The display
        // identity is resolved in the same pass (the row is already loaded) so
        // the relay gets the authoritative handle once per connection-open.
        let user = users::Entity::find_by_id(row.user_id.clone())
            .one(&self.db)
            .await
            .map_err(map_err)?;
        let Some(user) = user else {
            return Ok(None);
        };
        if user.disabled_at.is_some() {
            return Ok(None);
        }
        Ok(Some(VerifiedSession {
            user_id: UserId::from(user.id),
            username: user.username,
            display_name: user.display_name,
        }))
    }

    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError> {
        let hash = TokenHash::of(token);
        if self.admin_token_hash.ct_eq(&hash) {
            return Ok(Some(Principal::Admin));
        }
        let row = tagma_tokens::Entity::find()
            .filter(tagma_tokens::Column::TokenHash.eq(hash.as_bytes().to_vec()))
            .one(&self.db)
            .await
            .map_err(map_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        // A revoked tagma never authenticates (the unified revoke flag cuts the
        // tagma off on its next request).
        let tagma = tagmata::Entity::find_by_id(row.tagma_id.clone())
            .one(&self.db)
            .await
            .map_err(map_err)?;
        let Some(tagma) = tagma else {
            return Ok(None);
        };
        if tagma.revoked_at.is_some() {
            return Ok(None);
        }
        // A tagma owned by a disabled account never authenticates either. A
        // missing owner is unreachable (FK ON DELETE RESTRICT); treat it as
        // disabled so this path fails closed, matching `tagma_profiles`'s owner
        // arm.
        let owner_disabled = match users::Entity::find_by_id(tagma.owner_user_id.clone())
            .one(&self.db)
            .await
            .map_err(map_err)?
        {
            Some(owner) => owner.disabled_at.is_some(),
            None => true,
        };
        if owner_disabled {
            return Ok(None);
        }
        Ok(Some(Principal::Tagma(TagmaId::from(row.tagma_id))))
    }

    async fn tagma_profiles(
        &self,
        tagma_ids: &[TagmaId],
    ) -> Result<Vec<TagmaProfile>, ControlPlaneError> {
        if tagma_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = tagma_ids.iter().map(|t| t.to_string()).collect();
        // UNFILTERED: return every existing input row with its raw usability
        // state. The relay -- not the registry -- combines these facts into an
        // authorization decision. Unknown input ids are absent from the rows
        // (IS IN), so they are omitted by construction.
        let rows = tagmata::Entity::find()
            .filter(tagmata::Column::Id.is_in(ids))
            .all(&self.db)
            .await
            .map_err(map_err)?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        // One batched read of the owner rows (FK ON DELETE RESTRICT guarantees
        // each exists). Map owner_user_id -> (username, display_name, disabled).
        let owner_ids: Vec<String> = rows.iter().map(|r| r.owner_user_id.clone()).collect();
        let owners = users::Entity::find()
            .filter(users::Column::Id.is_in(owner_ids))
            .all(&self.db)
            .await
            .map_err(map_err)?;
        let owner_facts: std::collections::HashMap<String, (String, Option<String>, bool)> = owners
            .into_iter()
            .map(|u| (u.id, (u.username, u.display_name, u.disabled_at.is_some())))
            .collect();
        let mut out = Vec::with_capacity(rows.len());
        for t in rows {
            // FK ON DELETE RESTRICT guarantees the owner row exists; the `None`
            // arm is unreachable defense-in-depth. Treat a missing row as
            // disabled (deny) so such a tagma can never join a room or open a
            // chat -- never silently permissive.
            let (owner_username, owner_display_name, owner_disabled) =
                match owner_facts.get(&t.owner_user_id) {
                    Some((u, d, dis)) => (u.clone(), d.clone(), *dis),
                    None => (String::new(), None, true),
                };
            out.push(TagmaProfile {
                tagma_id: TagmaId::from(t.id),
                pinned_public_key: t.pinned_public_key.map(Ed25519PublicKey),
                owner_user_id: UserId::from(t.owner_user_id),
                label: t.label,
                owner_username,
                owner_display_name,
                enrolled: t.enrolled_at.is_some(),
                revoked: t.revoked_at.is_some(),
                owner_disabled,
            });
        }
        Ok(out)
    }

    async fn user_identities(
        &self,
        user_ids: &[UserId],
    ) -> Result<Vec<UserIdentity>, ControlPlaneError> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = user_ids.iter().map(|u| u.to_string()).collect();
        // UNFILTERED: return every existing input user with its raw `disabled`
        // state; the relay derives the invite gate (`!disabled`) locally.
        let rows = users::Entity::find()
            .filter(users::Column::Id.is_in(ids))
            .all(&self.db)
            .await
            .map_err(map_err)?;
        Ok(rows
            .into_iter()
            .map(|u| UserIdentity {
                user_id: UserId::from(u.id),
                username: u.username,
                display_name: u.display_name,
                disabled: u.disabled_at.is_some(),
            })
            .collect())
    }

    async fn user_identity_by_username(
        &self,
        username: &str,
    ) -> Result<Option<UserIdentity>, ControlPlaneError> {
        // Single source of truth for handle shape: the same normalizer signup
        // uses. A malformed / wrong-shape handle fails normalization and
        // collapses to `None` -- the same outcome as an unknown handle, so the
        // invite gate renders one fixed 404 with no shape leak.
        let Ok(normalized) = crate::username::normalize(username) else {
            return Ok(None);
        };
        let row = users::Entity::find()
            .filter(users::Column::Username.eq(normalized))
            .one(&self.db)
            .await
            .map_err(map_err)?;
        Ok(row.map(|u| UserIdentity {
            user_id: UserId::from(u.id),
            username: u.username,
            display_name: u.display_name,
            disabled: u.disabled_at.is_some(),
        }))
    }

    async fn bump_tunnel_proof_ts(
        &self,
        tagma_id: &TagmaId,
        ts: i64,
    ) -> Result<bool, ControlPlaneError> {
        // Atomic conditional UPDATE: advances the high-water-mark iff it is NULL
        // or strictly less than `ts`. Cross-restart replay guard.
        let updated = tagmata::Entity::update_many()
            .filter(tagmata::Column::Id.eq(tagma_id.to_string()))
            .filter(
                sea_orm::Condition::any()
                    .add(tagmata::Column::LastTunnelProofTs.is_null())
                    .add(tagmata::Column::LastTunnelProofTs.lt(ts)),
            )
            .col_expr(
                tagmata::Column::LastTunnelProofTs,
                sea_orm::sea_query::Expr::value(ts),
            )
            .exec(&self.db)
            .await
            .map_err(map_err)?;
        Ok(updated.rows_affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::{sessions, tagmata};
    use crate::test_helpers::{make_state, seed_tagma, seed_user};
    use crate::token::SESSION;
    use kallip_agora_common::control_plane::ControlPlane;
    use kallip_agora_common::principal::Principal;
    use kallip_common::authtoken::MintedToken;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    use time::{Duration, OffsetDateTime};

    fn cp(state: &crate::state::SharedState) -> DbControlPlane {
        DbControlPlane::new(state.db.clone(), state.admin_token_hash.clone())
    }

    /// A disabled user's already-issued session is rejected on the very next
    /// resolve: the hot-path disabled check is what makes "disable" take effect
    /// immediately, not just at the next login.
    #[tokio::test]
    async fn verify_session_rejects_disabled_user() {
        let state = make_state().await;
        let user_id = seed_user(&state, "frozen", "frozen@example.test").await;
        let session = MintedToken::generate(SESSION);
        let now = OffsetDateTime::now_utc();
        sessions::ActiveModel {
            token_hash: Set(session.hash().as_bytes().to_vec()),
            user_id: Set(user_id.to_string()),
            created_at: Set(now),
            expires_at: Set(now + Duration::hours(1)),
            authed_at: Set(None),
        }
        .insert(&state.db)
        .await
        .expect("insert session");

        let control = cp(&state);
        assert!(
            control
                .verify_session(session.secret())
                .await
                .unwrap()
                .is_some()
        );

        let row = users::Entity::find_by_id(user_id.to_string())
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut am: users::ActiveModel = row.into();
        am.disabled_at = Set(Some(now));
        am.update(&state.db).await.unwrap();
        assert!(
            control
                .verify_session(session.secret())
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Resolving a user by handle returns the canonical identity (with the
    /// `user_id` the invite gate reads back out), and folds case via the shared
    /// normalizer.
    #[tokio::test]
    async fn user_identity_by_username_resolves() {
        let state = make_state().await;
        let user_id = seed_user(&state, "alice", "alice@example.test").await;

        let control = cp(&state);
        let resolved = control.user_identity_by_username("ALICE").await.unwrap();
        let resolved = resolved.expect("seeded user resolves by handle");
        assert_eq!(resolved.user_id, user_id);
        assert_eq!(resolved.username, "alice");
        assert!(!resolved.disabled);
    }

    /// An unknown handle and a malformed handle both collapse to `None` -- the
    /// same outcome, so the invite gate renders one fixed 404 with no shape
    /// leak (the existence-oracle invariant).
    #[tokio::test]
    async fn user_identity_by_username_unknown_and_malformed_are_none() {
        let state = make_state().await;
        let control = cp(&state);

        // Unknown but well-formed handle.
        assert!(
            control
                .user_identity_by_username("nobody")
                .await
                .unwrap()
                .is_none()
        );
        // Malformed: a bare `@` (the caller strips the sigil; if one slips
        // through, normalize rejects it and it still collapses to None).
        assert!(
            control
                .user_identity_by_username("@alice")
                .await
                .unwrap()
                .is_none()
        );
        // Malformed: invalid char (underscore) and interior space.
        assert!(
            control
                .user_identity_by_username("no_good")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            control
                .user_identity_by_username("not valid")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A revoked tagma's bearer never authenticates.
    #[tokio::test]
    async fn verify_bearer_rejects_revoked_tagma() {
        let state = make_state().await;
        let user_id = seed_user(&state, "owner", "owner@example.test").await;
        let (tagma_id, token) = seed_tagma(&state, &user_id, Ed25519PublicKey(vec![0u8; 32])).await;

        let control = cp(&state);
        assert!(matches!(
            control.verify_bearer(&token).await.unwrap(),
            Some(Principal::Tagma(id)) if id == tagma_id
        ));

        let row = tagmata::Entity::find_by_id(tagma_id.to_string())
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut am: tagmata::ActiveModel = row.into();
        am.revoked_at = Set(Some(OffsetDateTime::now_utc()));
        am.update(&state.db).await.unwrap();
        assert!(control.verify_bearer(&token).await.unwrap().is_none());
    }
}
