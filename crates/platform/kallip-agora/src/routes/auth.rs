//! WebAuthn passkey registration + login, and session minting.
//!
//! # Clone detection & signature counters — READ BEFORE CHANGING LOGIN
//!
//! WebAuthn's signature counter (`signCount`) was designed to detect cloned
//! credentials on single-device HARDWARE authenticators. It is effectively dead
//! for modern passkeys, and a regression MUST NOT auto-revoke a credential:
//!
//! - Synced passkeys (iCloud Keychain, Google, Microsoft, 1Password, ...)
//!   intentionally share one private key across devices, so they report
//!   `signCount = 0` forever and the regression check never runs. That is
//!   correct — they are legitimately multi-device, not cloned.
//! - Platform authenticators (TPM / Secure Enclave) often keep no reliable
//!   monotonic counter; a regression is frequently a firmware quirk, not a clone.
//! - Auto-revoking on a regression is an ATTACKER-TRIGGERABLE DoS: a holder of a
//!   stale clone (counter behind the stored value) can force a regression that
//!   destroys the legitimate user's credential, while the legit user — also
//!   failing the regression — cannot self-recover.
//!
//! Accordingly, the login finish handlers (`login_finish` and
//! `login_discoverable_finish`, sharing the `finalize_login_and_mint_session`
//! tail) treat `CredentialPossibleCompromise` as DENY-THIS-LOGIN + LOG ONLY:
//! they return 401 and emit a structured warn (`user_id`, `cred_id`) for
//! monitoring. They NEVER mutate the credential. A regression already fails the
//! login regardless, so not revoking costs no real protection; a leading clone
//! is undetectable anyway.
//!
//! Then how ARE clones prevented? Structurally, not at auth time:
//! - Passkey private keys are non-exportable (locked in TPM / Secure Enclave /
//!   Android Keystore) and the sync fabric is E2EE — there is no software blob
//!   an attacker can copy short of compromising the secure enclave or sync vendor.
//! - The RP stores only PUBLIC keys, so a DB breach leaks nothing usable.
//! - Clone RECOVERY is the self-service surface in `routes/passkeys.rs`: suspect
//!   a credential? revoke it (hard-delete + audit row) from another device, then
//!   add a new one. The last-live-passkey guard blocks self-lockout on revoke.
//!
//! References: W3C webauthn-3 §7.2.4; Adam Langley, "Signature counters"
//! (imperialviolet, 2023); MojoAuth, "signCount Is Dead".
//!
//! # Identity model
//!
//! The **login id is the username** (`users.username`, normalized via
//! `crate::username`). `login_begin` resolves the user by username. Email is
//! NOT a login id and NOT collected at registration: it is an optional
//! contact/recovery channel a user links later in settings, stored in the
//! `emails` table (1:N, with a primary + verification state). WebAuthn
//! `user.name` is the username; `display_name` surfaces only as the fallback
//! WebAuthn `displayName` (when the client omits one) and in `/v1/me`. `user.id`
//! stays the opaque pre-generated `UserId` -- the only crypto binding.
//!
//! # Ceremonies
//!
//! - **register** `begin`/`finish`: open signup (no invite). `begin` normalizes
//!   the username, synthesizes a prompt-only `display_name`, pre-generates the
//!   `UserId`, and persists a bare discoverable `RegistrationState` (+ username)
//!   on the challenge row. `finish` verifies the credential (CPU, outside the
//!   txn) through `WebauthnCore`, then in ONE transaction locks the challenge
//!   row `FOR UPDATE`, checks username uniqueness `FOR UPDATE` (`409` on
//!   conflict), inserts the user + passkey (marked `discoverable`), deletes the
//!   challenge, and mints a fresh session. A parallel double-finish on one
//!   ceremony loses on the row lock.
//! - **login** `begin`/`finish`: username-first. `begin` resolves the user by
//!   `username`, loads their passkeys, and bakes them into the ceremony state
//!   via the wrapper's `start_passkey_authentication`. `finish` verifies the
//!   assertion against that baked state (CPU, outside the txn) and advances the
//!   stored passkey via `Passkey::update_credential` inside the SAME transaction
//!   that locks the challenge `FOR UPDATE`, inserts the session, and deletes the
//!   challenge — so a parallel ceremony-id replay loses on the row lock and a
//!   transient failure cannot advance the counter without issuing a session.
//!   `update_credential` returning `None` (cred_id mismatch) is a hard 500, not
//!   a silent skip: it means the row moved under us, and issuing a session
//!   while the stored `Passkey` is out of sync with what was verified would
//!   weaken the next assertion's baseline. (This is integrity of the stored
//!   credential, not clone detection — see the section above.)
//! - **login (discoverable)** `begin`/`finish`: passwordless, usernameless.
//!   `begin` resolves NO user — it returns conditional-UI options with an empty
//!   allowList. `finish` identifies the account from the assertion's signed
//!   `userHandle`, then reuses the regular login tail (`finalize_login_and_mint_
//!   session`) so the two login paths cannot drift. The assertion itself is
//!   verified via `finish_discoverable_authentication` against the resolved
//!   user's credentials (CPU, outside the txn).
//!
//! Session ids are rotated: every register/login finish mints a brand-new
//! session token (never reuses a pre-login one), defeating session fixation.

