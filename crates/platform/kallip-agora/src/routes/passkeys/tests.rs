//! Row-level helper tests for the live/revoked split + last-passkey guard.
//! The WebAuthn crypto ceremonies themselves are exercised by the browser.

use super::*;
use crate::auth::Principal;
use crate::test_helpers::{make_state, seed_passkey, seed_user};
use sea_orm::EntityTrait;

/// Revoking a passkey hard-deletes it from `passkeys` and appends a
/// `passkey_revocations` audit row.
#[tokio::test]
async fn revoke_deletes_live_row_and_appends_audit() {
    let state = make_state().await;
    let user_id = seed_user(&state, "alice").await;
    let a = seed_passkey(&state, &user_id, vec![1]).await;
    let b = seed_passkey(&state, &user_id, vec![2]).await;

    revoke_passkey_row(
        &state,
        &user_id,
        a,
        false,
        passkey_revocations::REASON_REVOKED,
        passkey_revocations::REVOKED_BY_USER,
    )
    .await
    .expect("revoke a");

    // The live set no longer contains `a`; `b` remains.
    let live = list_user_passkey_rows(&state.db, &user_id)
        .await
        .expect("list");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, b);

    // The audit row carries cred_id + reason + revoked_by.
    let audits = passkey_revocations::Entity::find()
        .filter(passkey_revocations::Column::UserId.eq(user_id.to_string()))
        .all(&state.db)
        .await
        .expect("audit list");
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].cred_id, vec![1]);
    assert_eq!(audits[0].reason, passkey_revocations::REASON_REVOKED);
    assert_eq!(audits[0].revoked_by, passkey_revocations::REVOKED_BY_USER);
}

/// The user path (`allow_last = false`) refuses to revoke the last live
/// passkey, preventing self-lockout.
#[tokio::test]
async fn revoke_rejects_last_live_passkey() {
    let state = make_state().await;
    let user_id = seed_user(&state, "bob").await;
    let only = seed_passkey(&state, &user_id, vec![9]).await;
    let err = revoke_passkey_row(
        &state,
        &user_id,
        only,
        false,
        passkey_revocations::REASON_REVOKED,
        passkey_revocations::REVOKED_BY_USER,
    )
    .await
    .expect_err("last passkey must not be revokable");
    assert_eq!(err.status, 409);
    // The row survived.
    assert_eq!(
        list_user_passkey_rows(&state.db, &user_id)
            .await
            .expect("list")
            .len(),
        1
    );
}

