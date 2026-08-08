//! Email management -- the self-service surface for the optional contact
//! channel (`/v1/me/emails`).
//!
//! Email is decoupled from login (username) and from the WebAuthn `user.id`.
//! An authenticated user may link one or more addresses here; each starts
//! UNVERIFIED and must prove inbox ownership via a single-use verification token
//! before it can be made the primary contact address. Verification works
//! end-to-end today via the logging transport (the token is logged); swapping a
//! real SMTP provider is a one-line construction change (see [`crate::notify`]).
//!
//! Security model: adding an unverified address is low-risk (it is the user's
//! own account), so it is NOT gated behind the one-shot step-up (that is
//! reserved for binding a new passkey). Making an address primary requires it to
//! be verified, so an attacker who only has a borrowed session cannot redirect
//! the contact/recovery channel to an address they have not proven they control.

use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::emails;
use crate::db::map_db_err;
use crate::email;
use crate::routes::session::EmailSummary;
use crate::state::SharedState;
use crate::token::EMAIL_VERIFY;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use kallip_agora_common::ids::UserId;
use kallip_common::authtoken::{MintedToken, TokenHash};
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, SqlErr, TransactionTrait,
};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

/// Name of the `emails.address` global unique index. Matched against the
/// Postgres unique-violation message to map a duplicate-address insert to a
/// clean 409.
const ADDRESS_UNIQUE_CONSTRAINT: &str = "uniq_emails_address";

/// How long a pending verification token is redeemable, set at `add_email`.
const VERIFY_TOKEN_TTL: time::Duration = time::Duration::hours(24);

/// Per-account cap on linked addresses. Bounds DB rows and (once SMTP is wired)
/// outbound verification mail per account.
const MAX_EMAILS_PER_ACCOUNT: usize = 10;

/// The cookie-authed management surface: list, make-primary, remove. Rides the
/// session router (no rate layer; a signed-in user reading their own list must
/// not be throttled).
pub fn session_router() -> Router<SharedState> {
    Router::new()
        .route("/me/emails", get(list_emails))
        .route("/me/emails/{id}", post(make_primary).delete(remove_email))
}

/// `POST /me/emails` (add_email) -- the send-triggering mutation. Layered with
/// the per-IP auth rate limiter at router assembly so it cannot be used as an
/// outbound-mail amplifier (see routes.rs).
pub fn write_router() -> Router<SharedState> {
    Router::new().route("/me/emails", post(add_email))
}

/// `POST /me/emails/verify` -- unauthenticated (click-from-inbox). Layered with
/// the per-IP auth rate limiter; the 256-bit single-use token remains the real
/// barrier, this is defense-in-depth. Split out of the cookie-auth group so an
/// unauthenticated handler does not ride the session router.
pub fn verify_router() -> Router<SharedState> {
    Router::new().route("/me/emails/verify", post(verify_email))
}

#[derive(Deserialize)]
struct AddEmailRequest {
    address: String,
}

#[derive(Deserialize)]
struct VerifyEmailRequest {
    token: String,
}

