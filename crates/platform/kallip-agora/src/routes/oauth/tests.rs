//! Handler tests using an injected mock provider (the trait is the seam):
//! the full begin/finish/login/create logic is exercised with NO real
//! provider round-trip. The crypto/HTTP ceremonies of real providers stay
//! integration-tested (manual, with real credentials).

use super::*;
use crate::routes::passkeys::revoke_passkey_row;
use crate::test_helpers::{
    make_state_with_oauth, make_state_with_oauth_signup_disabled, seed_passkey, seed_user,
};
use axum::body::to_bytes;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, PaginatorTrait};

/// A mock GitHub provider. The `code` a test posts becomes the resolved
/// subject: `exchange` echoes it as the `access_token`, and `fetch_identity`
/// reads it back. So a test controls whether finish sees a known (login) or
/// novel (signup) identity by passing the subject as `code`, WITHOUT needing
/// a provider-specific state per subject (one shared state per test).
struct MockGithub;

#[async_trait::async_trait]
impl OAuthProvider for MockGithub {
    fn id(&self) -> ProviderId {
        ProviderId::Github
    }
    fn authorize_url(&self, state: &str, _pkce: Option<&str>) -> url::Url {
        url::Url::parse(&format!("https://example/oauth?state={state}")).unwrap()
    }
    async fn exchange(
        &self,
        code: &str,
        _pkce_verifier: Option<&str>,
        _http: &reqwest::Client,
    ) -> Result<oauth::TokenSet, oauth::OAuthError> {
        Ok(oauth::TokenSet {
            access_token: code.to_string(),
        })
    }
    async fn fetch_identity(
        &self,
        tokens: &oauth::TokenSet,
        _http: &reqwest::Client,
    ) -> Result<oauth::ProviderIdentity, oauth::OAuthError> {
        Ok(oauth::ProviderIdentity {
            subject: tokens.access_token.clone(),
            // A non-None name exercises the claim_display_name -> identity
            // carry-through (held at finish, persisted at complete).
            display_name: Some("Ghost".to_string()),
        })
    }
}

/// A state with the mock GitHub provider. One per test; the per-call subject
/// is carried by the `code` a test posts, so a single state serves every
/// subject (and concurrency tests can hoist it out of a loop).
async fn state_with() -> SharedState {
    make_state_with_oauth(vec![Box::new(MockGithub)]).await
}

/// Insert an `oauth_states` row whose plaintext is `state_plain` (so finish
/// can redeem it), returning nothing. Models what begin persisted.
async fn seed_state(state: &SharedState, state_plain: &str, provider: &str, action: &str) {
    let now = OffsetDateTime::now_utc();
    oauth_states::ActiveModel {
        state_hash: Set(TokenHash::of(state_plain).as_bytes().to_vec()),
        provider: Set(provider.to_string()),
        action: Set(action.to_string()),
        return_path: Set(None),
        user_id: Set(None),
        pkce_verifier: Set(None),
        subject: Set(None),
        claim_display_name: Set(None),
        signup_token_hash: Set(None),
        created_at: Set(now),
        expires_at: Set(now + CHALLENGE_TTL),
    }
    .insert(&state.db)
    .await
    .expect("seed oauth state");
}

/// Like [`seed_state`] but for a LINK ceremony: `action = link` and the row
/// carries the bound `user_id` that `link_begin` persists.
async fn seed_link_state(state: &SharedState, state_plain: &str, provider: &str, user_id: &UserId) {
    let now = OffsetDateTime::now_utc();
    oauth_states::ActiveModel {
        state_hash: Set(TokenHash::of(state_plain).as_bytes().to_vec()),
        provider: Set(provider.to_string()),
        action: Set(ACTION_LINK.to_string()),
        return_path: Set(None),
        user_id: Set(Some(user_id.to_string())),
        pkce_verifier: Set(None),
        subject: Set(None),
        claim_display_name: Set(None),
        signup_token_hash: Set(None),
        created_at: Set(now),
        expires_at: Set(now + CHALLENGE_TTL),
    }
    .insert(&state.db)
    .await
    .expect("seed link oauth state");
}

