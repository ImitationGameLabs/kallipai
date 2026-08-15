//! The WebAuthn crypto ceremonies are exercised by the browser; these tests
//! cover the pairing-code lifecycle + the helpers' edge cases that do not
//! need a virtual authenticator. The finish handler's pre-crypto guards
//! (unknown / wrong-kind / expired ceremony) all fire before
//! `finish_passkey_registration`, so they are testable here; the
//! consume-race and denylist live in extracted helpers exercised directly.
use super::*;
use crate::db::entity::sessions;
use crate::test_helpers::{make_state, seed_user};
use crate::token::SESSION;
use kallip_common::authtoken::MintedToken;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait, TransactionTrait};

/// Seed a pairing code (`code`) for `user_id` and return its hash.
/// `lifetime` lets a caller mint an already-expired row.
async fn seed_code(
    state: &SharedState,
    user_id: &UserId,
    code: &str,
    lifetime: Duration,
) -> Vec<u8> {
    let hash = code::hash_of(&code::canonicalize(code)).as_bytes().to_vec();
    let now = OffsetDateTime::now_utc();
    device_pairing_codes::ActiveModel {
        code_hash: Set(hash.clone()),
        user_id: Set(user_id.to_string()),
        created_at: Set(now),
        expires_at: Set(now + lifetime),
        consumed_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("insert code");
    hash
}

/// A live pairing code validates; a consumed / unknown one does not (both
/// uniformly 401 — the message leaks nothing about which).
#[tokio::test]
async fn begin_validates_the_pairing_code_uniformly() {
    let state = make_state().await;
    let user_id = seed_user(&state, "alice").await;
    seed_code(&state, &user_id, "ABCD-EFGH", PAIR_CODE_TTL).await;

    // Unknown code -> 401 invalid pairing code.
    let err = pair_begin(
        State(state.clone()),
        Json(PairBeginRequest {
            code: "ZZZZ-ZZZZ".to_string(),
        }),
    )
    .await
    .expect_err("unknown code rejected");
    assert_eq!(err.status, 401);

    // Consumed code -> same 401 (uniform, no distinct "consumed" leak).
    device_pairing_codes::Entity::update_many()
        .filter(device_pairing_codes::Column::UserId.eq(user_id.to_string()))
        .col_expr(
            device_pairing_codes::Column::ConsumedAt,
            sea_orm::sea_query::Expr::value(OffsetDateTime::now_utc()),
        )
        .exec(&state.db)
        .await
        .expect("consume");
    let err = pair_begin(
        State(state),
        Json(PairBeginRequest {
            code: "ABCD-EFGH".to_string(),
        }),
    )
    .await
    .expect_err("consumed code rejected");
    assert_eq!(err.status, 401);
}

/// A disabled account is screened at begin with a distinct 403 (separable
/// from the uniform 401 code message).
#[tokio::test]
async fn begin_screens_disabled_account() {
    let state = make_state().await;
    let user_id = seed_user(&state, "frozen").await;
    let mut am: users::ActiveModel = users::Entity::find_by_id(user_id.to_string())
        .one(&state.db)
        .await
        .unwrap()
        .unwrap()
        .into();
    am.disabled_at = Set(Some(OffsetDateTime::now_utc()));
    am.update(&state.db).await.unwrap();
    seed_code(&state, &user_id, "ABCD-EFGH", PAIR_CODE_TTL).await;

    let err = pair_begin(
        State(state),
        Json(PairBeginRequest {
            code: "ABCD-EFGH".to_string(),
        }),
    )
    .await
    .expect_err("disabled must be screened");
    assert_eq!(err.status, 403);
}

/// Finish rejects an unknown ceremony id with 404 (before any crypto).
#[tokio::test]
async fn finish_rejects_unknown_ceremony() {
    let state = make_state().await;
    let err = pair_finish(
        State(state),
        Json(PairFinishRequest {
            ceremony_id: Uuid::new_v4(),
            credential: phantom_credential(),
            label: "Phone".to_string(),
        }),
    )
    .await
    .expect_err("unknown ceremony");
    assert_eq!(err.status, 404);
}

/// Finish rejects a ceremony whose kind is not `KIND_PAIR` with 400 — a
/// register/login challenge cannot be spent via the pairing surface. Fires
/// before the credential is verified.
#[tokio::test]
async fn finish_rejects_wrong_ceremony_kind() {
    let state = make_state().await;
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    webauthn_challenges::ActiveModel {
        id: Set(id),
        kind: Set(crate::routes::auth::KIND_REGISTER.to_string()),
        state: Set(serde_json::json!({})),
        pairing_code_hash: Set(None),
        user_id: Set(None),
        username: Set(None),
        expires_at: Set(now + CHALLENGE_TTL),
        created_at: Set(now),
    }
    .insert(&state.db)
    .await
    .unwrap();

    let err = pair_finish(
        State(state),
        Json(PairFinishRequest {
            ceremony_id: id,
            credential: phantom_credential(),
            label: "Phone".to_string(),
        }),
    )
    .await
    .expect_err("wrong kind rejected");
    assert_eq!(err.status, 400);
}

/// Finish rejects an expired (but otherwise valid-kind) ceremony with 401,
/// before the credential is verified.
#[tokio::test]
async fn finish_rejects_expired_ceremony() {
    let state = make_state().await;
    let user_id = seed_user(&state, "alice").await;
    let id = Uuid::new_v4();
    let past = OffsetDateTime::now_utc() - Duration::from_secs(60);
    webauthn_challenges::ActiveModel {
        id: Set(id),
        kind: Set(KIND_PAIR.to_string()),
        state: Set(serde_json::json!({})),
        pairing_code_hash: Set(Some(vec![0u8; 32])),
        user_id: Set(Some(user_id.to_string())),
        username: Set(None),
        expires_at: Set(past),
        created_at: Set(past),
    }
    .insert(&state.db)
    .await
    .unwrap();

    let err = pair_finish(
        State(state),
        Json(PairFinishRequest {
            ceremony_id: id,
            credential: phantom_credential(),
            label: "Phone".to_string(),
        }),
    )
    .await
    .expect_err("expired rejected");
    assert_eq!(err.status, 401);
}

/// `consume_pairing_code` is the anti-double-enroll mutex: the first call
/// wins, a second call on the same code loses (returns false), and an
/// already-expired code is never consumable.
#[tokio::test]
async fn consume_is_single_use_and_expiry_aware() {
    let state = make_state().await;
    let user_id = seed_user(&state, "alice").await;

    // Live code: first consume true, second false.
    let live = seed_code(&state, &user_id, "ABCD-EFGH", PAIR_CODE_TTL).await;
    let now = OffsetDateTime::now_utc();
    let first = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let live = live.clone();
            Box::pin(async move { consume_pairing_code(txn, &live, now).await })
        })
        .await
        .expect("txn1");
    assert!(first, "first finisher consumes the code");
    let second = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let live = live.clone();
            Box::pin(async move { consume_pairing_code(txn, &live, now).await })
        })
        .await
        .expect("txn2");
    assert!(!second, "a racing finisher must not re-consume");

    // Expired code: never consumable. Capture `later` AFTER seeding so it
    // is past the row's expires_at (seeded at ~the same instant).
    let expired = seed_code(&state, &user_id, "WXYZ-WXYZ", Duration::ZERO).await;
    let later = OffsetDateTime::now_utc();
    let expired_ok = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let expired = expired.clone();
            Box::pin(async move { consume_pairing_code(txn, &expired, later).await })
        })
        .await
        .expect("txn3");
    assert!(!expired_ok, "an expired code cannot be consumed");
}

