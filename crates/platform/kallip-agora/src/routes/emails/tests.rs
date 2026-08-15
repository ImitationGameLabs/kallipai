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