async fn list_emails(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<Vec<EmailSummary>>, ApiError> {
    let user_id = require_user(&principal)?;
    let owned = load_owned(&state, user_id).await?;
    Ok(Json(owned.into_iter().map(EmailSummary::from).collect()))
}

async fn add_email(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(req): Json<AddEmailRequest>,
) -> Result<Json<EmailSummary>, ApiError> {
    let user_id = require_user(&principal)?;
    let address = email::normalize(&req.address)?;

    // Per-account cap: bounds DB rows and (once SMTP is wired) outbound mail.
    let owned_count = emails::Entity::find()
        .filter(emails::Column::AccountId.eq(user_id.to_string()))
        .count(&state.db)
        .await
        .map_err(map_db_err)?;
    if owned_count >= MAX_EMAILS_PER_ACCOUNT as u64 {
        return Err(ApiError::conflict(
            "too many email addresses on this account",
        ));
    }

    // Mint the single-use verification token up front. Only its hash is stored;
    // the plaintext is delivered to the address via the transport. The token
    // expires after VERIFY_TOKEN_TTL so a leaked log line / compromised inbox
    // cannot redeem it indefinitely.
    let token = MintedToken::generate(EMAIL_VERIFY);
    let token_hash = token.hash().as_bytes().to_vec();

    // A new address is NEVER auto-primary: a primary must be a verified address
    // the user explicitly promotes (see `make_primary`), so an attacker with a
    // borrowed session cannot redirect the contact channel to an address they
    // have not proven they control. This also avoids a count-then-insert race
    // on `is_primary`.
    let now = OffsetDateTime::now_utc();
    let insert_result = emails::ActiveModel {
        id: Set(Uuid::new_v4()),
        account_id: Set(user_id.to_string()),
        address: Set(address.clone()),
        is_primary: Set(false),
        verified_at: Set(None),
        verification_token_hash: Set(Some(token_hash)),
        verification_token_expires_at: Set(Some(now + VERIFY_TOKEN_TTL)),
        added_at: Set(now),
    }
    .insert(&state.db)
    .await;
    let model = match insert_result {
        Ok(m) => m,
        Err(e) => {
            // A duplicate address -> 409. The address is globally unique, so the
            // conflict is either this same account or another; a single generic
            // message is used so an authenticated caller cannot probe which
            // (mirrors the no-reason-leak rule on the auth gates).
            if let Some(SqlErr::UniqueConstraintViolation(msg)) = e.sql_err()
                && msg.contains(ADDRESS_UNIQUE_CONSTRAINT)
            {
                return Err(ApiError::conflict("email address already linked"));
            }
            return Err(map_db_err(e));
        }
    };

    // Deliver the verification token. LoggingTransport just emits it.
    state.mail.send_verification(&address, token.secret()).await;

    Ok(Json(EmailSummary::from(model)))
}

async fn verify_email(
    State(state): State<SharedState>,
    Json(req): Json<VerifyEmailRequest>,
) -> Result<Json<EmailSummary>, ApiError> {
    // Resolve by token hash (constant-time on our side; the index is the lookup
    // key). The find + consume run under a row lock in one txn so two concurrent
    // verifies of the same token cannot both succeed: the second re-reads under
    // the lock and finds the hash already cleared -> 404. A wrong / expired /
    // already-consumed token all surface as the same 404, leaking nothing about
    // which address was targeted.
    let token_hash = TokenHash::of(&req.token).as_bytes().to_vec();
    let outcome = state
        .db
        .transaction::<_, _, crate::db::TxnError>(|txn| {
            let token_hash = token_hash.clone();
            Box::pin(async move {
                let row = emails::Entity::find()
                    .filter(emails::Column::VerificationTokenHash.eq(token_hash))
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| {
                        crate::db::TxnError::Api(ApiError::not_found(
                            "unknown or expired verification token",
                        ))
                    })?;
                let now = OffsetDateTime::now_utc();
                // Expiry: NULL (legacy rows) is treated as not-yet-expiring.
                let expired = row
                    .verification_token_expires_at
                    .is_some_and(|at| at <= now);
                if expired {
                    // The expiry check rejects; the stale hash lingers in
                    // `idx_emails_verify_token` only until the address is
                    // re-verified or removed. Clearing it here would not persist
                    // anyway -- this closure returns Err, so the enclosing txn
                    // rolls any UPDATE back.
                    return Err(crate::db::TxnError::Api(ApiError::not_found(
                        "unknown or expired verification token",
                    )));
                }
                let mut am: emails::ActiveModel = row.into();
                am.verified_at = Set(Some(now));
                am.verification_token_hash = Set(None);
                am.verification_token_expires_at = Set(None);
                let model = am.update(txn).await?;
                Ok(EmailSummary::from(model))
            })
        })
        .await;
    Ok(Json(crate::db::flatten_txn(outcome)?))
}