/// The user path (`allow_last = false`) ALLOWS revoking the last passkey
/// when the account still has an external identity to sign in with (the
/// symmetric last-method guard: at least one credential of either kind).
#[tokio::test]
async fn revoke_last_passkey_allowed_with_external_identity() {
    let state = make_state().await;
    let user_id = seed_user(&state, "dory").await;
    let only = seed_passkey(&state, &user_id, vec![3]).await;
    // The account also has a linked GitHub identity.
    external_identities::ActiveModel {
        id: Set(Uuid::new_v4()),
        user_id: Set(user_id.to_string()),
        provider: Set("github".to_string()),
        subject: Set("gh-1".to_string()),
        display_name: Set(None),
        created_at: Set(OffsetDateTime::now_utc()),
        last_used_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("seed identity");

    revoke_passkey_row(
        &state,
        &user_id,
        only,
        false,
        passkey_revocations::REASON_REVOKED,
        passkey_revocations::REVOKED_BY_USER,
    )
    .await
    .expect("identity keeps the account reachable");
    assert!(
        list_user_passkey_rows(&state.db, &user_id)
            .await
            .expect("list")
            .is_empty()
    );
}

/// The admin path (`allow_last = true`) overrides the guard.
#[tokio::test]
async fn revoke_allow_last_overrides_guard() {
    let state = make_state().await;
    let user_id = seed_user(&state, "carol").await;
    let only = seed_passkey(&state, &user_id, vec![7]).await;
    revoke_passkey_row(
        &state,
        &user_id,
        only,
        true,
        passkey_revocations::REASON_REVOKED,
        passkey_revocations::REVOKED_BY_ADMIN,
    )
    .await
    .expect("admin override");
    assert!(
        list_user_passkey_rows(&state.db, &user_id)
            .await
            .expect("list")
            .is_empty()
    );
}

/// Revoking an already-revoked (absent) id is an idempotent no-op.
#[tokio::test]
async fn revoke_is_idempotent() {
    let state = make_state().await;
    let user_id = seed_user(&state, "dave").await;
    let pk = seed_passkey(&state, &user_id, vec![1]).await;
    seed_passkey(&state, &user_id, vec![2]).await;
    revoke_passkey_row(
        &state,
        &user_id,
        pk,
        false,
        passkey_revocations::REASON_REVOKED,
        passkey_revocations::REVOKED_BY_USER,
    )
    .await
    .expect("first");
    // Second revoke of the same id: no-op (no 409, no duplicate audit row).
    revoke_passkey_row(
        &state,
        &user_id,
        pk,
        false,
        passkey_revocations::REASON_REVOKED,
        passkey_revocations::REVOKED_BY_USER,
    )
    .await
    .expect("second is no-op");
    let audits = passkey_revocations::Entity::find()
        .filter(passkey_revocations::Column::UserId.eq(user_id.to_string()))
        .count(&state.db)
        .await
        .expect("count");
    assert_eq!(audits, 1);
}

// --- discoverable add-passkey begin: builder + step-up + cap ----------------

/// Seed a fresh step-up session for `user_id` and return the headers that
/// carry the matching `kallip_session` cookie. Mirrors the production
/// login-finish row: `token_hash = TokenHash::of(cookie value)` and
/// `authed_at` set to `now` (fresh within the step-up window).
async fn seed_step_up_session(state: &SharedState, user_id: &UserId) -> HeaderMap {
    let now = OffsetDateTime::now_utc();
    let secret = "sk-sess-test".to_string();
    let hash = TokenHash::of(&secret).as_bytes().to_vec();
    sessions::ActiveModel {
        token_hash: Set(hash),
        user_id: Set(user_id.to_string()),
        created_at: Set(now),
        expires_at: Set(now + time::Duration::seconds(3600)),
        authed_at: Set(Some(now)),
    }
    .insert(&state.db)
    .await
    .expect("seed step-up session");
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        format!("{}={secret}", crate::session::SESSION_COOKIE_NAME)
            .parse()
            .expect("cookie header value"),
    );
    headers
}

/// `?discoverable=true` builds the resident-key ceremony via the bare core
/// and persists the BARE `RegistrationState` (not `PasskeyRegistration`),
/// tagged `add_discoverable`, owner-scoped to the caller. This is the
/// highest-risk untested code path (the hand-mirrored builder); the crypto
/// assertion itself is browser-tested.
#[tokio::test]
async fn add_passkey_begin_discoverable_writes_bare_registration_state() {
    let state = make_state().await;
    let user_id = seed_user(&state, "erin").await;
    let headers = seed_step_up_session(&state, &user_id).await;

    let resp = add_passkey_begin(
        State(state.clone()),
        AuthPrincipal(Principal::User(user_id.clone())),
        headers,
        Query(AddPasskeyBeginParams {
            discoverable: Some(true),
        }),
    )
    .await
    .expect("discoverable begin ok");
    assert!(!resp.ceremony_id.is_empty());

    let rows = webauthn_challenges::Entity::find()
        .all(&state.db)
        .await
        .expect("read challenges");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, KIND_ADD_DISCOVERABLE);
    assert_eq!(rows[0].user_id, Some(user_id.to_string()));
    // The state is the bare core RegistrationState (a different shape from
    // the username-registration PasskeyRegistration): rehydration is what
    // finish branches on.
    let _: RegistrationState =
        serde_json::from_value(rows[0].state.clone()).expect("deserialize bare reg state");

    // The step-up was consumed (one fresh login authorizes exactly one bind).
    let still_authed = sessions::Entity::find()
        .filter(sessions::Column::UserId.eq(user_id.to_string()))
        .one(&state.db)
        .await
        .expect("read session")
        .expect("session exists");
    assert!(still_authed.authed_at.is_none(), "step-up consumed");
}

