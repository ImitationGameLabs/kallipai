//! Device pairing — enroll a passkey on a NEW device via a short-lived code.
//!
//! The authenticated "Add device" flow (`routes::passkeys`) only enrolls a
//! passkey on the CURRENT browser's authenticator. A brand-new device with no
//! session and no passkey cannot use it (passkey private keys are non-exportable;
//! the new device can't reach the session-gated surface). Pairing bridges that:
//!
//! - **Issue** (`POST /v1/me/device-pairing`, session-authed + step-up): mint a
//!   short-lived one-time code bound to the caller (see `PAIR_CODE_TTL`); the
//!   logged-in device shows it (typed `XXXX-XXXX` and/or a QR encoding the same
//!   code).
//! - **Redeem** (`POST /v1/auth/device-pairing/{begin,finish}`,
//!   unauthenticated): the new device submits the code → a WebAuthn
//!   registration ceremony that binds a LOCAL passkey to the EXISTING account,
//!   then mints a session. Device B is signed in with its own passkey.
//!
//! The code is 8 Crockford-base32 symbols (2^40); the begin endpoint is guarded
//! by BOTH the per-IP limiter and a single shared `pair_rate_limiter` bucket
//! (the real distributed brute-force bound). Identity binding is sound: the
//! `user_id` comes from the code row minted by the authenticated caller, rides
//! the challenge, and is the only input at finish — Device B cannot target
//! another account.

use crate::auth::{AuthPrincipal, require_user};
use crate::code;
use crate::db::entity::{device_pairing_codes, passkeys, sessions, users, webauthn_challenges};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::session::{build_set_cookie, read_session_cookie};
use crate::state::SharedState;
use crate::token::SESSION;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use kallip_agora_common::ids::UserId;
use kallip_common::authtoken::{MintedToken, TokenHash};
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, EntityTrait,
    PaginatorTrait, QueryFilter, QuerySelect, SqlErr, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;
use webauthn_rs::prelude::{
    CreationChallengeResponse, PasskeyRegistration, RegisterPublicKeyCredential,
};

use super::auth::{
    CHALLENGE_TTL, CeremonyBeginResponse, KIND_PAIR, register_err, set_cookie_response,
};
use super::passkeys::{
    CRED_ID_UNIQUE_CONSTRAINT, bind_passkey_to_user, consume_step_up, normalize_label,
};

/// How long a minted pairing code remains redeemable.
const PAIR_CODE_TTL: Duration = Duration::from_secs(180);

/// Max live (unconsumed, unexpired) pairing codes per user. The step-up's
/// session-row lock already serializes mints per session; this bounds
/// multi-session concurrency and table growth.
const MAX_ACTIVE_PAIR_CODES: u64 = 3;

/// Max live (unexpired) pair ceremonies per code (storage bound on a code-holder
/// spamming begins). Count-then-insert, soft under true concurrency.
const MAX_INFLIGHT_PAIR_CEREMONIES: u64 = 3;

/// The session-scoped issue surface (cookie-authed, CSRF-covered by the v1
/// layer, not rate-limited).
pub fn session_router() -> Router<SharedState> {
    Router::new().route("/me/device-pairing", post(mint_pairing_code))
}

/// The unauthenticated redeem BEGIN surface. Mounted in `routes.rs` under BOTH
/// the per-IP and the shared `pair_rate_limit` layers.
pub fn begin_router() -> Router<SharedState> {
    Router::new().route("/auth/device-pairing/begin", post(pair_begin))
}

/// The unauthenticated redeem FINISH surface. Not rate-limited (bounded by
/// begin's rate limit + the unguessable ceremony id); no CSRF (Device B has no
/// cookie).
pub fn finish_router() -> Router<SharedState> {
    Router::new().route("/auth/device-pairing/finish", post(pair_finish))
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
struct MintPairingCodeResponse {
    code: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Deserialize)]
struct PairBeginRequest {
    code: String,
}

#[derive(Deserialize)]
struct PairFinishRequest {
    ceremony_id: Uuid,
    credential: RegisterPublicKeyCredential,
    label: String,
}

#[derive(Serialize)]
struct AuthFinishResponse {
    user_id: String,
}

// ---------------------------------------------------------------------------
// POST /v1/me/device-pairing (issue)
// ---------------------------------------------------------------------------