async fn seed_identity(state: &SharedState, user_id: &UserId, subject: &str) {
    let now = OffsetDateTime::now_utc();
    external_identities::ActiveModel {
        id: Set(uuid::Uuid::new_v4()),
        user_id: Set(user_id.to_string()),
        provider: Set("github".to_string()),
        subject: Set(subject.to_string()),
        display_name: Set(None),
        created_at: Set(now),
        last_used_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("seed identity");
}

#[tokio::test]
async fn providers_list_enabled() {
    let state = state_with().await;
    let Json(list) = list_providers(State(state)).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "github");
}

#[tokio::test]
async fn signin_begin_writes_state_row() {
    let state = state_with().await;
    let Json(resp) = signin_begin(
        State(state.clone()),
        Path("github".to_string()),
        Json(BeginBody {
            return_path: Some("/tagmata".to_string()),
        }),
    )
    .await
    .expect("begin ok");
    assert!(resp.authorize_url.contains("state="));
    // Exactly one ceremony row, signin, no bound user.
    let rows = oauth_states::Entity::find()
        .all(&state.db)
        .await
        .expect("read states");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, ACTION_SIGNIN);
    assert!(rows[0].user_id.is_none());
    assert_eq!(rows[0].return_path.as_deref(), Some("/tagmata"));
}

#[tokio::test]
async fn finish_signin_login_mints_session() {
    let state = state_with().await;
    let user_id = seed_user(&state, "alice").await;
    seed_identity(&state, &user_id, "123").await;
    let state_plain = "known-state".to_string();
    seed_state(&state, &state_plain, "github", ACTION_SIGNIN).await;

    let resp = finish(
        State(state.clone()),
        Path("github".to_string()),
        Json(FinishBody {
            state: state_plain,
            code: "123".to_string(),
        }),
    )
    .await
    .expect("login ok");
    assert_eq!(resp.status(), StatusCode::OK);
    // A session row was minted for the existing user; the ceremony is gone.
    assert_eq!(
        crate::db::entity::sessions::Entity::find()
            .filter(crate::db::entity::sessions::Column::UserId.eq(user_id.to_string()))
            .count(&state.db)
            .await
            .expect("count sessions"),
        1
    );
    assert_eq!(
        oauth_states::Entity::find()
            .count(&state.db)
            .await
            .expect("count"),
        0
    );
}

/// The 202 needs-username body shape.
#[derive(Deserialize)]
struct NeedsUsernameBody {
    kind: String,
    signup_token: String,
    #[allow(dead_code)]
    provider: String,
}

/// Drive finish to completion and return the held ceremony's signup token,
/// asserting the response is the 202 needs-username outcome.
async fn finish_needs_username(state: SharedState, state_plain: &str, code: &str) -> String {
    let resp = finish(
        State(state),
        Path("github".to_string()),
        Json(FinishBody {
            state: state_plain.to_string(),
            code: code.to_string(),
        }),
    )
    .await
    .expect("needs-username ok");
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    let body: NeedsUsernameBody = serde_json::from_slice(&bytes).expect("deserialize");
    assert_eq!(body.kind, "needs-username");
    body.signup_token
}

/// POST a chosen username against a held signup token.
async fn complete(
    state: SharedState,
    signup_token: String,
    username: &str,
) -> Result<Response, ApiError> {
    signup_complete(
        State(state),
        Json(SignupCompleteBody {
            signup_token,
            username: username.to_string(),
        }),
    )
    .await
}

#[tokio::test]
async fn finish_unlinked_holds_claim_and_returns_signup_token() {
    // A subject nobody has linked -> the claim is held (no account created),
    // and a single-use signup token is returned.
    let state = state_with().await;
    let state_plain = "signup-state".to_string();
    seed_state(&state, &state_plain, "github", ACTION_SIGNIN).await;

    let token = finish_needs_username(state.clone(), &state_plain, "novel-999").await;
    assert!(!token.is_empty());

    // The ceremony row SURVIVES, now held: subject + signup_token_hash set.
    let row = oauth_states::Entity::find()
        .filter(oauth_states::Column::Subject.eq("novel-999"))
        .one(&state.db)
        .await
        .expect("read state")
        .expect("held row present");
    assert!(row.signup_token_hash.is_some());

    // No account was created at finish -- creation waits for the username.
    assert_eq!(
        users::Entity::find().count(&state.db).await.expect("users"),
        0
    );
    assert_eq!(
        external_identities::Entity::find()
            .count(&state.db)
            .await
            .expect("identities"),
        0
    );
}

