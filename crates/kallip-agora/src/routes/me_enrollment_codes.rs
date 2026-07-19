//! Self-service tagma enrollment codes: a signed-in user mints, lists, and
//! revokes their OWN single-use `sk-enroll-...` codes (the credential a herald
//! redeems at `POST /v1/tagmata` to enroll a device). This is the user-facing
//! counterpart to the admin `/v1/admin/enrollment-codes` surface, which is
//! retained for operator use.
//!
//! All three routes are cookie-authenticated (`require_user`) and intentionally
//! NOT rate-limited: the per-user live cap ([`MAX_LIVE_ENROLLMENT_CODES`]) is
//! the bound on stored rows, and only an authenticated user can reach the
//! surface. Mint-rate abuse is a closed-beta residual (revisit when personal
//! access tokens or a public signup surface land). The whole-`v1` `csrf_guard`
//! covers POST and DELETE.
//!
//! The minted plaintext is returned ONCE and only its SHA-256 hash is stored
//! ([`enrollment_tokens`]); the list therefore never carries the secret.

use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::enrollment_tokens;
use crate::db::map_db_err;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use kallip_common::authtoken::MintedToken;
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder,
};
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::state::SharedState;
use crate::token::ENROLLMENT;

/// Max live (unconsumed + unrevoked) enrollment codes per user. Bounds
/// `enrollment_tokens` growth against a user who mints and forgets; count-then-
/// insert, so the cap is soft under true concurrency (mirrors the ceremony
/// in-flight cap in `routes/auth.rs`). Revoked/consumed rows do not count.
const MAX_LIVE_ENROLLMENT_CODES: u64 = 8;

