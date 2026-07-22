//! The DB-backed [`ControlPlane`] impl: the single source of truth for
//! credential verification and tagma metadata, consumed by the data-plane relay
//! (`kallip-lesche`) through the `/internal/*` HTTP API (each handler wraps this
//! impl). The lesche never touches these tables directly.

use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control_plane::{ControlPlane, ControlPlaneError, TagmaIdentity};
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
    ) -> Result<Option<UserId>, ControlPlaneError> {
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
        // every authenticated request, not just at next login.
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
        Ok(Some(UserId::from(user.id)))
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
        // herald off on its next request).
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
        // A tagma owned by a disabled account never authenticates either.
        let owner_disabled = match users::Entity::find_by_id(tagma.owner_user_id.clone())
            .one(&self.db)
            .await
            .map_err(map_err)?
        {
            Some(owner) => owner.disabled_at.is_some(),
            None => false,
        };
        if owner_disabled {
            return Ok(None);
        }
        Ok(Some(Principal::Tagma(TagmaId::from(row.tagma_id))))
    }

    async fn tagma_resolvable_by(
        &self,
        tagma_id: &TagmaId,
        user: &UserId,
    ) -> Result<bool, ControlPlaneError> {
        let tagma = tagmata::Entity::find_by_id(tagma_id.to_string())
            .one(&self.db)
            .await
            .map_err(map_err)?;
        Ok(matches!(
            tagma,
            Some(t) if t.owner_user_id.as_str() == user.as_ref() && t.enrolled_at.is_some()
        ))
    }

    async fn tagma_identity(
        &self,
        tagma_id: &TagmaId,
    ) -> Result<Option<TagmaIdentity>, ControlPlaneError> {
        let tagma = tagmata::Entity::find_by_id(tagma_id.to_string())
            .one(&self.db)
            .await
            .map_err(map_err)?;
        let Some(tagma) = tagma else {
            return Ok(None);
        };
        let Some(pinned) = tagma.pinned_public_key else {
            return Ok(None);
        };
        Ok(Some(TagmaIdentity {
            pinned_public_key: Ed25519PublicKey(pinned),
            owner_user_id: UserId::from(tagma.owner_user_id),
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