#[tokio::test]
async fn finish_double_submit_on_held_row_is_rejected() {
    // The single-use `state` invariant: a second finish on an already-held
    // row must NOT mint a fresh token and overwrite the first.
    let state = state_with().await;
    let state_plain = "dbl".to_string();
    seed_state(&state, &state_plain, "github", ACTION_SIGNIN).await;
    let first = finish_needs_username(state.clone(), &state_plain, "subj-dbl").await;

    let err = finish(
        State(state.clone()),
        Path("github".to_string()),
        Json(FinishBody {
            state: state_plain,
            code: "subj-dbl".to_string(),
        }),
    )
    .await
    .expect_err("held row not redeemable");
    assert_eq!(err.status, 401);
    // The first token is still the one on the row (not rotated).
    let row = oauth_states::Entity::find()
        .filter(oauth_states::Column::Subject.eq("subj-dbl"))
        .one(&state.db)
        .await
        .expect("read")
        .expect("held row");
    assert_eq!(
        row.signup_token_hash,
        Some(TokenHash::of(&first).as_bytes().to_vec())
    );
}

#[tokio::test]
async fn signup_complete_creates_account() {
    let state = state_with().await;
    let state_plain = "su".to_string();
    seed_state(&state, &state_plain, "github", ACTION_SIGNIN).await;
    let token = finish_needs_username(state.clone(), &state_plain, "su-subj").await;

    let resp = complete(state.clone(), token, "chosen-name")
        .await
        .expect("complete ok");
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Account created with the user-chosen username; identity bound; row gone.
    let user = users::Entity::find()
        .filter(users::Column::Username.eq("chosen-name"))
        .one(&state.db)
        .await
        .expect("find user")
        .expect("user created");
    // The held claim_display_name lands on the identity row (display-only).
    let identity = external_identities::Entity::find()
        .filter(external_identities::Column::Subject.eq("su-subj"))
        .filter(external_identities::Column::UserId.eq(user.id.clone()))
        .one(&state.db)
        .await
        .expect("find identity")
        .expect("identity created");
    assert_eq!(identity.display_name.as_deref(), Some("Ghost"));
    // The user row's display_name stays None (no display_name at signup).
    assert!(user.display_name.is_none());
    assert_eq!(
        crate::db::entity::sessions::Entity::find()
            .filter(crate::db::entity::sessions::Column::UserId.eq(user.id))
            .count(&state.db)
            .await
            .expect("sessions"),
        1
    );
    assert_eq!(
        oauth_states::Entity::find()
            .count(&state.db)
            .await
            .expect("states"),
        0
    );
}

#[tokio::test]
async fn signup_complete_duplicate_username_is_409() {
    let state = state_with().await;
    seed_user(&state, "taken").await;
    let state_plain = "dup".to_string();
    seed_state(&state, &state_plain, "github", ACTION_SIGNIN).await;
    let token = finish_needs_username(state.clone(), &state_plain, "dup-subj").await;

    let err = complete(state.clone(), token, "taken")
        .await
        .expect_err("duplicate username");
    assert_eq!(err.status, 409);
    assert_eq!(err.message, "username already taken");
    // The held row survives so the user can retry with another username.
    assert_eq!(
        oauth_states::Entity::find()
            .count(&state.db)
            .await
            .expect("states"),
        1
    );
}

#[tokio::test]
async fn signup_complete_token_reuse_is_rejected() {
    let state = state_with().await;
    let state_plain = "reuse".to_string();
    seed_state(&state, &state_plain, "github", ACTION_SIGNIN).await;
    let token = finish_needs_username(state.clone(), &state_plain, "reuse-subj").await;

    let first = complete(state.clone(), token.clone(), "ok-one").await;
    assert!(first.is_ok());
    // The token is single-use: a replay is rejected, and cannot create a
    // second account.
    let err = complete(state.clone(), token, "ok-two")
        .await
        .expect_err("reuse");
    assert_eq!(err.status, 401);
    assert_eq!(
        users::Entity::find().count(&state.db).await.expect("users"),
        1,
        "reuse must not create a second account"
    );
}

