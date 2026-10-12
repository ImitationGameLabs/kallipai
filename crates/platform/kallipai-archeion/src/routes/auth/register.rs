//! Open-signup registration ceremony (begin + finish), its state loader, and the
//! discoverable registration challenge builder.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use kallipai_archeion_common::ids::UserId;
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QuerySelect, SqlErr, TransactionError, TransactionTrait,
};
use serde::Deserialize;
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, RegisterPublicKeyCredential, WebauthnError,
};
use webauthn_rs_core::proto::{
    AttestationConveyancePreference, CredProtect, CredentialProtectionPolicy, RegistrationState,
    RequestRegistrationExtensions, UserVerificationPolicy,
};

use crate::db::entity::{passkeys, users, webauthn_challenges};
use crate::db::{TxnError, map_db_err};
use crate::state::SharedState;
use crate::username;

use super::session::{
    AuthFinishResponse, CeremonyBeginResponse, mint_session_row, set_cookie_response,
};
use super::{CHALLENGE_TTL, KIND_REGISTER, MAX_INFLIGHT_CEREMONIES, USERNAME_UNIQUE_CONSTRAINT};

#[derive(Deserialize)]
pub(super) struct RegisterBeginRequest {
    pub(super) username: String,
    pub(super) display_name: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RegisterFinishRequest {
    pub(super) ceremony_id: Uuid,
    pub(super) credential: RegisterPublicKeyCredential,
}

/// Max length of a client-supplied `display_name` (after trim). The WebAuthn
/// `user.displayName` is shown in the authenticator prompt; an unbounded value
/// is both a prompt-DoS and a storage concern.
const MAX_DISPLAY_NAME_LEN: usize = 64;

pub(super) async fn register_begin(
    State(state): State<SharedState>,
    Json(req): Json<RegisterBeginRequest>,
) -> Result<Json<CeremonyBeginResponse<CreationChallengeResponse>>, ApiError> {
    // Open signup is gated by the runtime kill switch.
    // Login is unaffected. Register can surface an
    // explicit 403 because the request IS signup intent; OAuth `finish_signin`
    // cannot -- it must mask signup-disabled as a generic 401 so a disabled-
    // signup probe does not reveal whether `(provider, subject)` is a known
    // account (see `routes::oauth`).
    if !state.signup_enabled {
        return Err(ApiError::forbidden("signup is disabled"));
    }
    // Normalize the username (the login id + in-site handle) once; the same
    // transform runs at login_begin so a user can log in with exactly the handle
    // they registered.
    let username_norm = username::normalize(&req.username)?;
    // Reserved names are a deterministic policy, not a race: reject at begin
    // so no ceremony row is created and no authenticator prompt is shown.
    // finish re-checks with the same authority (defense in depth).
    if username::is_reserved(&username_norm) {
        return Err(ApiError::bad_request("username is reserved"));
    }
    // The WebAuthn `displayName` shown in the authenticator prompt MUST be
    // non-empty -- webauthn-rs rejects an empty one -- so when the client omits
    // `display_name` we fall back to the normalized username. Trim and cap the
    // length on the trimmed slice before cloning (the body limit already bounds
    // the raw input, but avoid materializing a huge trimmed copy).
    let display_name_for_prompt = match req.display_name.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => {
            if s.chars().count() > MAX_DISPLAY_NAME_LEN {
                return Err(ApiError::bad_request(format!(
                    "display_name longer than {MAX_DISPLAY_NAME_LEN} chars"
                )));
            }
            s.to_string()
        }
        _ => username_norm.clone(),
    };
    // NOTE: this fallback is ceremony-local ONLY. It is NOT persisted
    // (`users.display_name` stays NULL) and the data layer does no synthesis:
    // `/me` returns `display_name` verbatim and leaves any fallback
    // rendering to the frontend. The two layers intentionally differ -- the
    // authenticator requires a non-empty label at ceremony time, while the API
    // represents stored data faithfully.