async fn mint_pairing_code(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    headers: HeaderMap,
) -> Result<Json<MintPairingCodeResponse>, ApiError> {
    let user_id = require_user(&principal)?;

    // Resolve the calling session (its authed_at is the step-up marker).
    let cookie = read_session_cookie(&headers)
        .ok_or_else(|| ApiError::internal("session cookie missing on authenticated request"))?;
    let session_hash = TokenHash::of(&cookie).as_bytes().to_vec();

    // Mint the code. The plaintext is returned once; only its hash is stored.
    let code = code::generate();
    let code_hash = code::hash_of(&code::canonicalize(&code))
        .as_bytes()
        .to_vec();

    let now = OffsetDateTime::now_utc();
    let expires_at = now + PAIR_CODE_TTL;
    let user_id_for_txn = user_id.clone();
    let outcome = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let session_hash = session_hash.clone();
            let code_hash = code_hash.clone();
            let user_id = user_id_for_txn.clone();
            Box::pin(async move {
                // One-shot step-up (shared with add-passkey begin).
                consume_step_up(txn, &session_hash, now).await?;

                // Per-user active-code cap (storage bound).
                let active = device_pairing_codes::Entity::find()
                    .filter(device_pairing_codes::Column::UserId.eq(user_id.to_string()))
                    .filter(device_pairing_codes::Column::ConsumedAt.is_null())
                    .filter(device_pairing_codes::Column::ExpiresAt.gt(now))
                    .count(txn)
                    .await?;
                if active >= MAX_ACTIVE_PAIR_CODES {
                    return Err(TxnError::Api(ApiError::too_many_requests(
                        "too many active pairing codes",
                    )));
                }

                device_pairing_codes::ActiveModel {
                    code_hash: Set(code_hash),
                    user_id: Set(user_id.to_string()),
                    created_at: Set(now),
                    expires_at: Set(expires_at),
                    consumed_at: Set(None),
                }
                .insert(txn)
                .await?;
                Ok(())
            })
        })
        .await;
    flatten_txn(outcome)?;

    Ok(Json(MintPairingCodeResponse { code, expires_at }))
}

// ---------------------------------------------------------------------------
// POST /v1/auth/device-pairing/begin
// ---------------------------------------------------------------------------

async fn pair_begin(
    State(state): State<SharedState>,
    Json(req): Json<PairBeginRequest>,
) -> Result<Json<CeremonyBeginResponse<CreationChallengeResponse>>, ApiError> {
    let code_hash = code::hash_of(&code::canonicalize(&req.code))
        .as_bytes()
        .to_vec();
    let now = OffsetDateTime::now_utc();

    // Validate the code WITHOUT consuming: live + unconsumed + unexpired. A
    // uniform message for unknown/consumed/expired leaks nothing (mirrors
    // register_begin's invite screen).
    let code_row = device_pairing_codes::Entity::find()
        .filter(device_pairing_codes::Column::CodeHash.eq(code_hash.clone()))
        .filter(device_pairing_codes::Column::ConsumedAt.is_null())
        .filter(device_pairing_codes::Column::ExpiresAt.gt(now))
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    let Some(code_row) = code_row else {
        return Err(ApiError::unauthorized("invalid pairing code"));
    };
    let user_id = UserId::from(code_row.user_id);

    // Screen disabled accounts (distinct 403, mirrors add_passkey_begin).
    let user = users::Entity::find_by_id(user_id.to_string())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::unauthorized("invalid pairing code"))?;
    if user.disabled_at.is_some() {
        return Err(ApiError::forbidden("account disabled"));
    }

    // Build excludeCredentials from the user's existing live passkeys (the table
    // is live-only). A UX/authenticator hint; the real anti-duplicate guarantee
    // is `uniq_passkeys_cred_id`.
    let exclude: Vec<Vec<u8>> = passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .all(&state.db)
        .await
        .map_err(map_db_err)?
        .into_iter()
        .map(|p| p.cred_id)
        .collect();

    let user_uuid = Uuid::parse_str(user_id.as_ref())
        .map_err(|e| ApiError::internal(format_args!("user id not a uuid: {e}")))?;
    let display = match user.display_name.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => user.username.clone(),
    };
    let (options, reg_state) = state
        .webauthn
        .start_passkey_registration(user_uuid, &user.email, &display, Some(exclude))
        .map_err(register_err)?;
    let state_value = serde_json::to_value(&reg_state)
        .map_err(|e| ApiError::internal(format_args!("serialize reg state: {e}")))?;

    // Per-code in-flight cap (storage bound on a code-holder spamming begins).
    let in_flight = webauthn_challenges::Entity::find()
        .filter(webauthn_challenges::Column::HeldCodeHash.eq(code_hash.clone()))
        .filter(webauthn_challenges::Column::Kind.eq(KIND_PAIR))
        .filter(webauthn_challenges::Column::ExpiresAt.gt(now))
        .count(&state.db)
        .await
        .map_err(map_db_err)?;
    if in_flight >= MAX_INFLIGHT_PAIR_CEREMONIES {
        return Err(ApiError::too_many_requests(
            "too many in-flight pair ceremonies",
        ));
    }

    let ceremony_id = Uuid::new_v4();
    webauthn_challenges::ActiveModel {
        id: Set(ceremony_id),
        kind: Set(KIND_PAIR.to_string()),
        state: Set(state_value),
        held_code_hash: Set(Some(code_hash)),
        user_id: Set(Some(user_id.to_string())),
        email: Set(None),
        username: Set(None),
        expires_at: Set(now + CHALLENGE_TTL),
        created_at: Set(now),
    }
    .insert(&state.db)
    .await
    .map_err(map_db_err)?;

    Ok(Json(CeremonyBeginResponse {
        ceremony_id: ceremony_id.to_string(),
        options,
    }))
}