#[tokio::test]
async fn signup_complete_race_identity_linked_signs_in() {
    // Between finish (needs-username) and complete, the identity gets linked
    // (another flow won). complete must sign the user in, not 409/duplicate.
    let state = state_with().await;
    let owner = seed_user(&state, "racer-owner").await;
    let state_plain = "race".to_string();
    seed_state(&state, &state_plain, "github", ACTION_SIGNIN).await;
    let token = finish_needs_username(state.clone(), &state_plain, "race-subj").await;

    // Simulate the race: the subject is now linked to an existing account.
    seed_identity(&state, &owner, "race-subj").await;

    let resp = complete(state.clone(), token, "would-be")
        .await
        .expect("sign-in ok");
    assert_eq!(resp.status(), StatusCode::OK);
    // No second account was created.
    assert_eq!(
        users::Entity::find().count(&state.db).await.expect("users"),
        1
    );
}

#[tokio::test]
async fn signup_complete_rejects_when_signup_disabled() {
    // signup_enabled is enforced at complete (the create step), not at finish.
    let state = make_state_with_oauth_signup_disabled(vec![Box::new(MockGithub)]).await;
    let state_plain = "off".to_string();
    seed_state(&state, &state_plain, "github", ACTION_SIGNIN).await;
    // finish still resolves the claim and returns needs-username...
    let token = finish_needs_username(state.clone(), &state_plain, "off-subj").await;
    // ...but completing the account is refused.
    let err = complete(state.clone(), token, "someone")
        .await
        .expect_err("disabled");
    assert_eq!(err.status, 403);
    // The held row survives the rejection (the 403 rolls back before the
    // delete) -- though a disabled-signup row is a dead end regardless.
    assert_eq!(
        oauth_states::Entity::find()
            .count(&state.db)
            .await
            .expect("states"),
        1
    );
}

#[tokio::test]
async fn finish_rejects_unknown_state() {
    let state = state_with().await;
    let err = finish(
        State(state),
        Path("github".to_string()),
        Json(FinishBody {
            state: "never-issued".to_string(),
            code: "x".to_string(),
        }),
    )
    .await
    .expect_err("unknown state");
    assert_eq!(err.status, 401);
}

#[tokio::test]
async fn finish_rejects_provider_mismatch() {
    // A state issued for "google" cannot redeem at the github finish path.
    let state = state_with().await;
    let state_plain = "mismatch".to_string();
    seed_state(&state, &state_plain, "google", ACTION_SIGNIN).await;
    let err = finish(
        State(state),
        Path("github".to_string()),
        Json(FinishBody {
            state: state_plain,
            code: "x".to_string(),
        }),
    )
    .await
    .expect_err("provider mismatch");
    assert_eq!(err.status, 401);
}

#[tokio::test]
async fn unlink_last_identity_blocked_without_a_passkey() {
    let state = state_with().await;
    let user_id = seed_user(&state, "solo").await;
    seed_identity(&state, &user_id, "123").await;
    // No passkeys; one identity -> unlinking it would leave the account
    // credentialless.
    let rows = external_identities::Entity::find()
        .filter(external_identities::Column::UserId.eq(user_id.to_string()))
        .all(&state.db)
        .await
        .expect("list");
    let id = rows[0].id;
    let err = unlink_external_identity(
        State(state),
        AuthPrincipal(crate::auth::Principal::User(user_id)),
        Path(id),
    )
    .await
    .expect_err("last method");
    assert_eq!(err.status, 409);
}