/// The cookie-authenticated self-service enrollment-code surface. Mounted under
/// `/v1/me/enrollment-codes` WITHOUT the rate-limit layer (only the unauth
/// ceremony-begin + enroll surfaces are rate-limited; see `routes::router`).
pub fn me_enrollment_codes_router() -> Router<SharedState> {
    Router::new()
        .route("/me/enrollment-codes", post(mint).get(list))
        .route("/me/enrollment-codes/{id}", delete(revoke))
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MintResponse {
    /// `sk-enroll-...` plaintext; returned once. The agora retains only its hash.
    code: String,
    /// Public row id (the DELETE target). A synthetic UUID.
    id: Uuid,
    created_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

#[derive(Serialize)]
struct EnrollmentCodeSummary {
    id: Uuid,
    created_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// Mint a single-use enrollment code bound to the caller.
async fn mint(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<MintResponse>, ApiError> {
    let user_id = require_user(&principal)?;
    let now = OffsetDateTime::now_utc();

    // Bound the caller's live (unconsumed + unrevoked) codes. Count-then-insert
    // (soft under concurrency); the predicate matches `list` exactly so a user at
    // the cap sees precisely the rows that count toward it.
    let live = enrollment_tokens::Entity::find()
        .filter(enrollment_tokens::Column::UserId.eq(user_id.to_string()))
        .filter(enrollment_tokens::Column::ConsumedAt.is_null())
        .filter(enrollment_tokens::Column::RevokedAt.is_null())
        .count(&state.db)
        .await
        .map_err(map_db_err)?;
    if live >= MAX_LIVE_ENROLLMENT_CODES {
        return Err(ApiError::too_many_requests(
            "too many live enrollment codes",
        ));
    }

    let code = MintedToken::generate(ENROLLMENT);
    let plaintext = code.secret().to_string();
    let id = Uuid::new_v4();
    let expires_at = now + state.limits.enrollment_code_ttl;
    enrollment_tokens::ActiveModel {
        id: Set(id),
        token_hash: Set(code.hash().as_bytes().to_vec()),
        user_id: Set(user_id.to_string()),
        created_at: Set(now),
        expires_at: Set(expires_at),
        consumed_at: Set(None),
        consumed_by_tagma: Set(None),
        revoked_at: Set(None),
    }
    .insert(&state.db)
    .await
    .map_err(map_db_err)?;

    Ok(Json(MintResponse {
        code: plaintext,
        id,
        created_at: now,
        expires_at,
    }))
}

/// List the caller's PENDING codes (unconsumed + unrevoked), newest first. The
/// plaintext is unrecoverable (only its hash is stored) and is never sent.
async fn list(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<Vec<EnrollmentCodeSummary>>, ApiError> {
    let user_id = require_user(&principal)?;
    let rows = enrollment_tokens::Entity::find()
        .filter(enrollment_tokens::Column::UserId.eq(user_id.to_string()))
        .filter(enrollment_tokens::Column::ConsumedAt.is_null())
        .filter(enrollment_tokens::Column::RevokedAt.is_null())
        .order_by_desc(enrollment_tokens::Column::CreatedAt)
        .all(&state.db)
        .await
        .map_err(map_db_err)?;
    let items = rows
        .into_iter()
        .map(|r| EnrollmentCodeSummary {
            id: r.id,
            created_at: r.created_at,
            expires_at: r.expires_at,
        })
        .collect();
    Ok(Json(items))
}

/// Revoke one of the caller's codes. Owner-scoped: a missing or other-user id
/// returns 404 (no existence oracle across users). Idempotent + race-free via a
/// conditional UPDATE that only touches rows still live, so two concurrent
/// revokes cannot clobber the first-revoked timestamp. 204 either way for a row
/// that exists and is/was the caller's.
async fn revoke(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let user_id = require_user(&principal)?;
    // Existence + ownership check first so an unknown / other-user id is a clean
    // 404 rather than a silent 204 (the conditional UPDATE alone could not tell
    // those apart from an already-revoked own row).
    let owned = enrollment_tokens::Entity::find()
        .filter(enrollment_tokens::Column::Id.eq(id))
        .filter(enrollment_tokens::Column::UserId.eq(user_id.to_string()))
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    if owned.is_none() {
        return Err(ApiError::not_found("unknown enrollment code"));
    }
    // Only advance `revoked_at` if still NULL; a second revoke leaves the
    // original timestamp intact (audit-relevant).
    enrollment_tokens::Entity::update_many()
        .filter(enrollment_tokens::Column::Id.eq(id))
        .filter(enrollment_tokens::Column::RevokedAt.is_null())
        .col_expr(
            enrollment_tokens::Column::RevokedAt,
            sea_orm::sea_query::Expr::value(OffsetDateTime::now_utc()),
        )
        .exec(&state.db)
        .await
        .map_err(map_db_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    //! Handler-level round-trips for the self-service enrollment-code surface.

    use std::time::Duration;

    use axum::Json;
    use axum::extract::State;
    use kallip_agora_common::ids::UserId;
    use time::{Duration as TimeDuration, OffsetDateTime};
    use uuid::Uuid;

    use super::{MintResponse, list, mint};
    use crate::auth::{AuthPrincipal, Principal};
    use crate::db::entity::enrollment_tokens;
    use crate::state::SharedState;
    use crate::test_helpers::{make_state, seed_user};
    use kallip_common::authtoken::TokenHash;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    /// A distinct 32-byte token hash for seed rows (the column is UNIQUE, so the
    /// all-zero placeholder cannot be reused across rows).
    fn distinct_hash() -> Vec<u8> {
        let mut h = [0u8; 32];
        h[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        h.to_vec()
    }

    /// Seed a live enrollment-code row owned by `user`, returning its row id.
    async fn seed_code(state: &SharedState, user: &UserId) -> Uuid {
        let now = OffsetDateTime::now_utc();
        let id = Uuid::new_v4();
        enrollment_tokens::ActiveModel {
            id: Set(id),
            token_hash: Set(distinct_hash()),
            user_id: Set(user.to_string()),
            created_at: Set(now),
            expires_at: Set(now + TimeDuration::seconds(600)),
            consumed_at: Set(None),
            consumed_by_tagma: Set(None),
            revoked_at: Set(None),
        }
        .insert(&state.db)
        .await
        .expect("insert code");
        id
    }

    /// `mint` returns a plaintext that hashes to a stored row, and `list` shows
    /// it pending.
    #[tokio::test]
    async fn mint_then_list_round_trip() {
        let state = make_state(Duration::from_secs(2)).await;
        let user = seed_user(&state, "alice", "alice@example.test").await;
        let principal = AuthPrincipal(Principal::User(user.clone()));

        let Json(MintResponse { code, id, .. }) = mint(State(state.clone()), principal.clone())
            .await
            .expect("mint");
        assert!(code.starts_with("sk-enroll-"));
        let expected_hash = TokenHash::of(&code).as_bytes().to_vec();
        let row = enrollment_tokens::Entity::find_by_id(id)
            .one(&state.db)
            .await
            .expect("read code")
            .expect("code row exists");
        assert_eq!(row.token_hash, expected_hash);

        let Json(listed) = list(State(state.clone()), principal.clone())
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
    }

    /// An expired-but-unconsumed code still counts toward the cap (the predicate
    /// is consumed/revoked, not expires_at), so minting past the cap 429s even
    /// with expired rows present.
    #[tokio::test]
    async fn cap_counts_expired_unconsumed_codes() {
        let state = make_state(Duration::from_secs(2)).await;
        let user = seed_user(&state, "bob", "bob@example.test").await;
        let principal = AuthPrincipal(Principal::User(user.clone()));
        // Seed the cap (8) of already-expired, unconsumed, unrevoked codes.
        let now = OffsetDateTime::now_utc();
        for _ in 0..super::MAX_LIVE_ENROLLMENT_CODES {
            enrollment_tokens::ActiveModel {
                id: Set(Uuid::new_v4()),
                token_hash: Set(distinct_hash()),
                user_id: Set(user.to_string()),
                created_at: Set(now - TimeDuration::days(2)),
                expires_at: Set(now - TimeDuration::days(1)),
                consumed_at: Set(None),
                consumed_by_tagma: Set(None),
                revoked_at: Set(None),
            }
            .insert(&state.db)
            .await
            .expect("seed expired code");
        }
        match mint(State(state), principal).await {
            Err(e) => assert_eq!(e.status, 429),
            Ok(_) => panic!("cap reached must 429"),
        }
    }

    /// Revoking twice is idempotent: both return 204, and `revoked_at` keeps its
    /// original timestamp.
    #[tokio::test]
    async fn revoke_is_idempotent() {
        let state = make_state(Duration::from_secs(2)).await;
        let user = seed_user(&state, "carol", "carol@example.test").await;
        let id = seed_code(&state, &user).await;
        let principal = AuthPrincipal(Principal::User(user.clone()));

        let s1 = super::revoke(
            State(state.clone()),
            principal.clone(),
            axum::extract::Path(id),
        )
        .await
        .expect("first revoke");
        assert_eq!(s1, axum::http::StatusCode::NO_CONTENT);
        let first = enrollment_tokens::Entity::find_by_id(id)
            .one(&state.db)
            .await
            .expect("read")
            .expect("row")
            .revoked_at
            .expect("revoked once");

        let s2 = super::revoke(State(state.clone()), principal, axum::extract::Path(id))
            .await
            .expect("second revoke");
        assert_eq!(s2, axum::http::StatusCode::NO_CONTENT);
        let second = enrollment_tokens::Entity::find_by_id(id)
            .one(&state.db)
            .await
            .expect("read")
            .expect("row")
            .revoked_at
            .expect("still revoked");
        assert_eq!(first, second, "revoked_at must not be clobbered");
    }

    /// A consumed code is still revocable (204): the owner can clean up a code
    /// that was redeemed by a herald.
    #[tokio::test]
    async fn revoke_consumed_code_is_204() {
        let state = make_state(Duration::from_secs(2)).await;
        let user = seed_user(&state, "dan", "dan@example.test").await;
        let id = seed_code(&state, &user).await;
        // Mark it consumed.
        let row = enrollment_tokens::Entity::find_by_id(id)
            .one(&state.db)
            .await
            .expect("read")
            .expect("row");
        let mut am: enrollment_tokens::ActiveModel = row.into();
        am.consumed_at = Set(Some(OffsetDateTime::now_utc()));
        am.update(&state.db).await.expect("consume");

        let principal = AuthPrincipal(Principal::User(user));
        let s = super::revoke(State(state), principal, axum::extract::Path(id))
            .await
            .expect("revoke consumed");
        assert_eq!(s, axum::http::StatusCode::NO_CONTENT);
    }

    /// Revoking another user's code is a 404 (no cross-user existence oracle),
    /// and `list` never surfaces another user's codes.
    #[tokio::test]
    async fn owner_isolation() {
        let state = make_state(Duration::from_secs(2)).await;
        let alice = seed_user(&state, "alice", "alice@example.test").await;
        let bob = seed_user(&state, "bob", "bob@example.test").await;
        let alice_code = seed_code(&state, &alice).await;
        let bob_principal = AuthPrincipal(Principal::User(bob.clone()));

        // Bob cannot revoke Alice's code.
        match super::revoke(
            State(state.clone()),
            bob_principal.clone(),
            axum::extract::Path(alice_code),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("cross-user revoke must 404"),
        }
        // Bob's list does not include Alice's code (bob owns none).
        let Json(listed) = list(State(state), bob_principal).await.expect("list");
        assert!(listed.is_empty(), "bob must not see alice's codes");
    }
}