/// Atomically consume a pairing code. A conditional `UPDATE` requiring the row
/// still be unconsumed AND unexpired: the first finisher wins (returns `true`),
/// any racing finish — of this or a distinct ceremony sharing the code — loses
/// (returns `false`). This is the anti-double-enroll mutex; the ceremony-id row
/// lock only guards replay of one ceremony id.
async fn consume_pairing_code(
    txn: &DatabaseTransaction,
    code_hash: &[u8],
    now: OffsetDateTime,
) -> Result<bool, TxnError> {
    let res = device_pairing_codes::Entity::update_many()
        .filter(device_pairing_codes::Column::CodeHash.eq(code_hash))
        .filter(device_pairing_codes::Column::ConsumedAt.is_null())
        .filter(device_pairing_codes::Column::ExpiresAt.gt(now))
        .col_expr(
            device_pairing_codes::Column::ConsumedAt,
            sea_orm::sea_query::Expr::value(now),
        )
        .exec(txn)
        .await?;
    Ok(res.rows_affected > 0)
}

// ---------------------------------------------------------------------------
// POST /v1/auth/device-pairing/finish
// ---------------------------------------------------------------------------

async fn pair_finish(
    State(state): State<SharedState>,
    Json(req): Json<PairFinishRequest>,
) -> Result<Response, ApiError> {
    // Rehydrate the ceremony (read without a lock; the txn is the authority).
    let row = webauthn_challenges::Entity::find_by_id(req.ceremony_id)
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown ceremony"))?;
    if row.kind != KIND_PAIR {
        return Err(ApiError::bad_request("ceremony is not a pairing"));
    }
    if row.expires_at <= OffsetDateTime::now_utc() {
        return Err(ApiError::unauthorized("ceremony expired"));
    }
    let user_id = UserId::from(
        row.user_id
            .clone()
            .ok_or_else(|| ApiError::internal(format_args!("pair ceremony missing user id")))?,
    );
    let code_hash = row
        .held_code_hash
        .clone()
        .ok_or_else(|| ApiError::internal(format_args!("pair ceremony missing code hash")))?;
    let reg_state: PasskeyRegistration = serde_json::from_value(row.state)
        .map_err(|e| ApiError::internal(format_args!("deserialize reg state: {e}")))?;

    // Verify the credential (CPU-bound, outside the txn).
    let passkey = state
        .webauthn
        .finish_passkey_registration(&req.credential, &reg_state)
        .map_err(register_err)?;
    let label = normalize_label(req.label)?;
    let credential_json = serde_json::to_value(&passkey)
        .map_err(|e| ApiError::internal(format_args!("serialize passkey: {e}")))?;
    let cred_id = passkey.cred_id().as_slice().to_vec();

    // Mint the session token up front; its hash is inserted inside the txn.
    let session = MintedToken::generate(SESSION);
    let session_hash = session.hash().as_bytes().to_vec();
    let set_cookie = build_set_cookie(&state.session_cfg, session.secret());
    let session_ttl = state.session_cfg.ttl;

    let ceremony_id = req.ceremony_id;
    let user_id_for_txn = user_id.clone();
    let outcome = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let credential_json = credential_json.clone();
            let cred_id = cred_id.clone();
            let label = label.clone();
            let user_id = user_id_for_txn.clone();
            let code_hash = code_hash.clone();
            let session_hash = session_hash.clone();
            Box::pin(async move {
                // Lock the challenge FOR UPDATE (anti-replay of one ceremony_id;
                // a parallel finish of the SAME id loses). The
                // anti-DOUBLE-ENROLL mutex across distinct ceremonies sharing a
                // code is the conditional code-consume below, not this lock.
                let locked = webauthn_challenges::Entity::find_by_id(ceremony_id)
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                let Some(challenge) = locked else {
                    return Err(TxnError::Api(ApiError::conflict(
                        "ceremony already finished or unknown",
                    )));
                };
                if challenge.expires_at <= OffsetDateTime::now_utc() {
                    return Err(TxnError::Api(ApiError::unauthorized("ceremony expired")));
                }

                // Re-check the owner is not disabled under the lock.
                let user = users::Entity::find_by_id(user_id.to_string())
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::unauthorized("invalid credentials")))?;
                if user.disabled_at.is_some() {
                    return Err(TxnError::Api(ApiError::unauthorized("invalid credentials")));
                }

                let now = OffsetDateTime::now_utc();
                // Consume the code: the conditional UPDATE is the
                // anti-double-enroll mutex (a racing finish of a distinct
                // ceremony sharing the code loses here).
                if !consume_pairing_code(txn, &code_hash, now).await? {
                    return Err(TxnError::Api(ApiError::conflict(
                        "pairing code expired or already used",
                    )));
                }

                // Denylist + insert (shared with add-passkey finish). The
                // unique-violation on cred_id is mapped to a 409 at the call site.
                bind_passkey_to_user(txn, &user_id, cred_id, credential_json, label).await?;

                webauthn_challenges::Entity::delete_by_id(ceremony_id)
                    .exec(txn)
                    .await?;

                // Device B just proved UV via the create ceremony; seed
                // `authed_at` so it can add another device without an immediate
                // re-login (mirrors register_finish / login_finish).
                let now = OffsetDateTime::now_utc();
                sessions::ActiveModel {
                    token_hash: Set(session_hash),
                    user_id: Set(user_id.to_string()),
                    created_at: Set(now),
                    expires_at: Set(now + session_ttl),
                    authed_at: Set(Some(now)),
                }
                .insert(txn)
                .await?;
                Ok(())
            })
        })
        .await;
    // A duplicate cred_id loses to `uniq_passkeys_cred_id`; surface as a 409.
    match outcome {
        Ok(()) => {}
        Err(sea_orm::TransactionError::Transaction(TxnError::Api(e))) => return Err(e),
        Err(sea_orm::TransactionError::Transaction(TxnError::Db(e)))
        | Err(sea_orm::TransactionError::Connection(e)) => {
            if let Some(SqlErr::UniqueConstraintViolation(msg)) = e.sql_err()
                && msg.contains(CRED_ID_UNIQUE_CONSTRAINT)
            {
                return Err(ApiError::conflict("credential already registered"));
            }
            return Err(map_db_err(e));
        }
    }

    Ok(set_cookie_response(
        &set_cookie,
        AuthFinishResponse {
            user_id: user_id.to_string(),
        },
        StatusCode::CREATED,
    ))
}

