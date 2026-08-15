//! Handler-level tests for the auth glue that does NOT require a virtual
//! authenticator: ceremony begin, the in-flight cap, and ceremony GC. The
//! WebAuthn crypto ceremonies themselves are exercised end-to-end by the
//! browser (a unit-level virtual authenticator would have to re-implement
//! CTAP signing, which `webauthn-rs` does not ship).

use axum::Json;
use axum::extract::State;
use sea_orm::{ActiveModelTrait, ActiveValue::Set};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    DiscoverableAuthentication, KIND_LOGIN, KIND_LOGIN_DISCOVERABLE, LoginBeginRequest,
    LoginFinishRequest, PublicKeyCredential, RegisterBeginRequest, login_begin,
    login_discoverable_begin, login_discoverable_finish, register_begin,
};
use crate::db::entity::{users, webauthn_challenges};
use crate::test_helpers::{make_state, seed_user};
use sea_orm::EntityTrait;

/// `login_begin` rejects an unknown username with 401 (accepted enumeration
/// oracle for closed beta; see the handler doc comment).
#[tokio::test]
async fn login_begin_rejects_unknown_username() {
    let state = make_state().await;
    match login_begin(
        State(state),
        Json(LoginBeginRequest {
            username: "nobody".to_string(),
        }),
    )
    .await
    {
        Err(e) => assert_eq!(e.status, 401),
        Ok(_) => panic!("unknown username must be rejected"),
    }
}

/// A disabled account cannot start a login: same 401 as an unknown user,
/// so the response leaks no account state.
#[tokio::test]
async fn login_begin_rejects_disabled_user() {
    let state = make_state().await;
    let user_id = seed_user(&state, "frozen").await;
    // Flip the account to disabled.
    let mut am: users::ActiveModel = users::Entity::find_by_id(user_id.to_string())
        .one(&state.db)
        .await
        .expect("load user")
        .expect("user present")
        .into();
    am.disabled_at = Set(Some(OffsetDateTime::now_utc()));
    am.update(&state.db).await.expect("disable user");

    match login_begin(
        State(state),
        Json(LoginBeginRequest {
            username: "frozen".to_string(),
        }),
    )
    .await
    {
        Err(e) => assert_eq!(e.status, 401),
        Ok(_) => panic!("disabled user must be rejected"),
    }
}

/// `register_begin` refuses once the per-username in-flight ceremony cap is
/// reached, bounding `webauthn_challenges` growth against a begin flood.
#[tokio::test]
async fn register_begin_caps_ceremonies_per_username() {
    let state = make_state().await;
    let now = OffsetDateTime::now_utc();
    // Seed exactly the cap of live register ceremonies for this username.
    for _ in 0..super::MAX_INFLIGHT_CEREMONIES {
        webauthn_challenges::ActiveModel {
            id: Set(uuid::Uuid::new_v4()),
            kind: Set("register".to_string()),
            state: Set(serde_json::Value::Null),
            pairing_code_hash: Set(None),
            user_id: Set(None),
            username: Set(Some("newuser".to_string())),
            expires_at: Set(now + time::Duration::seconds(60)),
            created_at: Set(now),
        }
        .insert(&state.db)
        .await
        .expect("seed challenge");
    }
    // The next begin for the same username is rejected with 429.
    match register_begin(
        State(state),
        Json(RegisterBeginRequest {
            username: "newuser".to_string(),
            display_name: None,
        }),
    )
    .await
    {
        Err(e) => assert_eq!(e.status, 429),
        Ok(_) => panic!("cap reached must 429"),
    }
}

/// `register_begin` enrolls a DISCOVERABLE (resident-key) credential: the
/// persisted state rehydrates to the bare core `RegistrationState` (not the
/// wrapper `PasskeyRegistration`), so the discoverable login / conditional-UI
/// autofill path works for a signup-created account. The crypto itself is
/// browser-tested; this covers the begin state shape.
#[tokio::test]
async fn register_begin_writes_discoverable_reg_state() {
    let state = make_state().await;
    let resp = register_begin(
        State(state.clone()),
        Json(RegisterBeginRequest {
            username: "newuser".to_string(),
            display_name: None,
        }),
    )
    .await
    .expect("begin ok");
    assert!(!resp.ceremony_id.is_empty());

    let rows = webauthn_challenges::Entity::find()
        .all(&state.db)
        .await
        .expect("read challenges");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "register");
    assert_eq!(rows[0].username.as_deref(), Some("newuser"));
    // The pre-generated user_id rides the ceremony and becomes the WebAuthn
    // userHandle that discoverable login resolves the account by.
    assert!(
        rows[0].user_id.is_some(),
        "register ceremony must carry user_id"
    );
    // Bare core RegistrationState -- the shape finish rehydrates.
    let _: super::RegistrationState =
        serde_json::from_value(rows[0].state.clone()).expect("deserialize bare reg state");
}

