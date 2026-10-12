//! Username-first login ceremony (begin + finish) and the shared login tail.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use kallipai_archeion_common::ids::UserId;
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait,
    PaginatorTrait, QueryFilter, QuerySelect, TransactionTrait,
};
use serde::Deserialize;
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;
use webauthn_rs::prelude::{
    AuthenticationResult, Passkey, PasskeyAuthentication, PublicKeyCredential,
    RequestChallengeResponse, WebauthnError,
};

use crate::db::entity::{passkeys, users, webauthn_challenges};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::state::SharedState;
use crate::username;

use super::session::{
    AuthFinishResponse, CeremonyBeginResponse, mint_session_row, set_cookie_response,
};
use super::{CHALLENGE_TTL, KIND_LOGIN, MAX_INFLIGHT_CEREMONIES};

#[derive(Deserialize)]
pub(super) struct LoginBeginRequest {
    pub(super) username: String,
}

#[derive(Deserialize)]
pub(super) struct LoginFinishRequest {
    pub(super) ceremony_id: Uuid,
    pub(super) credential: PublicKeyCredential,
}

pub(super) async fn login_begin(
    State(state): State<SharedState>,
    Json(req): Json<LoginBeginRequest>,
) -> Result<Json<CeremonyBeginResponse<RequestChallengeResponse>>, ApiError> {
    let username_norm = username::normalize(&req.username)?;

    // Resolve the user by username (the login id). NOTE: this is a timing-
    // enumeration oracle -- an unknown username (or a user with no passkeys)
    // returns immediately, while a known username with passkeys pays the cost of
    // loading them + `start_passkey_authentication` (real crypto-state
    // construction) + an INSERT, so existence is distinguishable by latency.
    // The same generic "invalid credentials" body is used for all 401 branches
    // so the response BODY leaks nothing. Now that signup is open (more accounts
    // makes username enumeration more attractive) the per-IP rate limit on this
    // handler is the bound; the pre-public-launch fix is constant-time /
    // dummy-ceremony work, not message parity alone.
    let user = users::Entity::find()
        .filter(users::Column::Username.eq(username_norm))
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::unauthorized("invalid credentials"))?;
    // A disabled account cannot start a login. Same generic message as an
    // unknown user, so the response leaks nothing about account state.
    if user.disabled_at.is_some() {
        return Err(ApiError::unauthorized("invalid credentials"));
    }
    let user_id = UserId::from(user.id);

    // Bound concurrent in-flight login ceremonies for this user (see register).
    let now = OffsetDateTime::now_utc();
    let in_flight = webauthn_challenges::Entity::find()
        .filter(webauthn_challenges::Column::UserId.eq(user_id.to_string()))
        .filter(webauthn_challenges::Column::ExpiresAt.gt(now))
        .count(&state.db)
        .await
        .map_err(map_db_err)?;
    if in_flight >= MAX_INFLIGHT_CEREMONIES {
        return Err(ApiError::too_many_requests("too many in-flight ceremonies"));
    }

    // Load the user's passkeys. The `passkeys` table holds only live
    // credentials (revoked / cloned ones are hard-deleted into
    // `passkey_revocations`), so there is no status filter here. The wrapper
    // bakes them into the ceremony state so finish verifies the assertion
    // against the right public keys.
    let owned = passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .all(&state.db)
        .await
        .map_err(map_db_err)?;
    if owned.is_empty() {
        return Err(ApiError::unauthorized("invalid credentials"));
    }
    let creds: Vec<Passkey> = owned
        .iter()
        .map(|p| serde_json::from_value::<Passkey>(p.credential.clone()))
        .collect::<Result<_, _>>()
        .map_err(|e| ApiError::internal(format_args!("deserialize passkey: {e}")))?;

    let (options, auth_state) = state
        .webauthn
        .start_passkey_authentication(&creds)
        .map_err(login_err)?;
    let state_value = serde_json::to_value(&auth_state)
        .map_err(|e| ApiError::internal(format_args!("serialize auth state: {e}")))?;

    let ceremony_id = Uuid::new_v4();
    webauthn_challenges::ActiveModel {
        id: Set(ceremony_id),
        kind: Set(KIND_LOGIN.to_string()),
        state: Set(state_value),
        pairing_code_hash: Set(None),
        user_id: Set(Some(user_id.to_string())),
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

pub(super) async fn login_finish(
    State(state): State<SharedState>,
    Json(req): Json<LoginFinishRequest>,
) -> Result<Response, ApiError> {
    // Rehydrate the login ceremony (read without a lock; the txn below is the
    // authority and locks the row for the consume).
    let row = webauthn_challenges::Entity::find_by_id(req.ceremony_id)
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    let row = row.ok_or_else(|| ApiError::not_found("unknown ceremony"))?;
    if row.kind != KIND_LOGIN {
        return Err(ApiError::bad_request("ceremony is not a login"));
    }
    if row.expires_at <= OffsetDateTime::now_utc() {
        return Err(ApiError::unauthorized("ceremony expired"));
    }
    let auth_state: PasskeyAuthentication = serde_json::from_value(row.state)
        .map_err(|e| ApiError::internal(format_args!("deserialize auth state: {e}")))?;
    let user_id = UserId::from(
        row.user_id
            .clone()
            .ok_or_else(|| ApiError::internal(format_args!("login ceremony missing user id")))?,
    );

    // The authenticated credential's id, extracted up front so the counter-
    // advance lookup below can resolve the matching passkey row by cred_id.
    let raw_id = req.credential.raw_id.as_slice().to_vec();

    // Verify the assertion (CPU-bound, outside the txn). The state already
    // carries the user's passkeys (baked at begin), so no set_allowed_credentials
    // reach-through is needed. The wrapper enforces the signature-counter clone
    // check (returns CredentialPossibleCompromise on a regression). Note: clone
    // detection only fires for authenticators that maintain a non-zero monotonic
    // counter; synced/software passkeys report counter == 0 and never trigger it.
    let auth_result = check_login_assertion(
        &user_id,
        &raw_id,
        state
            .webauthn
            .finish_passkey_authentication(&req.credential, &auth_state),
    )?;

    // Resolve the matching passkey row by credential id, owner-scoped (see the
    // clone branch above). Needed to advance its stored counter inside the txn.
    let passkey_id = resolve_owner_passkey_id(&state.db, &user_id, &raw_id).await?;

    finalize_login_and_mint_session(&state, req.ceremony_id, &user_id, passkey_id, auth_result)
        .await
}

/// Interpret a login assertion result shared by `login_finish` and
/// `login_discoverable_finish`. A counter regression (`CredentialPossible-
/// Compromise`) is DENY + LOG ONLY (never revoke -- see the module doc); any
/// other WebAuthn error maps via `login_err`. Centralized so the two paths
/// cannot drift on the clone-detection policy.
pub(super) fn check_login_assertion(
    user_id: &UserId,
    raw_id: &[u8],
    result: Result<AuthenticationResult, WebauthnError>,
) -> Result<AuthenticationResult, ApiError> {
    match result {
        Ok(r) => Ok(r),
        Err(WebauthnError::CredentialPossibleCompromise) => {
            warn!(
                user_id = %user_id,
                cred_id = %hex::encode(raw_id),
                "passkey signature-counter regression (possible clone); denying login without revoking"
            );
            Err(ApiError::unauthorized("credential may be cloned"))
        }
        Err(e) => Err(login_err(e)),
    }
}

/// Resolve the matching passkey row by credential id, owner-scoped. Shared by
/// the two login finish paths (both need the row id to advance its counter in
/// `finalize_login_and_mint_session`).
pub(super) async fn resolve_owner_passkey_id(
    db: &DatabaseConnection,
    user_id: &UserId,
    raw_id: &[u8],
) -> Result<Uuid, ApiError> {
    Ok(passkeys::Entity::find()
        .filter(passkeys::Column::CredId.eq(raw_id))
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .one(db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::unauthorized("unknown credential"))?
        .id)
}

/// Shared tail of `login_finish` and `login_discoverable_finish`: mint a fresh
/// session token, then in ONE transaction lock the challenge row `FOR UPDATE`
/// (defeats a parallel replay of the same ceremony_id: the loser sees the row
/// gone -> 409), re-check the user is not disabled under the lock, advance the
/// stored passkey via `Passkey::update_credential` (no lost-update), insert the
/// session (seeding the step-up `authed_at`), and delete the challenge.
/// All-or-nothing so a transient failure cannot advance the counter without
/// issuing a session. Returns the `Set-Cookie` + `{ user_id }` 200 response.
///
/// Both login paths have already verified the assertion (CPU, outside the txn)
/// and resolved the owner-scoped `passkey_id` before calling this.
pub(super) async fn finalize_login_and_mint_session(
    state: &SharedState,
    ceremony_id: Uuid,
    user_id: &UserId,
    passkey_id: Uuid,
    auth_result: AuthenticationResult,
) -> Result<Response, ApiError> {
    let user_id_for_txn = user_id.clone();
    let session_cfg = state.session_cfg.clone();

    let outcome = state
        .db
        .transaction::<_, String, TxnError>(|txn| {
            let user_id = user_id_for_txn.clone();
            let session_cfg = session_cfg.clone();
            Box::pin(async move {
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

                // Re-check the owning user is not disabled, taking a `users`
                // row lock. This narrows the disabled-check race to a single
                // ordering: whoever wins `FOR UPDATE` first proceeds. It does
                // not fully close the race (if login wins it still mints a
                // session before a concurrent disable commits), but a disable
                // that has not yet acquired the row lock is visible here. Same
                // generic message as the begin-path check.
                let user = users::Entity::find_by_id(user_id.to_string())
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::unauthorized("invalid credentials")))?;
                if user.disabled_at.is_some() {
                    return Err(TxnError::Api(ApiError::unauthorized("invalid credentials")));
                }

                // Re-read the passkey under the lock and advance it via the
                // library helper. `None` means the cred_id no longer matches the
                // authenticated credential (the row moved under us) -- a HARD
                // error, not a silent skip: issuing a session while leaving the
                // stored `Passkey` stale would corrupt its integrity (the stored
                // counter + credential must reflect what was just verified).
                // `Some(false)` = nothing changed (most passkeys); skip the write.
                let current = passkeys::Entity::find_by_id(passkey_id)
                    .one(txn)
                    .await?
                    .ok_or_else(|| {
                        TxnError::Api(ApiError::unauthorized("credential removed during login"))
                    })?;
                let mut stored: Passkey = serde_json::from_value(current.credential.clone())
                    .map_err(|e| {
                        TxnError::Db(DbErr::Custom(format!("deserialize passkey: {e}")))
                    })?;
                let now = OffsetDateTime::now_utc();
                match stored.update_credential(&auth_result) {
                    Some(true) => {
                        let updated_json = serde_json::to_value(&stored).map_err(|e| {
                            TxnError::Db(DbErr::Custom(format!("serialize passkey: {e}")))
                        })?;
                        let mut am: passkeys::ActiveModel = current.into();
                        am.credential = Set(updated_json);
                        am.last_used_at = Set(now);
                        am.update(txn).await?;
                    }
                    Some(false) => {
                        // Counter unchanged (most software passkeys report a
                        // constant 0); still stamp `last_used_at` so the
                        // management UI can show "last used".
                        let mut am: passkeys::ActiveModel = current.into();
                        am.last_used_at = Set(now);
                        am.update(txn).await?;
                    }
                    None => {
                        return Err(TxnError::Api(ApiError::internal(
                            "credential id mismatch on login finish",
                        )));
                    }
                }

                let set_cookie = mint_session_row(txn, &user_id, &session_cfg, now).await?;

                webauthn_challenges::Entity::delete_by_id(ceremony_id)
                    .exec(txn)
                    .await?;
                Ok(set_cookie)
            })
        })
        .await;
    let set_cookie = flatten_txn(outcome)?;

    Ok(set_cookie_response(
        &set_cookie,
        AuthFinishResponse {
            user_id: user_id.to_string(),
        },
        StatusCode::OK,
    ))
}

/// An authentication failure is 401. `CredentialPossibleCompromise` keeps a
/// distinct message: it is an intentional signal to the legitimate user that
/// their passkey's counter regressed (possible clone). Every other failure gets
/// a generic message; the detail lives only in the log.
pub(super) fn login_err(e: WebauthnError) -> ApiError {
    warn!(error = %e, "webauthn login failed");
    match e {
        WebauthnError::CredentialPossibleCompromise => {
            ApiError::unauthorized("credential may be cloned")
        }
        _ => ApiError::unauthorized("passkey login failed"),
    }
}