    // Open signup (no invite). This per-username cap bounds repeat-begin
    // floods against a single chosen handle; it is NOT a global storage bound
    // (an attacker rotating the username field defeats it) -- the per-IP rate
    // limiter on this endpoint is the real storage-growth bound (each begin
    // adds one TTL-bounded row, GC'd every 60s). Only live (unexpired) rows
    // count.
    let now = OffsetDateTime::now_utc();
    let in_flight = webauthn_challenges::Entity::find()
        .filter(webauthn_challenges::Column::Username.eq(username_norm.clone()))
        .filter(webauthn_challenges::Column::Kind.eq(KIND_REGISTER))
        .filter(webauthn_challenges::Column::ExpiresAt.gt(now))
        .count(&state.db)
        .await
        .map_err(map_db_err)?;
    if in_flight >= MAX_INFLIGHT_CEREMONIES {
        return Err(ApiError::too_many_requests("too many in-flight ceremonies"));
    }

    // Pre-generate the UserId so it rides the ceremony and becomes the WebAuthn
    // `user.id` (the opaque, stable handle -- NOT the email or username, to
    // avoid correlating any of them). `UserId` is a UUID-v4 string newtype, so
    // it parses back to the `Uuid` the wrapper wants.
    let user_id = UserId::random();
    let user_uuid = Uuid::parse_str(user_id.as_ref())
        .map_err(|e| ApiError::internal(format_args!("user id not a uuid: {e}")))?;
    // Signup enrolls a DISCOVERABLE (resident-key) credential so the new account
    // is immediately usable for passwordless / conditional-UI login (the
    // authenticator stores the userHandle). `user.name` is the username (the
    // stable handle); `display_name_for_prompt` is the `displayName`. A brand-new
    // user has no existing credentials, so exclude is None.
    let (options, reg_state) = discoverable_registration_challenge(
        &state,
        user_uuid,
        &username_norm,
        &display_name_for_prompt,
        None,
    )?;
    let state_value = serde_json::to_value(&reg_state)
        .map_err(|e| ApiError::internal(format_args!("serialize reg state: {e}")))?;