use crate::db::entity::{passkeys, sessions, users, webauthn_challenges};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::session::{SessionCfg, build_set_cookie};
use crate::state::SharedState;
use crate::token::SESSION;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use kallip_agora_common::ids::UserId;
use kallip_common::authtoken::MintedToken;
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, DatabaseTransaction,
    DbErr, EntityTrait, PaginatorTrait, QueryFilter, QuerySelect, SqlErr, TransactionError,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;
use webauthn_rs::prelude::{
    AuthenticationResult, CreationChallengeResponse, DiscoverableAuthentication, DiscoverableKey,
    Passkey, PasskeyAuthentication, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, WebauthnError,
};
use webauthn_rs_core::proto::{
    AttestationConveyancePreference, CredProtect, CredentialProtectionPolicy, RegistrationState,
    RequestRegistrationExtensions, UserVerificationPolicy,
};

use crate::username;

/// Ceremony-kind discriminators stored on `webauthn_challenges.kind`. Each
/// begin path writes its own kind so the finish handler picks the right state
/// type/resolver and the per-kind in-flight caps do not collide. Shared with
/// `routes::passkeys` (the add-passkey ceremony) and `routes::device_pairing`
/// (the cross-device pair ceremony).
///
/// Open-signup registration: enrolls a DISCOVERABLE (resident-key) credential,
/// so its challenge row holds a bare core `RegistrationState` (distinct from
/// `KIND_ADD_REGULAR`, whose non-discoverable add-passkey rehydrates a
/// `PasskeyRegistration`). DISTINCT kinds also keep the per-kind in-flight caps
/// independent and let each finish handler route a row unambiguously.
pub(crate) const KIND_REGISTER: &str = "register";
/// Authenticated non-discoverable add-passkey ceremony (rehydrates a
/// `PasskeyRegistration`; resolved by the add-passkey finish handler, not
/// register_finish). Discoverable add-passkey uses `KIND_ADD_DISCOVERABLE`.
pub(crate) const KIND_ADD_REGULAR: &str = "add_regular";
const KIND_LOGIN: &str = "login";
/// Discoverable (usernameless) login: the user is resolved at finish from the
/// assertion's `userHandle`, not supplied at begin. Distinct kind so the finish
/// handler picks the right state type (`DiscoverableAuthentication`) and
/// resolver.
const KIND_LOGIN_DISCOVERABLE: &str = "login_discoverable";
pub(crate) const KIND_PAIR: &str = "pair";