#[cfg(test)]
mod tests {
    //! The WebAuthn crypto ceremonies are exercised by the browser; these tests
    //! cover the pairing-code lifecycle + the helpers' edge cases that do not
    //! need a virtual authenticator. The finish handler's pre-crypto guards
    //! (unknown / wrong-kind / expired ceremony) all fire before
    //! `finish_passkey_registration`, so they are testable here; the
    //! consume-race and denylist live in extracted helpers exercised directly.
    use super::*;
    use crate::test_helpers::{make_state, seed_user};
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
        let user_id = seed_user(&state, "alice", "alice@example.test").await;
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
        let user_id = seed_user(&state, "frozen", "frozen@example.test").await;
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
            held_code_hash: Set(None),
            user_id: Set(None),
            email: Set(None),
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
        let user_id = seed_user(&state, "alice", "alice@example.test").await;
        let id = Uuid::new_v4();
        let past = OffsetDateTime::now_utc() - Duration::from_secs(60);
        webauthn_challenges::ActiveModel {
            id: Set(id),
            kind: Set(KIND_PAIR.to_string()),
            state: Set(serde_json::json!({})),
            held_code_hash: Set(Some(vec![0u8; 32])),
            user_id: Set(Some(user_id.to_string())),
            email: Set(None),
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
        let user_id = seed_user(&state, "alice", "alice@example.test").await;

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
        let user_id = seed_user(&state, "alice", "alice@example.test").await;
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
        let user_id = seed_user(&state, "alice", "alice@example.test").await;
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
        let user_id = seed_user(&state, "alice", "alice@example.test").await;
        let hash = seed_code(&state, &user_id, "ABCD-EFGH", PAIR_CODE_TTL).await;
        let now = OffsetDateTime::now_utc();
        for _ in 0..MAX_INFLIGHT_PAIR_CEREMONIES {
            webauthn_challenges::ActiveModel {
                id: Set(Uuid::new_v4()),
                kind: Set(KIND_PAIR.to_string()),
                state: Set(serde_json::json!({})),
                held_code_hash: Set(Some(hash.clone())),
                user_id: Set(Some(user_id.to_string())),
                email: Set(None),
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
}