    let ceremony_id = Uuid::new_v4();
    webauthn_challenges::ActiveModel {
        id: Set(ceremony_id),
        kind: Set(KIND_REGISTER.to_string()),
        state: Set(state_value),
        // Open signup holds no code hash; the column stays for device pairing.
        pairing_code_hash: Set(None),
        user_id: Set(Some(user_id.to_string())),
        username: Set(Some(username_norm)),
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

pub(super) async fn register_finish(
    State(state): State<SharedState>,
    Json(req): Json<RegisterFinishRequest>,
) -> Result<Response, ApiError> {
    // Re-check the signup kill switch at finish too (mirrors `register_begin`
    // and OAuth `finish_signin`): without this, an in-flight registration stays
    // completable for up to `CHALLENGE_TTL` after the operator flips the switch.
    // Fires before any ceremony lookup, so the 403 leaks nothing about whether
    // the ceremony exists.
    if !state.signup_enabled {
        return Err(ApiError::forbidden("signup is disabled"));
    }
    // Rehydrate the ceremony state and run the (CPU-bound) registration
    // verification OUTSIDE the transaction so the row locks are not held across
    // crypto.
    let (reg_state, user_id, username) = load_register_state(&state.db, req.ceremony_id).await?;
    // Defense-in-depth twin of the begin-side reserved check: the ceremony
    // row may predate this list (challenge TTL) or come from a future code
    // path. Deterministic, so it needs no transaction.
    if username::is_reserved(&username) {
        return Err(ApiError::bad_request("username is reserved"));
    }
    // Discoverable signup: the begin stored a bare core `RegistrationState`; finish
    // through `WebauthnCore` and wrap the resulting `Credential` as a `Passkey`
    // (mirrors the discoverable add-passkey finish in `routes::passkeys`).
    let cred = state
        .webauthn_core
        .register_credential(&req.credential, &reg_state, None)
        .map_err(register_err)?;
    let passkey = Passkey::from(cred);

    let session_cfg = state.session_cfg.clone();

    // One transaction: lock challenge -> username, insert user + passkey,
    // delete the challenge, mint the session.
    let ceremony_id = req.ceremony_id;
    let credential_json = serde_json::to_value(&passkey)
        .map_err(|e| ApiError::internal(format_args!("serialize passkey: {e}")))?;
    let cred_id = passkey.cred_id().as_slice().to_vec();
    let user_id_for_txn = user_id.clone();
    let result = state
        .db
        .transaction::<_, String, TxnError>(|txn| {
            let user_id = user_id_for_txn.clone();
            let username = username.clone();
            let credential_json = credential_json.clone();
            let cred_id = cred_id.clone();
            let session_cfg = session_cfg.clone();
            Box::pin(async move {
                // Lock the challenge row; a parallel finish on the same ceremony
                // already deleted it -> the loser sees None -> 409.
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

                // Username (login id + in-site handle) uniqueness: FOR
                // UPDATE-then-check so a taken handle maps to a clean 409. The
                // `uniq_users_username` index is the backstop for the sub-ms
                // simultaneous-insert race.
                let existing = users::Entity::find()
                    .filter(users::Column::Username.eq(username.clone()))
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                if existing.is_some() {
                    return Err(TxnError::Api(ApiError::conflict("username already taken")));
                }

                let now = OffsetDateTime::now_utc();
                users::ActiveModel {
                    id: Set(user_id.to_string()),
                    username: Set(username),
                    display_name: Set(None),
                    created_at: Set(now),
                    disabled_at: Set(None),
                }
                .insert(txn)
                .await?;

                passkeys::ActiveModel {
                    id: Set(Uuid::new_v4()),
                    user_id: Set(user_id.to_string()),
                    cred_id: Set(cred_id),
                    credential: Set(credential_json),
                    // Initial passkey has no user-chosen label yet (the
                    // management UI lets the user name it later).
                    label: Set(String::new()),
                    created_at: Set(now),
                    last_used_at: Set(now),
                    // Signup enrolls a discoverable (resident-key) credential so
                    // the account is immediately usable for passwordless login.
                    discoverable: Set(true),
                }
                .insert(txn)
                .await?;

                webauthn_challenges::Entity::delete_by_id(ceremony_id)
                    .exec(txn)
                    .await?;

                let set_cookie = mint_session_row(txn, &user_id, &session_cfg, now).await?;
                Ok(set_cookie)
            })
        })
        .await;
    // Flatten the transaction result. The `users` insert can still race a
    // parallel register of the same username (the FOR UPDATE pre-check above
    // wins the common case; the sub-ms simultaneous-insert case loses to the
    // `uniq_users_username` index). Discriminate that unique-constraint
    // violation by constraint name and surface it as a clean 409; any other
    // unique violation (e.g. a duplicate passkey cred_id, which is never
    // legitimate) stays a generic 500 via map_db_err.
    let set_cookie = match result {
        Ok(set_cookie) => set_cookie,
        Err(TransactionError::Transaction(TxnError::Api(e))) => return Err(e),
        Err(TransactionError::Transaction(TxnError::Db(e)))
        | Err(TransactionError::Connection(e)) => {
            if let Some(SqlErr::UniqueConstraintViolation(msg)) = e.sql_err()
                && msg.contains(USERNAME_UNIQUE_CONSTRAINT)
            {
                return Err(ApiError::conflict("username already taken"));
            }
            return Err(map_db_err(e));
        }
    };

    Ok(set_cookie_response(
        &set_cookie,
        AuthFinishResponse {
            user_id: user_id.to_string(),
        },
        StatusCode::CREATED,
    ))
}

/// Build a discoverable (resident-key) registration challenge via the bare
/// `WebauthnCore`. The high-level `Webauthn::start_passkey_registration` wrapper
/// hardcodes `require_resident_key=false` and keeps its `core` field private, so
/// a discoverable credential (the kind conditional-UI / usernameless login
/// resolves) must be minted through the core directly. Mirrors that wrapper's
/// builder verbatim EXCEPT `require_resident_key(true)`; the cred_protect block
/// keeps `enforce=false` (upstream warns true breaks many authenticators).
///
/// Returns the bare core `RegistrationState` (a different shape from
/// `PasskeyRegistration`); the caller finishes via
/// `WebauthnCore::register_credential` + `Passkey::from`. Shared by signup
/// (`register_begin`, `exclude = None`) and the discoverable add-passkey flow
/// (`routes::passkeys`, `exclude = Some(existing cred ids)`).
pub(crate) fn discoverable_registration_challenge(
    state: &SharedState,
    user_uuid: Uuid,
    username: &str,
    display: &str,
    exclude: Option<Vec<Vec<u8>>>,
) -> Result<(CreationChallengeResponse, RegistrationState), ApiError> {
    let builder = state
        .webauthn_core
        .new_challenge_register_builder(user_uuid.as_bytes(), username, display)
        .map_err(register_err)?
        .attestation(AttestationConveyancePreference::None)
        .require_resident_key(true)
        .authenticator_attachment(None)
        .user_verification_policy(UserVerificationPolicy::Required)
        .reject_synchronised_authenticators(false)
        .exclude_credentials(exclude)
        .hints(None)
        .extensions(Some(RequestRegistrationExtensions {
            cred_protect: Some(CredProtect {
                credential_protection_policy: CredentialProtectionPolicy::UserVerificationRequired,
                enforce_credential_protection_policy: Some(false),
            }),
            uvm: Some(true),
            cred_props: Some(true),
            min_pin_length: None,
            hmac_create_secret: None,
        }));
    let (ccr, rs) = state
        .webauthn_core
        .generate_challenge_register(builder)
        .map_err(register_err)?;
    Ok((ccr, rs))
}

/// Read a register ceremony, rehydrate its bare core `RegistrationState`, and
/// return the pre-generated `UserId` and the chosen username. Errors if the
/// ceremony is missing, expired, or not a register ceremony.
async fn load_register_state(
    db: &crate::db::Db,
    ceremony_id: Uuid,
) -> Result<(RegistrationState, UserId, String), ApiError> {
    let row = webauthn_challenges::Entity::find_by_id(ceremony_id)
        .one(db)
        .await
        .map_err(map_db_err)?;
    let row = row.ok_or_else(|| ApiError::not_found("unknown ceremony"))?;
    if row.kind != KIND_REGISTER {
        return Err(ApiError::bad_request("ceremony is not a registration"));
    }
    if row.expires_at <= OffsetDateTime::now_utc() {
        return Err(ApiError::unauthorized("ceremony expired"));
    }
    let user_id = row
        .user_id
        .clone()
        .ok_or_else(|| ApiError::internal(format_args!("register ceremony missing user id")))?;
    let username = row
        .username
        .clone()
        .ok_or_else(|| ApiError::internal(format_args!("register ceremony missing username")))?;
    let state: RegistrationState = serde_json::from_value(row.state)
        .map_err(|e| ApiError::internal(format_args!("deserialize reg state: {e}")))?;
    Ok((state, UserId::from(user_id), username))
}

/// A registration failure is a client error (bad/invalid credential). The
/// `WebauthnError` detail is logged but NOT returned to the client — it can
/// distinguish failure modes and so leak why verification failed.
pub(crate) fn register_err(e: WebauthnError) -> ApiError {
    warn!(error = %e, "webauthn register failed");
    ApiError::bad_request("passkey registration failed")
}
