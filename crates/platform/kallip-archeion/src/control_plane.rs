//! The DB-backed [`ControlPlane`] impl: the single source of truth for
//! credential verification and tagma metadata, consumed by the data-plane relay
//! (`kallip-lesche`) through the `/internal/*` HTTP API (each handler wraps this
//! impl). The lesche never touches these tables directly.

use kallip_archeion_common::bytes::Ed25519PublicKey;
use kallip_archeion_common::control_plane::{
    ControlPlane, ControlPlaneError, EnrollmentLookup, LOCAL_ADMIN_PROVIDER, LOCAL_ADMIN_SUBJECT,
    TagmaProfile, UserIdentity, VerifiedSession,
};
use kallip_archeion_common::ids::{TagmaId, UserId};
use kallip_archeion_common::principal::Principal;
use kallip_common::authtoken::TokenHash;
use sea_orm::sea_query::{Expr, Query};
use sea_orm::{ColumnTrait, EntityTrait, FromQueryResult, QueryFilter, QuerySelect};
use time::OffsetDateTime;

use crate::db::Db;
use crate::db::entity::{external_identities, sessions, tagma_tokens, tagmata, users};

/// The registry, DB-backed. Cheap to construct (a cloned `Db` handle + the admin
/// hash), so the archeion control-plane's own `AuthPrincipal` extractor and the
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

/// The `verify_session` user row: the users columns plus the local-admin
/// marker flag projected in the same single query (an EXISTS subselect).
#[derive(FromQueryResult)]
struct SessionUserRow {
    id: String,
    username: String,
    display_name: Option<String>,
    disabled_at: Option<OffsetDateTime>,
    local_admin: bool,
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
        // identity AND the local-admin marker flag resolve in the same single
        // query (an EXISTS projection onto the user row), so a session check
        // never costs a second marker lookup.
        let user = users::Entity::find_by_id(row.user_id.clone())
            .expr_as(
                Expr::exists(
                    Query::select()
                        .column(external_identities::Column::Id)
                        .from(external_identities::Entity)
                        .cond_where(
                            Expr::col((
                                external_identities::Entity,
                                external_identities::Column::UserId,
                            ))
                            .eq(Expr::col((users::Entity, users::Column::Id))),
                        )
                        .cond_where(external_identities::Column::Provider.eq(LOCAL_ADMIN_PROVIDER))
                        .cond_where(external_identities::Column::Subject.eq(LOCAL_ADMIN_SUBJECT))
                        .take(),
                ),
                "local_admin",
            )
            .into_model::<SessionUserRow>()
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
            local_admin: user.local_admin,
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