/// How long an in-flight ceremony remains valid. Browsers prompt the user
/// within this window; a stale challenge is rejected at finish and GC'd at begin.
/// Shared with `routes::passkeys` (the add-passkey ceremony TTL).
pub(crate) const CHALLENGE_TTL: Duration = Duration::from_secs(300);

/// Name of the `users.username` unique index (see migration). Matched against
/// the Postgres unique-violation message to discriminate a username-collision
/// race (-> 409) from any other unique violation in the same transaction.
const USERNAME_UNIQUE_CONSTRAINT: &str = "uniq_users_username";

/// Max live (unexpired) ceremonies per username (register) / user (login).
/// Bounds `webauthn_challenges` storage growth against an attacker who spams
/// begins; the per-client rate limiter is the primary gate, this is the storage
/// bound. Count-then-insert, so the cap is soft under true concurrency.
const MAX_INFLIGHT_CEREMONIES: u64 = 16;

/// The unauthenticated, crypto-expensive ceremony BEGIN endpoints. These are
/// the ceremony-spam entry surface and the only ceremony routes the per-client
/// rate limiter should cover (see `routes::router`). A
/// begin mints the unguessable `ceremony_id` that finish then consumes, so
/// finish is transitively bounded by begin's rate limit and is NOT itself
/// rate-limited (otherwise a login ceremony would cost two tokens).
pub fn begin_router() -> Router<SharedState> {
    Router::new()
        .route("/auth/register/begin", post(register_begin))
        .route("/auth/login/begin", post(login_begin))
        .route(
            "/auth/login/discoverable/begin",
            post(login_discoverable_begin),
        )
}