/// `bind_passkey_to_user` refuses to re-bind a revoked cred_id (denylist)
/// and otherwise inserts a live passkey row. Shared with add-passkey finish.
#[tokio::test]
async fn bind_respects_the_revocation_denylist() {
    use crate::db::entity::passkey_revocations;
    let state = make_state().await;
    let user_id = seed_user(&state, "alice").await;
    let cred_id = vec![42u8];

    // Seed a revocation audit row for this cred_id (denylist).
    let now = OffsetDateTime::now_utc();
    passkey_revocations::ActiveModel {
        id: Set(Uuid::new_v4()),
        user_id: Set(user_id.to_string()),
        cred_id: Set(cred_id.clone()),
        reason: Set(passkey_revocations::REASON_REVOKED.to_string()),
        revoked_by: Set(passkey_revocations::REVOKED_BY_USER.to_string()),
        revoked_at: Set(now),
    }
    .insert(&state.db)
    .await
    .unwrap();

    // Re-binding the revoked cred_id fails with a conflict.
    let err = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let user_id = user_id.clone();
            let cred_id = cred_id.clone();
            Box::pin(async move {
                bind_passkey_to_user(
                    txn,
                    &user_id,
                    cred_id,
                    serde_json::json!({}),
                    "Phone".to_string(),
                    CredentialFlavor::Regular,
                )
                .await
            })
        })
        .await
        .expect_err("denied cred_id must not re-bind");
    match err {
        sea_orm::TransactionError::Transaction(TxnError::Api(e)) => {
            assert_eq!(e.status, 409);
        }
        other => panic!("expected Api conflict, got {other:?}"),
    }

    // A fresh cred_id binds cleanly.
    state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let user_id = user_id.clone();
            Box::pin(async move {
                bind_passkey_to_user(
                    txn,
                    &user_id,
                    vec![7u8],
                    serde_json::json!({}),
                    "Laptop".to_string(),
                    CredentialFlavor::Regular,
                )
                .await
            })
        })
        .await
        .expect("fresh cred_id binds");
    let n = passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .count(&state.db)
        .await
        .unwrap();
    assert_eq!(n, 1, "exactly one live passkey inserted");
}