/// The in-flight add-passkey cap is a SHARED budget across both add kinds:
/// `register` ceremonies already open for the user count against a
/// `?discoverable=true` begin, so alternating kinds cannot bypass the cap.
#[tokio::test]
async fn add_passkey_begin_cap_counts_both_add_kinds() {
    let state = make_state().await;
    let user_id = seed_user(&state, "frank").await;
    let now = OffsetDateTime::now_utc();
    // Saturate the cap with regular (non-discoverable) add ceremonies.
    for _ in 0..MAX_INFLIGHT_ADD_CEREMONIES {
        webauthn_challenges::ActiveModel {
            id: Set(Uuid::new_v4()),
            kind: Set(KIND_ADD_REGULAR.to_string()),
            state: Set(serde_json::Value::Null),
            pairing_code_hash: Set(None),
            user_id: Set(Some(user_id.to_string())),
            username: Set(None),
            expires_at: Set(now + time::Duration::seconds(60)),
            created_at: Set(now),
        }
        .insert(&state.db)
        .await
        .expect("seed register challenge");
    }
    let headers = seed_step_up_session(&state, &user_id).await;

    let err = add_passkey_begin(
        State(state),
        AuthPrincipal(Principal::User(user_id)),
        headers,
        Query(AddPasskeyBeginParams {
            discoverable: Some(true),
        }),
    )
    .await
    .expect_err("shared cap must 429");
    assert_eq!(err.status, 429);
}

/// A placeholder registration credential -- its contents never matter
/// because these finish tests each hit a pre-crypto guard (unknown/kind/
/// expiry), before `register_credential` is reached.
fn phantom_credential() -> RegisterPublicKeyCredential {
    serde_json::from_str(
            r#"{"id":"","rawId":"","type":"public-key","response":{"attestationObject":"","clientDataJSON":""}}"#,
        )
        .expect("phantom credential deserializes")
}

/// `add_passkey_finish` rejects an unknown ceremony id with 404.
#[tokio::test]
async fn add_passkey_finish_rejects_unknown_ceremony() {
    let state = make_state().await;
    let err = add_passkey_finish(
        State(state),
        Json(AddPasskeyFinishRequest {
            ceremony_id: Uuid::new_v4(),
            credential: phantom_credential(),
            label: "Phone".to_string(),
        }),
    )
    .await
    .expect_err("unknown ceremony");
    assert_eq!(err.status, 404);
}

/// The finish kind-discriminator accepts only the add kinds
/// (`KIND_ADD_REGULAR` + `KIND_ADD_DISCOVERABLE`); a ceremony of any other
/// kind (e.g. a pairing ceremony) is rejected with 400. Locks the
/// `KIND_REGISTER`/`KIND_ADD_REGULAR` split so a non-add ceremony cannot
/// reach the rehydrate branch.
#[tokio::test]
async fn add_passkey_finish_rejects_wrong_kind() {
    let state = make_state().await;
    let user = seed_user(&state, "alice").await;
    let now = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    webauthn_challenges::ActiveModel {
        id: Set(id),
        kind: Set(crate::routes::auth::KIND_PAIR.to_string()),
        state: Set(serde_json::Value::Null),
        pairing_code_hash: Set(None),
        user_id: Set(Some(user.to_string())),
        username: Set(None),
        expires_at: Set(now + time::Duration::seconds(60)),
        created_at: Set(now),
    }
    .insert(&state.db)
    .await
    .expect("seed pair ceremony");
    let err = add_passkey_finish(
        State(state),
        Json(AddPasskeyFinishRequest {
            ceremony_id: id,
            credential: phantom_credential(),
            label: "Phone".to_string(),
        }),
    )
    .await
    .expect_err("wrong kind");
    assert_eq!(err.status, 400);
}

/// `add_passkey_finish` rejects an expired add ceremony with 401.
#[tokio::test]
async fn add_passkey_finish_rejects_expired() {
    let state = make_state().await;
    let user = seed_user(&state, "alice").await;
    let now = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    webauthn_challenges::ActiveModel {
        id: Set(id),
        kind: Set(KIND_ADD_DISCOVERABLE.to_string()),
        state: Set(serde_json::Value::Null),
        pairing_code_hash: Set(None),
        user_id: Set(Some(user.to_string())),
        username: Set(None),
        expires_at: Set(now - time::Duration::seconds(60)),
        created_at: Set(now - time::Duration::seconds(120)),
    }
    .insert(&state.db)
    .await
    .expect("seed expired add ceremony");
    let err = add_passkey_finish(
        State(state),
        Json(AddPasskeyFinishRequest {
            ceremony_id: id,
            credential: phantom_credential(),
            label: "Phone".to_string(),
        }),
    )
    .await
    .expect_err("expired");
    assert_eq!(err.status, 401);
}