/// `login_discoverable_begin` opens a usernameless ceremony: no user is
/// resolved at begin (`user_id` stays None), and the persisted state
/// rehydrates to `DiscoverableAuthentication`. The crypto assertion itself
/// is browser-tested (no virtual authenticator); this covers the begin shape.
#[tokio::test]
async fn login_discoverable_begin_writes_discoverable_row() {
    let state = make_state().await;
    let resp = login_discoverable_begin(State(state.clone()))
        .await
        .expect("begin ok");
    assert!(!resp.ceremony_id.is_empty());

    let rows = webauthn_challenges::Entity::find()
        .all(&state.db)
        .await
        .expect("read challenges");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "login_discoverable");
    assert!(rows[0].user_id.is_none());
    // The state deserializes to the discoverable auth state type (not the
    // username-login PasskeyAuthentication shape).
    let _: DiscoverableAuthentication =
        serde_json::from_value(rows[0].state.clone()).expect("deserialize discoverable state");
}

/// A placeholder assertion credential -- its contents never matter because
/// these finish tests each hit a pre-crypto guard (unknown/kind/expiry),
/// before the assertion is touched.
fn phantom_assertion() -> PublicKeyCredential {
    serde_json::from_str(
            r#"{"id":"","rawId":"","type":"public-key","response":{"authenticatorData":"","clientDataJSON":"","signature":""}}"#,
        )
        .expect("phantom assertion deserializes")
}

/// `login_discoverable_finish` rejects an unknown ceremony id with 404.
#[tokio::test]
async fn login_discoverable_finish_rejects_unknown_ceremony() {
    let state = make_state().await;
    let err = login_discoverable_finish(
        State(state),
        Json(LoginFinishRequest {
            ceremony_id: Uuid::new_v4(),
            credential: phantom_assertion(),
        }),
    )
    .await
    .expect_err("unknown ceremony");
    assert_eq!(err.status, 404);
}

/// `login_discoverable_finish` rejects a ceremony of the WRONG kind (e.g. a
/// username-login ceremony id) with 400 -- the kind discriminator routes a
/// row to the right finish handler.
#[tokio::test]
async fn login_discoverable_finish_rejects_wrong_kind() {
    let state = make_state().await;
    let now = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    webauthn_challenges::ActiveModel {
        id: Set(id),
        kind: Set(KIND_LOGIN.to_string()),
        state: Set(serde_json::Value::Null),
        pairing_code_hash: Set(None),
        user_id: Set(None),
        username: Set(None),
        expires_at: Set(now + time::Duration::seconds(60)),
        created_at: Set(now),
    }
    .insert(&state.db)
    .await
    .expect("seed login ceremony");
    let err = login_discoverable_finish(
        State(state),
        Json(LoginFinishRequest {
            ceremony_id: id,
            credential: phantom_assertion(),
        }),
    )
    .await
    .expect_err("wrong kind");
    assert_eq!(err.status, 400);
}

/// `login_discoverable_finish` rejects an expired ceremony with 401.
#[tokio::test]
async fn login_discoverable_finish_rejects_expired() {
    let state = make_state().await;
    let now = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    webauthn_challenges::ActiveModel {
        id: Set(id),
        kind: Set(KIND_LOGIN_DISCOVERABLE.to_string()),
        state: Set(serde_json::Value::Null),
        pairing_code_hash: Set(None),
        user_id: Set(None),
        username: Set(None),
        expires_at: Set(now - time::Duration::seconds(60)),
        created_at: Set(now - time::Duration::seconds(120)),
    }
    .insert(&state.db)
    .await
    .expect("seed expired discoverable ceremony");
    let err = login_discoverable_finish(
        State(state),
        Json(LoginFinishRequest {
            ceremony_id: id,
            credential: phantom_assertion(),
        }),
    )
    .await
    .expect_err("expired");
    assert_eq!(err.status, 401);
}