    async fn enrollment_lookup(
        &self,
        tagma_id: &TagmaId,
    ) -> Result<Option<EnrollmentLookup>, ControlPlaneError> {
        let Some(tagma) = tagmata::Entity::find_by_id(tagma_id.to_string())
            .one(&self.db)
            .await
            .map_err(map_err)?
        else {
            return Ok(None);
        };
        // Fail-closed, mirroring `verify_bearer`'s gates exactly: pending and
        // revoked tagmas resolve to `None` here, and so does a tagma whose
        // owner is disabled (or missing -- unreachable via FK RESTRICT,
        // treated as disabled). A tagma that could not authenticate is also
        // not addressable as a delivery target.
        // The reads below are independent statements, not one transaction: the
        // answer is a point-in-time snapshot. Per-request with no cache, a
        // revoke landing mid-call affects at most the single in-flight
        // request; there is no cached state to retro-correct.
        if tagma.enrolled_at.is_none() || tagma.revoked_at.is_some() {
            return Ok(None);
        }
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
        let rows = tagmata::Entity::find()
            .filter(tagmata::Column::OwnerUserId.eq(tagma.owner_user_id.clone()))
            .filter(tagmata::Column::EnrolledAt.is_not_null())
            .filter(tagmata::Column::RevokedAt.is_null())
            .all(&self.db)
            .await
            .map_err(map_err)?;
        let mut enrolled_tagmas: Vec<TagmaId> =
            rows.into_iter().map(|t| TagmaId::from(t.id)).collect();
        enrolled_tagmas.sort();
        Ok(Some(EnrollmentLookup {
            user_id: UserId::from(tagma.owner_user_id),
            enrolled_tagmas,
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
    use crate::db::entity::{external_identities, sessions, tagmata};
    use crate::test_helpers::{make_state, seed_tagma, seed_user};
    use crate::token::SESSION;
    use kallip_archeion_common::control_plane::ControlPlane;
    use kallip_archeion_common::principal::Principal;
    use kallip_common::authtoken::MintedToken;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    use time::{Duration, OffsetDateTime};

    fn cp(state: &crate::state::SharedState) -> DbControlPlane {
        DbControlPlane::new(
            state.db.clone(),
            state
                .admin_token_hash
                .read()
                .expect("admin token lock")
                .clone(),
        )
    }

    /// A disabled user's already-issued session is rejected on the very next
    /// resolve: the hot-path disabled check is what makes "disable" take effect
    /// immediately, not just at the next login.
    #[tokio::test]
    async fn verify_session_rejects_disabled_user() {
        let state = make_state().await;
        let user_id = seed_user(&state, "frozen").await;
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

    /// The `local_admin` flag follows the external_identities marker row:
    /// the fixed admin-login account's session carries it, a plain user's
    /// does not — the single query resolves both without a second lookup.
    #[tokio::test]
    async fn verify_session_flags_the_local_admin_marker_row() {
        let state = make_state().await;
        let plain_id = seed_user(&state, "plain").await;
        let admin_id = seed_user(&state, "op").await;
        let now = OffsetDateTime::now_utc();
        external_identities::ActiveModel {
            id: Set(uuid::Uuid::new_v4()),
            user_id: Set(admin_id.to_string()),
            provider: Set(LOCAL_ADMIN_PROVIDER.to_string()),
            subject: Set(LOCAL_ADMIN_SUBJECT.to_string()),
            display_name: Set(None),
            created_at: Set(now),
            last_used_at: Set(None),
        }
        .insert(&state.db)
        .await
        .expect("insert marker row");
        let plain = mint_session(&state, plain_id.to_string(), now).await;
        let admin = mint_session(&state, admin_id.to_string(), now).await;
        let control = cp(&state);
        let plain = control.verify_session(plain.secret()).await.unwrap();
        assert!(!plain.as_ref().unwrap().local_admin);
        let admin = control.verify_session(admin.secret()).await.unwrap();
        assert!(admin.unwrap().local_admin);
    }

    /// Seed one live session row for `user_id` and return its plaintext
    /// cookie value (used by the marker-row test above).
    async fn mint_session(
        state: &crate::state::SharedState,
        user_id: String,
        now: OffsetDateTime,
    ) -> MintedToken {
        let session = MintedToken::generate(SESSION);
        sessions::ActiveModel {
            token_hash: Set(session.hash().as_bytes().to_vec()),
            user_id: Set(user_id),
            created_at: Set(now),
            expires_at: Set(now + Duration::hours(1)),
            authed_at: Set(None),
        }
        .insert(&state.db)
        .await
        .expect("insert session");
        session
    }

    /// Resolving a user by handle returns the canonical identity (with the
    /// `user_id` the invite gate reads back out), and folds case via the shared
    /// normalizer.
    #[tokio::test]
    async fn user_identity_by_username_resolves() {
        let state = make_state().await;
        let user_id = seed_user(&state, "alice").await;

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
        let user_id = seed_user(&state, "owner").await;
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

    /// The lookup resolves the owning user plus the FULL enrolled set of that
    /// space (sorted), and never crosses into another user's tagmas.
    #[tokio::test]
    async fn enrollment_lookup_resolves_space_and_full_set() {
        let state = make_state().await;
        let user_id = seed_user(&state, "owner").await;
        let (t1, _) = seed_tagma(&state, &user_id, Ed25519PublicKey(vec![0u8; 32])).await;
        let (t2, _) = seed_tagma(&state, &user_id, Ed25519PublicKey(vec![1u8; 32])).await;
        let other = seed_user(&state, "other").await;
        let (foreign, _) = seed_tagma(&state, &other, Ed25519PublicKey(vec![2u8; 32])).await;

        let control = cp(&state);
        let lookup = control
            .enrollment_lookup(&t2)
            .await
            .unwrap()
            .expect("enrolled tagma resolves");
        assert_eq!(lookup.user_id, user_id);
        let mut expected = vec![t1.clone(), t2.clone()];
        expected.sort();
        assert_eq!(lookup.enrolled_tagmas, expected);
        assert!(!lookup.enrolled_tagmas.contains(&foreign));
    }

    /// An unknown tagma id collapses to `None` (404 on the wire).
    #[tokio::test]
    async fn enrollment_lookup_unknown_tagma_is_none() {
        let state = make_state().await;
        let control = cp(&state);
        assert!(
            control
                .enrollment_lookup(&TagmaId::random())
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Pending and revoked tagmas collapse to `None` -- the same population
    /// `verify_bearer` rejects: a tagma that cannot authenticate is also not
    /// addressable as a delivery target.
    #[tokio::test]
    async fn enrollment_lookup_pending_and_revoked_are_none() {
        let state = make_state().await;
        let user_id = seed_user(&state, "owner").await;
        let (pending, _) = seed_tagma(&state, &user_id, Ed25519PublicKey(vec![0u8; 32])).await;
        let (revoked, _) = seed_tagma(&state, &user_id, Ed25519PublicKey(vec![1u8; 32])).await;

        // Flip the first tagma back to pending (enrolled_at cleared).
        let row = tagmata::Entity::find_by_id(pending.to_string())
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut am: tagmata::ActiveModel = row.into();
        am.enrolled_at = Set(None);
        am.update(&state.db).await.unwrap();

        // Revoke the second.
        let row = tagmata::Entity::find_by_id(revoked.to_string())
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut am: tagmata::ActiveModel = row.into();
        am.revoked_at = Set(Some(OffsetDateTime::now_utc()));
        am.update(&state.db).await.unwrap();

        let control = cp(&state);
        assert!(control.enrollment_lookup(&pending).await.unwrap().is_none());
        assert!(control.enrollment_lookup(&revoked).await.unwrap().is_none());
    }

    /// A tagma owned by a disabled account resolves to `None` (fail closed),
    /// matching `verify_bearer`'s owner-disabled arm.
    #[tokio::test]
    async fn enrollment_lookup_owner_disabled_is_none() {
        let state = make_state().await;
        let user_id = seed_user(&state, "owner").await;
        let (tagma_id, _) = seed_tagma(&state, &user_id, Ed25519PublicKey(vec![0u8; 32])).await;

        let row = users::Entity::find_by_id(user_id.to_string())
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut am: users::ActiveModel = row.into();
        am.disabled_at = Set(Some(OffsetDateTime::now_utc()));
        am.update(&state.db).await.unwrap();

        let control = cp(&state);
        assert!(
            control
                .enrollment_lookup(&tagma_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