#[tokio::test]
async fn concurrent_last_pair_revoke_and_unlink_keeps_one_credential() {
    // The symmetric last-method guard's headline property: two concurrent
    // removals of the last passkey + last identity cannot both succeed
    // (leaving the account credentialless). The fixed lock order
    // (passkeys -> external_identities) shared by `revoke_passkey_row` and
    // `unlink_external_identity` is what makes this safe; this test is its
    // proof. A scheduling-dependent race would surface as both ops succeeding
    // or zero credentials remaining. One shared DB, a fresh user per
    // iteration so the runs do not interfere.
    let state = state_with().await;
    for i in 0..20u32 {
        let user_id = seed_user(&state, &format!("racer{i}")).await;
        // cred_id and (provider, subject) are both GLOBALLY unique, so each
        // iteration needs distinct values (one shared DB across the loop).
        let passkey_id = seed_passkey(&state, &user_id, i.to_le_bytes().to_vec()).await;
        seed_identity(&state, &user_id, &format!("subj-{i}")).await;
        let identity_id = external_identities::Entity::find()
            .filter(external_identities::Column::UserId.eq(user_id.to_string()))
            .one(&state.db)
            .await
            .expect("find identity")
            .map(|m| m.id)
            .expect("seeded identity");

        let revoke_state = state.clone();
        let revoke_user = user_id.clone();
        let unlink_state = state.clone();
        let unlink_user = user_id.clone();
        let (revoke_res, unlink_res) = tokio::join!(
            async {
                revoke_passkey_row(
                    &revoke_state,
                    &revoke_user,
                    passkey_id,
                    false,
                    "test",
                    "test",
                )
                .await
            },
            async {
                unlink_external_identity(
                    State(unlink_state),
                    AuthPrincipal(crate::auth::Principal::User(unlink_user)),
                    Path(identity_id),
                )
                .await
            }
        );

        // Exactly one succeeds; the other hits the 409 last-method guard.
        let revoke_ok = revoke_res.is_ok();
        let unlink_ok = unlink_res.is_ok();
        assert!(
            revoke_ok ^ unlink_ok,
            "iter {i}: exactly one of revoke/unlink must succeed; \
                 revoke_ok={revoke_ok} unlink_ok={unlink_ok}"
        );
        if !revoke_ok {
            assert_eq!(revoke_res.unwrap_err().status, 409);
        }
        if !unlink_ok {
            assert_eq!(unlink_res.unwrap_err().status, 409);
        }

        // Exactly one credential survives.
        let passkeys_left = crate::db::entity::passkeys::Entity::find()
            .filter(crate::db::entity::passkeys::Column::UserId.eq(user_id.to_string()))
            .count(&state.db)
            .await
            .expect("count passkeys");
        let identities_left = external_identities::Entity::find()
            .filter(external_identities::Column::UserId.eq(user_id.to_string()))
            .count(&state.db)
            .await
            .expect("count identities");
        assert_eq!(
            passkeys_left + identities_left,
            1,
            "iter {i}: exactly one credential must remain; \
                 passkeys={passkeys_left} identities={identities_left}"
        );
    }
}

#[tokio::test]
async fn concurrent_finish_link_for_same_subject_one_wins_one_409() {
    // Two signed-in users concurrently link the SAME novel (provider,
    // subject). SELECT FOR UPDATE gap-locks nothing on a non-existent row,
    // so both reach the insert; the loser's unique-violation must surface as
    // a clean 409 (not a generic 500 that leaks the constraint name). The
    // property under test is "exactly one winner"; the loser's 409 may come
    // from the new constraint-violation arm (genuine race) OR the
    // pre-existing `existing.user_id != target` arm (serialized) -- both are
    // correct. The loop makes the race arm overwhelmingly likely to fire.
    // One shared state: the subject is carried by `code` (varied per
    // iteration), and (provider, subject) is globally unique, so the subject
    // is novel each iteration without a fresh DB.
    let state = state_with().await;
    for i in 0..20u32 {
        let subject = format!("link-race-{i}");
        let user_a = seed_user(&state, &format!("linka{i}")).await;
        let user_b = seed_user(&state, &format!("linkb{i}")).await;
        let state_a = format!("la-{i}");
        let state_b = format!("lb-{i}");
        seed_link_state(&state, &state_a, "github", &user_a).await;
        seed_link_state(&state, &state_b, "github", &user_b).await;

        let sa = state.clone();
        let sb = state.clone();
        let (res_a, res_b) = tokio::join!(
            async {
                finish(
                    State(sa),
                    Path("github".to_string()),
                    Json(FinishBody {
                        state: state_a,
                        code: subject.clone(),
                    }),
                )
                .await
            },
            async {
                finish(
                    State(sb),
                    Path("github".to_string()),
                    Json(FinishBody {
                        state: state_b,
                        code: subject.clone(),
                    }),
                )
                .await
            }
        );

        // Exactly one succeeds (204); the other hits the 409 link guard.
        let a_ok = res_a.is_ok();
        let b_ok = res_b.is_ok();
        assert!(
            a_ok ^ b_ok,
            "iter {i}: exactly one link must succeed; a_ok={a_ok} b_ok={b_ok}"
        );
        if !a_ok {
            assert_eq!(res_a.unwrap_err().status, 409);
        }
        if !b_ok {
            assert_eq!(res_b.unwrap_err().status, 409);
        }

        // Exactly one identity row for the raced subject.
        let rows = external_identities::Entity::find()
            .filter(external_identities::Column::Subject.eq(&subject))
            .count(&state.db)
            .await
            .expect("count identities");
        assert_eq!(rows, 1, "iter {i}: exactly one identity for the subject");
    }
}