/// Seed a session row carrying a fresh `authed_at` (a valid step-up) for
/// `user_id`, and return headers presenting its cookie. Mirrors what a real
/// login/register finish writes, so `mint_pairing_code` can run end-to-end.
async fn step_up_headers(state: &SharedState, user_id: &UserId) -> HeaderMap {
    let session = MintedToken::generate(SESSION);
    let now = OffsetDateTime::now_utc();
    sessions::ActiveModel {
        token_hash: Set(session.hash().as_bytes().to_vec()),
        user_id: Set(user_id.to_string()),
        created_at: Set(now),
        expires_at: Set(now + time::Duration::seconds(3600)),
        authed_at: Set(Some(now)),
    }
    .insert(&state.db)
    .await
    .expect("insert session");
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        format!(
            "{}={}",
            crate::session::SESSION_COOKIE_NAME,
            session.secret()
        )
        .parse()
        .expect("cookie header"),
    );
    headers
}

/// `mint_pairing_code` refuses to mint past `MAX_ACTIVE_PAIR_CODES` live
/// codes per user (storage + abuse bound). Three codes are pre-seeded, so
/// the next mint hits the cap with 429 — even though the step-up is valid
/// (the capped txn rolls back, so `authed_at` is NOT consumed).
#[tokio::test]
async fn mint_caps_active_codes_per_user() {
    use crate::auth::Principal;
    let state = make_state().await;
    let user_id = seed_user(&state, "alice").await;
    seed_code(&state, &user_id, "AAAA-AAAA", PAIR_CODE_TTL).await;
    seed_code(&state, &user_id, "BBBB-BBBB", PAIR_CODE_TTL).await;
    seed_code(&state, &user_id, "CCCC-CCCC", PAIR_CODE_TTL).await;

    let err = mint_pairing_code(
        State(state.clone()),
        AuthPrincipal(Principal::User(user_id.clone())),
        step_up_headers(&state, &user_id).await,
    )
    .await
    .expect_err("4th mint must hit the cap");
    assert_eq!(err.status, 429);
}

/// `pair_begin` refuses a 4th in-flight ceremony against one code (storage
/// bound on a code-holder spamming begins). Soft under true concurrency, as
/// the doc states; here the ceremonies are seeded serially.
#[tokio::test]
async fn begin_caps_inflight_ceremonies_per_code() {
    let state = make_state().await;
    let user_id = seed_user(&state, "alice").await;
    let hash = seed_code(&state, &user_id, "ABCD-EFGH", PAIR_CODE_TTL).await;
    let now = OffsetDateTime::now_utc();
    for _ in 0..MAX_INFLIGHT_PAIR_CEREMONIES {
        webauthn_challenges::ActiveModel {
            id: Set(Uuid::new_v4()),
            kind: Set(KIND_PAIR.to_string()),
            state: Set(serde_json::json!({})),
            pairing_code_hash: Set(Some(hash.clone())),
            user_id: Set(Some(user_id.to_string())),
            username: Set(None),
            expires_at: Set(now + CHALLENGE_TTL),
            created_at: Set(now),
        }
        .insert(&state.db)
        .await
        .unwrap();
    }

    let err = pair_begin(
        State(state),
        Json(PairBeginRequest {
            code: "ABCD-EFGH".to_string(),
        }),
    )
    .await
    .expect_err("4th begin must hit the cap");
    assert_eq!(err.status, 429);
}

/// A placeholder credential — its contents never matter because every
/// finish test here hits a pre-crypto guard.
fn phantom_credential() -> RegisterPublicKeyCredential {
    serde_json::from_str(
            r#"{"id":"","rawId":"","type":"public-key","response":{"attestationObject":"","clientDataJSON":""}}"#,
        )
        .expect("phantom credential deserializes")
}