async fn make_primary(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Result<Json<EmailSummary>, ApiError> {
    let user_id = require_user(&principal)?;

    // Clear-then-set under one txn with the account's emails locked FOR UPDATE,
    // so the partial unique index `WHERE is_primary` cannot reject a naive
    // set-first ordering and the at-most-one-primary invariant holds.
    let outcome = state
        .db
        .transaction::<_, _, crate::db::TxnError>(|txn| {
            let user_id = user_id.to_string();
            Box::pin(async move {
                let owned = emails::Entity::find()
                    .filter(emails::Column::AccountId.eq(user_id.clone()))
                    .lock_exclusive()
                    .all(txn)
                    .await?;
                let target = owned.iter().find(|e| e.id == id).ok_or_else(|| {
                    crate::db::TxnError::Api(ApiError::not_found("unknown email"))
                })?;
                if target.verified_at.is_none() {
                    return Err(crate::db::TxnError::Api(ApiError::bad_request(
                        "email must be verified before it can be primary",
                    )));
                }
                // Clear the current primary (if any), then set the target.
                for e in &owned {
                    if e.is_primary && e.id != id {
                        let mut am: emails::ActiveModel = e.clone().into();
                        am.is_primary = Set(false);
                        am.update(txn).await?;
                    }
                }
                let mut am: emails::ActiveModel = target.clone().into();
                am.is_primary = Set(true);
                let updated = am.update(txn).await?;
                Ok(EmailSummary::from(updated))
            })
        })
        .await;
    Ok(Json(crate::db::flatten_txn(outcome)?))
}

async fn remove_email(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<EmailSummary>>, ApiError> {
    let user_id = require_user(&principal)?;

    // Lock the account's emails FOR UPDATE so the delete + re-primary is atomic
    // and the at-most-one-primary invariant holds.
    let outcome = state
        .db
        .transaction::<_, _, crate::db::TxnError>(|txn| {
            let user_id = user_id.to_string();
            Box::pin(async move {
                let owned = emails::Entity::find()
                    .filter(emails::Column::AccountId.eq(user_id.clone()))
                    .lock_exclusive()
                    .all(txn)
                    .await?;
                let target = owned.iter().find(|e| e.id == id).ok_or_else(|| {
                    crate::db::TxnError::Api(ApiError::not_found("unknown email"))
                })?;
                let was_primary = target.is_primary;
                emails::Entity::delete_by_id(id).exec(txn).await?;

                // If we removed the primary, hand off to a VERIFIED successor so
                // the account keeps a trusted contact address. We never promote
                // an unverified address (a primary must be verified); if none of
                // the remaining addresses is verified, the account simply has no
                // primary until the user verifies and promotes one.
                if was_primary {
                    let remaining: Vec<emails::Model> =
                        owned.into_iter().filter(|e| e.id != id).collect();
                    if let Some(promote) = remaining.iter().find(|e| e.verified_at.is_some()) {
                        let mut am: emails::ActiveModel = promote.clone().into();
                        am.is_primary = Set(true);
                        am.update(txn).await?;
                    }
                }
                let after = emails::Entity::find()
                    .filter(emails::Column::AccountId.eq(user_id))
                    .all(txn)
                    .await?;
                Ok(after
                    .into_iter()
                    .map(EmailSummary::from)
                    .collect::<Vec<_>>())
            })
        })
        .await;
    Ok(Json(crate::db::flatten_txn(outcome)?))
}

/// All emails owned by `user_id`, oldest first.
async fn load_owned(state: &SharedState, user_id: &UserId) -> Result<Vec<emails::Model>, ApiError> {
    emails::Entity::find()
        .filter(emails::Column::AccountId.eq(user_id.to_string()))
        .order_by_asc(emails::Column::AddedAt)
        .all(&state.db)
        .await
        .map_err(map_db_err)
}

#[cfg(test)]
mod tests {
    //! Handler-level tests for the email-management surface: add, verify,
    //! make-primary (verified gate + swap), remove (primary promotion), and the
    //! global address-uniqueness 409. The handlers are called directly with a
    //! seeded `Principal::User`; the WebAuthn ceremonies are irrelevant here.

    use super::{
        AddEmailRequest, VerifyEmailRequest, add_email, list_emails, make_primary, remove_email,
        verify_email,
    };
    use crate::auth::{AuthPrincipal, Principal};
    use crate::db::entity::emails;
    use crate::test_helpers::{make_state, seed_user};
    use crate::token::EMAIL_VERIFY;
    use axum::Json;
    use axum::extract::State;
    use kallip_common::authtoken::MintedToken;
    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    };
    use uuid::Uuid;

    /// Overwrite an email row's verification-token hash with a known token and
    /// return it (`add_email` mints one the test cannot observe). The caller
    /// then drives `verify_email` with the secret.
    async fn plant_token(state: &crate::state::SharedState, email_id: Uuid) -> MintedToken {
        let token = MintedToken::generate(EMAIL_VERIFY);
        let row = emails::Entity::find_by_id(email_id)
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut am: emails::ActiveModel = row.into();
        am.verification_token_hash = Set(Some(token.hash().as_bytes().to_vec()));
        am.update(&state.db).await.unwrap();
        token
    }

    /// `add_email` links an unverified, NON-primary address (a primary must be
    /// verified and explicitly promoted).
    #[tokio::test]
    async fn add_email_starts_unverified_non_primary() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let Json(sum) = add_email(
            State(state.clone()),
            AuthPrincipal(Principal::User(user.clone())),
            Json(AddEmailRequest {
                address: "Alice@Example.Test".to_string(),
            }),
        )
        .await
        .expect("add ok");
        assert_eq!(sum.address, "Alice@example.test"); // domain lowercased
        assert!(!sum.is_primary);
        assert!(sum.verified_at.is_none());

        let Json(list) = list_emails(State(state), AuthPrincipal(Principal::User(user)))
            .await
            .expect("list ok");
        assert_eq!(list.len(), 1);
    }

    /// A second account cannot link an address another account owns (global
    /// unique -> 409).
    #[tokio::test]
    async fn add_email_rejects_address_owned_by_another_account() {
        let state = make_state().await;
        let alice = seed_user(&state, "alice").await;
        let Json(_) = add_email(
            State(state.clone()),
            AuthPrincipal(Principal::User(alice)),
            Json(AddEmailRequest {
                address: "shared@Example.Test".to_string(),
            }),
        )
        .await
        .expect("first add ok");

        // Same local part, domain in different case: canonicalizes (domain is
        // lowercased) to the same address as alice's -> 409.
        let bob = seed_user(&state, "bob").await;
        let err = add_email(
            State(state),
            AuthPrincipal(Principal::User(bob)),
            Json(AddEmailRequest {
                address: "shared@example.test".to_string(),
            }),
        )
        .await
        .expect_err("duplicate address must be rejected");
        assert_eq!(err.status, 409);
    }

    /// `verify_email` clears the token and stamps `verified_at`; the token is
    /// single-use (a second attempt 404s).
    #[tokio::test]
    async fn verify_email_marks_verified_and_is_single_use() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let Json(added) = add_email(
            State(state.clone()),
            AuthPrincipal(Principal::User(user)),
            Json(AddEmailRequest {
                address: "alice@example.test".to_string(),
            }),
        )
        .await
        .expect("add ok");
        let id = Uuid::parse_str(&added.id).unwrap();
        let token = plant_token(&state, id).await;

        let Json(sum) = verify_email(
            State(state.clone()),
            Json(VerifyEmailRequest {
                token: token.secret().to_string(),
            }),
        )
        .await
        .expect("verify ok");
        assert!(sum.verified_at.is_some());

        // Single-use: the hash was cleared, so the same token no longer resolves.
        let err = verify_email(
            State(state),
            Json(VerifyEmailRequest {
                token: token.secret().to_string(),
            }),
        )
        .await
        .expect_err("consumed token must be rejected");
        assert_eq!(err.status, 404);
    }

    /// `make_primary` refuses an unverified address.
    #[tokio::test]
    async fn make_primary_requires_verified() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let Json(second) = add_email(
            State(state.clone()),
            AuthPrincipal(Principal::User(user.clone())),
            Json(AddEmailRequest {
                address: "two@example.test".to_string(),
            }),
        )
        .await
        .expect("add ok");

        let id = Uuid::parse_str(&second.id).unwrap();
        let err = make_primary(
            State(state),
            AuthPrincipal(Principal::User(user)),
            axum::extract::Path(id),
        )
        .await
        .expect_err("unverified must be rejected");
        assert_eq!(err.status, 400);
    }

    /// `make_primary` swaps: clears the old primary and sets the new (verified)
    /// one -- the at-most-one-primary invariant holds.
    #[tokio::test]
    async fn make_primary_swaps_to_verified() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let principal = AuthPrincipal(Principal::User(user));
        let Json(first) = add_email(
            State(state.clone()),
            AuthPrincipal(principal.0.clone()),
            Json(AddEmailRequest {
                address: "one@example.test".to_string(),
            }),
        )
        .await
        .expect("add one");
        let Json(second) = add_email(
            State(state.clone()),
            AuthPrincipal(principal.0.clone()),
            Json(AddEmailRequest {
                address: "two@example.test".to_string(),
            }),
        )
        .await
        .expect("add two");

        // Verify both.
        let t1 = plant_token(&state, Uuid::parse_str(&first.id).unwrap()).await;
        let t2 = plant_token(&state, Uuid::parse_str(&second.id).unwrap()).await;
        let _ = verify_email(
            State(state.clone()),
            Json(VerifyEmailRequest {
                token: t1.secret().to_string(),
            }),
        )
        .await
        .expect("verify one");
        let _ = verify_email(
            State(state.clone()),
            Json(VerifyEmailRequest {
                token: t2.secret().to_string(),
            }),
        )
        .await
        .expect("verify two");

        // Make the first primary, then swap to the second.
        let Json(now_primary) = make_primary(
            State(state.clone()),
            AuthPrincipal(principal.0.clone()),
            axum::extract::Path(Uuid::parse_str(&first.id).unwrap()),
        )
        .await
        .expect("make first primary");
        assert!(now_primary.is_primary);

        let Json(swapped) = make_primary(
            State(state.clone()),
            AuthPrincipal(principal.0.clone()),
            axum::extract::Path(Uuid::parse_str(&second.id).unwrap()),
        )
        .await
        .expect("swap to second");
        assert!(swapped.is_primary);

        // Exactly one primary remains, and it is the second.
        let Json(list) = list_emails(State(state), principal).await.expect("list");
        assert_eq!(list.iter().filter(|e| e.is_primary).count(), 1);
        assert!(
            list.iter()
                .find(|e| e.address == "two@example.test")
                .unwrap()
                .is_primary
        );
    }

    /// Removing the primary promotes a VERIFIED successor; if none of the
    /// remaining addresses is verified, the account is left with no primary.
    #[tokio::test]
    async fn remove_primary_promotes_verified_only() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let principal = AuthPrincipal(Principal::User(user));
        let Json(one) = add_email(
            State(state.clone()),
            AuthPrincipal(principal.0.clone()),
            Json(AddEmailRequest {
                address: "one@example.test".to_string(),
            }),
        )
        .await
        .expect("add one");
        let Json(two) = add_email(
            State(state.clone()),
            AuthPrincipal(principal.0.clone()),
            Json(AddEmailRequest {
                address: "two@example.test".to_string(),
            }),
        )
        .await
        .expect("add two");
        // Verify both, make `one` primary.
        let t1 = plant_token(&state, Uuid::parse_str(&one.id).unwrap()).await;
        let t2 = plant_token(&state, Uuid::parse_str(&two.id).unwrap()).await;
        let _ = verify_email(
            State(state.clone()),
            Json(VerifyEmailRequest {
                token: t1.secret().to_string(),
            }),
        )
        .await
        .expect("verify one");
        let _ = verify_email(
            State(state.clone()),
            Json(VerifyEmailRequest {
                token: t2.secret().to_string(),
            }),
        )
        .await
        .expect("verify two");
        let _ = make_primary(
            State(state.clone()),
            AuthPrincipal(principal.0.clone()),
            axum::extract::Path(Uuid::parse_str(&one.id).unwrap()),
        )
        .await
        .expect("make one primary");

        // Remove the primary; `two` is verified -> promoted.
        let Json(remaining) = remove_email(
            State(state.clone()),
            AuthPrincipal(principal.0.clone()),
            axum::extract::Path(Uuid::parse_str(&one.id).unwrap()),
        )
        .await
        .expect("remove primary");
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].is_primary);

        // Now remove that promoted primary; nothing verified remains -> no primary.
        let Json(remaining) = remove_email(
            State(state),
            principal,
            axum::extract::Path(Uuid::parse_str(&two.id).unwrap()),
        )
        .await
        .expect("remove last");
        assert!(remaining.is_empty());
    }

    /// `make_primary` and `remove_email` are owner-scoped: an id owned by
    /// another account is a 404, not a mutation.
    #[tokio::test]
    async fn mutations_are_owner_scoped() {
        let state = make_state().await;
        let alice = seed_user(&state, "alice").await;
        let Json(alice_email) = add_email(
            State(state.clone()),
            AuthPrincipal(Principal::User(alice)),
            Json(AddEmailRequest {
                address: "alice@example.test".to_string(),
            }),
        )
        .await
        .expect("add ok");
        let id = Uuid::parse_str(&alice_email.id).unwrap();
        let bob = seed_user(&state, "bob").await;

        let err = make_primary(
            State(state.clone()),
            AuthPrincipal(Principal::User(bob.clone())),
            axum::extract::Path(id),
        )
        .await
        .expect_err("bob must not touch alice's email");
        assert_eq!(err.status, 404);

        let err = remove_email(
            State(state),
            AuthPrincipal(Principal::User(bob)),
            axum::extract::Path(id),
        )
        .await
        .expect_err("bob must not remove alice's email");
        assert_eq!(err.status, 404);
    }

    /// A re-add of an already-owned address hits the global unique index and
    /// returns a 409 with a single generic message that does NOT distinguish
    /// self from another account (so an authenticated caller cannot probe which).
    #[tokio::test]
    async fn add_email_re_add_own_address_returns_generic_409() {
        let state = make_state().await;
        let alice = seed_user(&state, "alice").await;
        let req = Json(AddEmailRequest {
            address: "mine@example.test".to_string(),
        });
        let Json(_) = add_email(
            State(state.clone()),
            AuthPrincipal(Principal::User(alice.clone())),
            req,
        )
        .await
        .expect("first add ok");

        let err = add_email(
            State(state),
            AuthPrincipal(Principal::User(alice)),
            Json(AddEmailRequest {
                address: "mine@example.test".to_string(),
            }),
        )
        .await
        .expect_err("re-add own address must be rejected");
        assert_eq!(err.status, 409);
        assert_eq!(err.message, "email address already linked");
    }

    /// `verify_email` is single-use UNDER CONCURRENCY: two simultaneous verifies
    /// of the same token resolve to exactly one 200 and one 404. The row lock in
    /// the verify txn is what serializes them.
    #[tokio::test]
    async fn verify_email_concurrent_double_consume_one_wins() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let Json(added) = add_email(
            State(state.clone()),
            AuthPrincipal(Principal::User(user)),
            Json(AddEmailRequest {
                address: "alice@example.test".to_string(),
            }),
        )
        .await
        .expect("add ok");
        let id = Uuid::parse_str(&added.id).unwrap();
        let token = plant_token(&state, id).await;
        let secret = token.secret().to_string();

        let (a, b) = tokio::join!(
            async {
                verify_email(
                    State(state.clone()),
                    Json(VerifyEmailRequest {
                        token: secret.clone(),
                    }),
                )
                .await
            },
            async {
                verify_email(
                    State(state.clone()),
                    Json(VerifyEmailRequest {
                        token: secret.clone(),
                    }),
                )
                .await
            }
        );
        // Exactly one succeeds; the other is the 404 single-use rejection.
        assert!(a.is_ok() ^ b.is_ok(), "exactly one verify must succeed");
        if let Err(e) = &a {
            assert_eq!(e.status, 404);
        }
        if let Err(e) = &b {
            assert_eq!(e.status, 404);
        }
    }

    /// `make_primary` under concurrency: two simultaneous promotions of
    /// different verified addresses leave the account with EXACTLY one primary.
    /// Exercises the load-bearing `lock_exclusive()` + clear-then-set txn.
    #[tokio::test]
    async fn concurrent_make_primary_leaves_one_primary() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        // Two verified addresses.
        let mut verified_ids = vec![];
        for addr in ["a@example.test", "b@example.test"] {
            let Json(added) = add_email(
                State(state.clone()),
                AuthPrincipal(Principal::User(user.clone())),
                Json(AddEmailRequest {
                    address: addr.to_string(),
                }),
            )
            .await
            .expect("add ok");
            let id = Uuid::parse_str(&added.id).unwrap();
            let token = plant_token(&state, id).await;
            let Json(_) = verify_email(
                State(state.clone()),
                Json(VerifyEmailRequest {
                    token: token.secret().to_string(),
                }),
            )
            .await
            .expect("verify ok");
            verified_ids.push(id);
        }
        let [id_a, id_b] = [verified_ids[0], verified_ids[1]];

        let (ra, rb) = tokio::join!(
            async {
                make_primary(
                    State(state.clone()),
                    AuthPrincipal(Principal::User(user.clone())),
                    axum::extract::Path(id_a),
                )
                .await
            },
            async {
                make_primary(
                    State(state.clone()),
                    AuthPrincipal(Principal::User(user.clone())),
                    axum::extract::Path(id_b),
                )
                .await
            }
        );
        // Both can succeed (last-write-wins), but the invariant is one primary.
        assert!(ra.is_ok() && rb.is_ok());

        let primaries = emails::Entity::find()
            .filter(emails::Column::AccountId.eq(user.to_string()))
            .filter(emails::Column::IsPrimary.eq(true))
            .count(&state.db)
            .await
            .expect("count primaries");
        assert_eq!(primaries, 1, "exactly one primary must remain");
    }
}