/// The ceremony FINISH endpoints. Not rate-limited: each requires a real,
/// unguessable, single-use `ceremony_id` issued by a (rate-limited) begin, so
/// the verification surface here is bounded by begin's limiter.
pub fn finish_router() -> Router<SharedState> {
    Router::new()
        .route("/auth/register/finish", post(register_finish))
        .route("/auth/login/finish", post(login_finish))
        .route(
            "/auth/login/discoverable/finish",
            post(login_discoverable_finish),
        )
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RegisterBeginRequest {
    username: String,
    display_name: Option<String>,
}

#[derive(Serialize, Debug)]
pub(crate) struct CeremonyBeginResponse<T: Serialize> {
    pub ceremony_id: String,
    pub options: T,
}

#[derive(Deserialize)]
struct RegisterFinishRequest {
    ceremony_id: Uuid,
    credential: RegisterPublicKeyCredential,
}

#[derive(Deserialize)]
struct LoginBeginRequest {
    username: String,
}

#[derive(Deserialize)]
struct LoginFinishRequest {
    ceremony_id: Uuid,
    credential: PublicKeyCredential,
}

#[derive(Serialize)]
struct AuthFinishResponse {
    user_id: String,
}

/// Max length of a client-supplied `display_name` (after trim). The WebAuthn
/// `user.displayName` is shown in the authenticator prompt; an unbounded value
/// is both a prompt-DoS and a storage concern.
const MAX_DISPLAY_NAME_LEN: usize = 64;

// ---------------------------------------------------------------------------
// register
// ---------------------------------------------------------------------------

async fn register_begin(
    State(state): State<SharedState>,
    Json(req): Json<RegisterBeginRequest>,
) -> Result<Json<CeremonyBeginResponse<CreationChallengeResponse>>, ApiError> {
    // Open signup is gated by the runtime kill switch (the invite gate is gone;
    // this is its replacement). Login is unaffected. Register can surface an
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
    // `/v1/me` returns `display_name` verbatim and leaves any fallback
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

async fn register_finish(
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

// ---------------------------------------------------------------------------
// login (username-first)
// ---------------------------------------------------------------------------

async fn login_begin(
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

async fn login_finish(
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

// ---------------------------------------------------------------------------
// discoverable (usernameless) login
// ---------------------------------------------------------------------------

/// Begin a discoverable login. No identifier is supplied -- the authenticator
/// resolves the user at finish via the assertion's `userHandle`. The wrapper
/// forces conditional mediation + an empty allowList. No per-user in-flight cap
/// is possible (no user handle at begin); the per-IP rate limiter is the gate.
async fn login_discoverable_begin(
    State(state): State<SharedState>,
) -> Result<Json<CeremonyBeginResponse<RequestChallengeResponse>>, ApiError> {
    let (options, auth_state) = state
        .webauthn
        .start_discoverable_authentication()
        .map_err(login_err)?;
    let state_value = serde_json::to_value(&auth_state)
        .map_err(|e| ApiError::internal(format_args!("serialize auth state: {e}")))?;
    let now = OffsetDateTime::now_utc();
    let ceremony_id = Uuid::new_v4();
    webauthn_challenges::ActiveModel {
        id: Set(ceremony_id),
        kind: Set(KIND_LOGIN_DISCOVERABLE.to_string()),
        state: Set(state_value),
        pairing_code_hash: Set(None),
        user_id: Set(None),
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

/// Finish a discoverable login. The user is identified from the assertion's
/// `userHandle` (the WebAuthn `user.id` set at registration = our opaque
/// `UserId` UUID). A forged `userHandle` cannot impersonate: the assertion is
/// verified against the resolved user's actual passkey public keys, so a handle
/// with no matching key fails the signature check.
async fn login_discoverable_finish(
    State(state): State<SharedState>,
    Json(req): Json<LoginFinishRequest>,
) -> Result<Response, ApiError> {
    // Rehydrate the discoverable ceremony (read without a lock; the shared tail
    // locks the row for the consume).
    let row = webauthn_challenges::Entity::find_by_id(req.ceremony_id)
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    let row = row.ok_or_else(|| ApiError::not_found("unknown ceremony"))?;
    if row.kind != KIND_LOGIN_DISCOVERABLE {
        return Err(ApiError::bad_request(
            "ceremony is not a discoverable login",
        ));
    }
    if row.expires_at <= OffsetDateTime::now_utc() {
        return Err(ApiError::unauthorized("ceremony expired"));
    }
    let auth_state: DiscoverableAuthentication = serde_json::from_value(row.state)
        .map_err(|e| ApiError::internal(format_args!("deserialize auth state: {e}")))?;

    // Identify the user from the assertion's userHandle. Map a missing/malformed
    // handle to the SAME generic 401 as an unknown/disabled user, so the
    // response leaks nothing about which user handles exist.
    let (user_uuid, cred_id_bytes) = state
        .webauthn
        .identify_discoverable_authentication(&req.credential)
        .map_err(|_| ApiError::unauthorized("invalid credentials"))?;
    let user_id = UserId::from(user_uuid.to_string());
    let raw_id = cred_id_bytes.to_vec();

    // Resolve the user; disabled/unknown -> same generic 401 as login_begin.
    let user = users::Entity::find_by_id(user_id.to_string())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::unauthorized("invalid credentials"))?;
    if user.disabled_at.is_some() {
        return Err(ApiError::unauthorized("invalid credentials"));
    }

    // Load the user's passkeys and project ALL to DiscoverableKey. Legacy
    // non-discoverable passkeys are harmless here -- they never surface via
    // conditional-UI autofill (no resident entry), and the signature check is
    // still owner-scoped.
    let owned = passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .all(&state.db)
        .await
        .map_err(map_db_err)?;
    let creds: Vec<DiscoverableKey> = owned
        .iter()
        .map(|p| serde_json::from_value::<Passkey>(p.credential.clone()))
        .map(|res| res.map(DiscoverableKey::from))
        .collect::<Result<_, _>>()
        .map_err(|e| ApiError::internal(format_args!("deserialize passkey: {e}")))?;

    // Verify the assertion (CPU, outside the txn). Same clone-detection
    // deny+log (never revoke) semantics as login_finish.
    let auth_result = check_login_assertion(
        &user_id,
        &raw_id,
        state
            .webauthn
            .finish_discoverable_authentication(&req.credential, auth_state, &creds),
    )?;

    // Resolve the matching passkey row by cred_id, owner-scoped, then run the
    // shared login-finish tail (advance counter, mint session).
    let passkey_id = resolve_owner_passkey_id(&state.db, &user_id, &raw_id).await?;

    finalize_login_and_mint_session(&state, req.ceremony_id, &user_id, passkey_id, auth_result)
        .await
}

/// Interpret a login assertion result shared by `login_finish` and
/// `login_discoverable_finish`. A counter regression (`CredentialPossible-
/// Compromise`) is DENY + LOG ONLY (never revoke -- see the module doc); any
/// other WebAuthn error maps via `login_err`. Centralized so the two paths
/// cannot drift on the clone-detection policy.
fn check_login_assertion(
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
async fn resolve_owner_passkey_id(
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
async fn finalize_login_and_mint_session(
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

/// Mint a session for `user_id` inside `txn`: generate a fresh `sk-sess-`
/// token, insert the `sessions` row (seeding `authed_at = now` so a freshly
/// signed-in user can immediately add a passkey), and return the `Set-Cookie`
/// header value. Shared by [`register_finish`], [`finalize_login_and_mint_session`],
/// and the OAuth login/create paths. The caller owns the surrounding txn
/// (ceremony lock/delete, disabled recheck, optional passkey advance) and
/// builds the response from the returned header.
pub(crate) async fn mint_session_row(
    txn: &DatabaseTransaction,
    user_id: &UserId,
    session_cfg: &SessionCfg,
    now: OffsetDateTime,
) -> Result<String, DbErr> {
    let session = MintedToken::generate(SESSION);
    let session_hash = session.hash().as_bytes().to_vec();
    let set_cookie = build_set_cookie(session_cfg, session.secret());
    sessions::ActiveModel {
        token_hash: Set(session_hash),
        user_id: Set(user_id.to_string()),
        created_at: Set(now),
        expires_at: Set(now + session_cfg.ttl),
        // A fresh login/signup is the step-up that authorizes adding a device;
        // seed `authed_at` (consumed on use by add-passkey begin).
        authed_at: Set(Some(now)),
    }
    .insert(txn)
    .await?;
    Ok(set_cookie)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A registration failure is a client error (bad/invalid credential). The
/// `WebauthnError` detail is logged but NOT returned to the client — it can
/// distinguish failure modes and so leak why verification failed.
pub(crate) fn register_err(e: WebauthnError) -> ApiError {
    warn!(error = %e, "webauthn register failed");
    ApiError::bad_request("passkey registration failed")
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

/// An authentication failure is 401. `CredentialPossibleCompromise` keeps a
/// distinct message: it is an intentional signal to the legitimate user that
/// their passkey's counter regressed (possible clone). Every other failure gets
/// a generic message; the detail lives only in the log.
fn login_err(e: WebauthnError) -> ApiError {
    warn!(error = %e, "webauthn login failed");
    match e {
        WebauthnError::CredentialPossibleCompromise => {
            ApiError::unauthorized("credential may be cloned")
        }
        _ => ApiError::unauthorized("passkey login failed"),
    }
}

/// Build a JSON response carrying `body` and a `Set-Cookie` header built from
/// `set_cookie`, with the given status. Shared by the session-minting finishes:
/// register/pair pass `CREATED` (a credential + session were created), login
/// passes `OK` (an existing credential was authenticated).
pub(crate) fn set_cookie_response<T: Serialize>(
    set_cookie: &str,
    body: T,
    status: StatusCode,
) -> Response {
    let mut resp = Json(body).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(set_cookie) {
        resp.headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    *resp.status_mut() = status;
    resp
}

#[cfg(test)]
mod tests {
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
}
