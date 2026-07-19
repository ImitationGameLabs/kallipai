//! WebAuthn passkey registration + login, invite redemption, sessions, and the
//! `/v1/me` profile.
//!
//! # Identity model
//!
//! The **login id is the email** (`users.email`, RFC 5321-faithful canonical
//! form: local part preserved verbatim, domain lowercased — see `crate::email`).
//! `login_begin` resolves the user by email. The **username** is a required,
//! unique in-site display handle chosen at redemption (normalized via
//! `crate::username`); it is NOT the login id. WebAuthn `user.name` is the
//! email; the username surfaces only as the fallback WebAuthn `displayName`
//! (when the client omits `display_name`) and in `/v1/me`. `user.id` stays the
//! opaque pre-generated `UserId`.
//!
//! # Ceremonies
//!
//! - **register** `begin`/`finish`: `begin` validates (but does not consume) an
//!   invite code, canonicalizes the email and normalizes the username,
//!   synthesizes a prompt-only `display_name`, pre-generates the `UserId`, and
//!   persists the `PasskeyRegistration` (+ email + username) on the challenge
//!   row. `finish` verifies the credential (CPU, outside the txn), then in ONE
//!   transaction locks the challenge row `FOR UPDATE`, re-checks the invite's
//!   full live predicate `FOR UPDATE`, checks email AND username uniqueness
//!   `FOR UPDATE` (`409` on conflict), inserts the user + passkey, consumes the
//!   invite, deletes the challenge, and mints a fresh session. A parallel
//!   double-finish on one invite loses on the row lock.
//! - **login** `begin`/`finish`: email-first. `begin` resolves the user by
//!   `email`, loads their passkeys, and bakes them into the ceremony state
//!   via the wrapper's `start_passkey_authentication`. `finish` verifies the
//!   assertion against that baked state (CPU, outside the txn) and advances the
//!   stored passkey via `Passkey::update_credential` inside the SAME transaction
//!   that locks the challenge `FOR UPDATE`, inserts the session, and deletes the
//!   challenge — so a parallel ceremony-id replay loses on the row lock and a
//!   transient failure cannot advance the counter without issuing a session.
//!   `update_credential` returning `None` (cred_id mismatch) is a hard 500, not
//!   a silent skip: it means the row moved under us, and issuing a session
//!   against a stale counter would lose clone detection.
//!
//! Session ids are rotated: every register/login finish mints a brand-new
//! session token (never reuses a pre-login one), defeating session fixation.

use crate::db::entity::{invite_codes, passkeys, sessions, users, webauthn_challenges};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::session::{build_clear_cookie, build_set_cookie, read_session_cookie};
use crate::state::SharedState;
use crate::token::SESSION;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use kallip_agora_common::ids::UserId;
use kallip_common::authtoken::{MintedToken, TokenHash};
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QuerySelect, SqlErr, TransactionError, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;
use webauthn_rs::prelude::{
    AuthenticationResult, CreationChallengeResponse, Passkey, PasskeyAuthentication,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, WebauthnError,
};

use crate::auth::{AuthPrincipal, require_user};
use crate::email;
use crate::username;

/// Ceremony-kind discriminator stored on `webauthn_challenges.kind`.
const KIND_REGISTER: &str = "register";
const KIND_LOGIN: &str = "login";

/// How long an in-flight ceremony remains valid. Browsers prompt the user
/// within this window; a stale challenge is rejected at finish and GC'd at begin.
const CHALLENGE_TTL: Duration = Duration::from_secs(300);

/// Name of the `users.username` unique index (see migration). Matched against
/// the Postgres unique-violation message to discriminate a username-collision
/// race (-> 409) from any other unique violation in the same transaction.
const USERNAME_UNIQUE_CONSTRAINT: &str = "uniq_users_username";

/// Name of the `users.email` unique index; same role as
/// [`USERNAME_UNIQUE_CONSTRAINT`] for the login-id collision race (-> 409).
const EMAIL_UNIQUE_CONSTRAINT: &str = "uniq_users_email";

/// Max live (unexpired) ceremonies per invite (register) / user (login). Bounds
/// `webauthn_challenges` storage growth against an attacker who spams begins;
/// the per-client rate limiter is the primary gate, this is the storage bound.
/// Count-then-insert, so the cap is soft under true concurrency.
const MAX_INFLIGHT_CEREMONIES: u64 = 16;

/// The unauthenticated, crypto-expensive ceremony BEGIN endpoints. These are
/// the invite-enumeration / ceremony-spam entry surface and the only ceremony
/// routes the per-client rate limiter should cover (see `routes::router`). A
/// begin mints the unguessable `ceremony_id` that finish then consumes, so
/// finish is transitively bounded by begin's rate limit and is NOT itself
/// rate-limited (otherwise a login ceremony would cost two tokens).
pub fn begin_router() -> Router<SharedState> {
    Router::new()
        .route("/auth/register/begin", post(register_begin))
        .route("/auth/login/begin", post(login_begin))
}

/// The ceremony FINISH endpoints. Not rate-limited: each requires a real,
/// unguessable, single-use `ceremony_id` issued by a (rate-limited) begin, so
/// the verification surface here is bounded by begin's limiter.
pub fn finish_router() -> Router<SharedState> {
    Router::new()
        .route("/auth/register/finish", post(register_finish))
        .route("/auth/login/finish", post(login_finish))
}

/// The cookie-authenticated session surface (no rate limiting).
pub fn session_router() -> Router<SharedState> {
    Router::new()
        .route("/auth/logout", post(logout))
        .route("/me", get(me))
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RegisterBeginRequest {
    invite_code: String,
    email: String,
    username: String,
    display_name: Option<String>,
}

#[derive(Serialize)]
struct CeremonyBeginResponse<T: Serialize> {
    ceremony_id: String,
    options: T,
}

#[derive(Deserialize)]
struct RegisterFinishRequest {
    ceremony_id: Uuid,
    credential: RegisterPublicKeyCredential,
}

#[derive(Deserialize)]
struct LoginBeginRequest {
    email: String,
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

#[derive(Serialize)]
struct MeResponse {
    user_id: String,
    username: String,
    email: String,
    display_name: Option<String>,
    created_at: OffsetDateTime,
    passkey_count: i64,
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
    // Canonicalize the email (login id) and normalize the username (in-site
    // handle) once; the same transforms run at login_begin so a user can log in
    // with exactly the address they registered.
    let email_norm = email::normalize(&req.email)?;
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

    let code_hash = TokenHash::of(&req.invite_code);
    let code_hash_bytes = code_hash.as_bytes().to_vec();

    // Validate the invite is live WITHOUT consuming it. The finish txn is the
    // authority; this only screens so a bogus code fails fast.
    let now = OffsetDateTime::now_utc();
    let live = invite_codes::Entity::find()
        .filter(invite_codes::Column::CodeHash.eq(code_hash_bytes.clone()))
        .filter(invite_codes::Column::ConsumedAt.is_null())
        .filter(invite_codes::Column::RevokedAt.is_null())
        .filter(invite_codes::Column::ExpiresAt.gt(now))
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    if live.is_none() {
        // Same message for unknown / consumed / revoked / expired so the
        // response leaks nothing about which.
        return Err(ApiError::unauthorized("invalid invite code"));
    }

    // Bound concurrent in-flight register ceremonies for this invite so a
    // begin-flood cannot grow the table without limit. Only live (unexpired)
    // rows count: the background GC may not have swept expired ones yet.
    let in_flight = webauthn_challenges::Entity::find()
        .filter(webauthn_challenges::Column::InviteCodeHash.eq(code_hash_bytes.clone()))
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
    // The wrapper hardcodes require_resident_key=false + UV=Required (upstream
    // passkey defaults); email-first login does not rely on discoverability.
    // `user.name` is the email; the username is purely an in-site handle and
    // reaches the authenticator only as the `displayName` fallback (see
    // `display_name_for_prompt` above) when no `display_name` was supplied.
    let (options, reg_state) = state
        .webauthn
        .start_passkey_registration(user_uuid, &email_norm, &display_name_for_prompt, None)
        .map_err(register_err)?;
    let state_value = serde_json::to_value(&reg_state)
        .map_err(|e| ApiError::internal(format_args!("serialize reg state: {e}")))?;

    let ceremony_id = Uuid::new_v4();
    webauthn_challenges::ActiveModel {
        id: Set(ceremony_id),
        kind: Set(KIND_REGISTER.to_string()),
        state: Set(state_value),
        invite_code_hash: Set(Some(code_hash_bytes)),
        user_id: Set(Some(user_id.to_string())),
        email: Set(Some(email_norm)),
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
    // Rehydrate the ceremony state and run the (CPU-bound) registration
    // verification OUTSIDE the transaction so the row locks are not held across
    // crypto.
    let (reg_state, invite_hash, user_id, email, username) =
        load_register_state(&state.db, req.ceremony_id).await?;
    let passkey = state
        .webauthn
        .finish_passkey_registration(&req.credential, &reg_state)
        .map_err(register_err)?;

    let session = MintedToken::generate(SESSION);
    let session_hash = session.hash().as_bytes().to_vec();
    let set_cookie = build_set_cookie(state.session_cfg, session.secret());
    let session_ttl = state.session_cfg.ttl;

    // One transaction: lock challenge -> invite -> username, insert user +
    // passkey, consume the invite, delete the challenge, mint the session.
    let ceremony_id = req.ceremony_id;
    let credential_json = serde_json::to_value(&passkey)
        .map_err(|e| ApiError::internal(format_args!("serialize passkey: {e}")))?;
    let cred_id = passkey.cred_id().as_slice().to_vec();
    let user_id_for_txn = user_id.clone();
    let result = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let invite_hash = invite_hash.clone();
            let user_id = user_id_for_txn.clone();
            let email = email.clone();
            let username = username.clone();
            let credential_json = credential_json.clone();
            let cred_id = cred_id.clone();
            let session_hash = session_hash.clone();
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

                // Lock the invite and re-check the FULL live predicate under the
                // row lock (defeats a parallel consume race).
                let invite = invite_codes::Entity::find()
                    .filter(invite_codes::Column::CodeHash.eq(invite_hash.clone()))
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                let Some(invite) = invite else {
                    return Err(TxnError::Api(ApiError::unauthorized("invalid invite code")));
                };
                let now = OffsetDateTime::now_utc();
                if invite.consumed_at.is_some() {
                    warn!("invite redeemed while already consumed");
                    return Err(TxnError::Api(ApiError::conflict(
                        "invite code already used",
                    )));
                }
                if invite.revoked_at.is_some() {
                    return Err(TxnError::Api(ApiError::conflict("invite code revoked")));
                }
                if invite.expires_at <= now {
                    return Err(TxnError::Api(ApiError::conflict("invite code expired")));
                }

                // Email (login id) uniqueness: FOR UPDATE-then-check so a taken
                // address maps to a clean 409. The `uniq_users_email` index is
                // the backstop for the sub-ms simultaneous-insert race.
                let existing_email = users::Entity::find()
                    .filter(users::Column::Email.eq(email.clone()))
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                if existing_email.is_some() {
                    return Err(TxnError::Api(ApiError::conflict(
                        "email already registered",
                    )));
                }

                // Username (in-site handle) uniqueness: same FOR UPDATE-then-check
                // shape; the `uniq_users_username` index is the race backstop.
                let existing = users::Entity::find()
                    .filter(users::Column::Username.eq(username.clone()))
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                if existing.is_some() {
                    return Err(TxnError::Api(ApiError::conflict("username already taken")));
                }

                users::ActiveModel {
                    id: Set(user_id.to_string()),
                    username: Set(username),
                    email: Set(email),
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
                    created_at: Set(now),
                    compromised_at: Set(None),
                }
                .insert(txn)
                .await?;

                let mut invite_am: invite_codes::ActiveModel = invite.into();
                invite_am.consumed_at = Set(Some(now));
                invite_am.consumed_by = Set(Some(user_id.to_string()));
                invite_am.update(txn).await?;

                webauthn_challenges::Entity::delete_by_id(ceremony_id)
                    .exec(txn)
                    .await?;

                sessions::ActiveModel {
                    token_hash: Set(session_hash),
                    user_id: Set(user_id.to_string()),
                    created_at: Set(now),
                    expires_at: Set(now + session_ttl),
                }
                .insert(txn)
                .await?;
                Ok(())
            })
        })
        .await;
    // Flatten the transaction result. The `users` insert can still race a
    // parallel register of the same email or username (the FOR UPDATE pre-checks
    // above win the common case; the sub-ms simultaneous-insert case loses to
    // the `uniq_users_email` / `uniq_users_username` index). Discriminate those
    // unique-constraint violations by constraint name and surface each as a
    // clean 409 with the right message instead of a 500; any other unique
    // violation (e.g. a duplicate passkey cred_id, which is never legitimate)
    // stays a generic 500 via map_db_err.
    match result {
        Ok(()) => {}
        Err(TransactionError::Transaction(TxnError::Api(e))) => return Err(e),
        Err(TransactionError::Transaction(TxnError::Db(e)))
        | Err(TransactionError::Connection(e)) => {
            if let Some(SqlErr::UniqueConstraintViolation(msg)) = e.sql_err() {
                if msg.contains(EMAIL_UNIQUE_CONSTRAINT) {
                    return Err(ApiError::conflict("email already registered"));
                }
                if msg.contains(USERNAME_UNIQUE_CONSTRAINT) {
                    return Err(ApiError::conflict("username already taken"));
                }
            }
            return Err(map_db_err(e));
        }
    }

    Ok(set_cookie_response(
        &set_cookie,
        AuthFinishResponse {
            user_id: user_id.to_string(),
        },
    ))
}

/// Read a register ceremony, rehydrate its `PasskeyRegistration`, and return
/// the bound invite hash, the pre-generated `UserId`, the canonicalized email,
/// and the chosen username. Errors if the ceremony is missing, expired, or not
/// a register ceremony.
async fn load_register_state(
    db: &crate::db::Db,
    ceremony_id: Uuid,
) -> Result<(PasskeyRegistration, Vec<u8>, UserId, String, String), ApiError> {
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
    let invite_hash = row
        .invite_code_hash
        .ok_or_else(|| ApiError::internal(format_args!("register ceremony missing invite hash")))?;
    let user_id = row
        .user_id
        .clone()
        .ok_or_else(|| ApiError::internal(format_args!("register ceremony missing user id")))?;
    let email = row
        .email
        .clone()
        .ok_or_else(|| ApiError::internal(format_args!("register ceremony missing email")))?;
    let username = row
        .username
        .clone()
        .ok_or_else(|| ApiError::internal(format_args!("register ceremony missing username")))?;
    let state: PasskeyRegistration = serde_json::from_value(row.state)
        .map_err(|e| ApiError::internal(format_args!("deserialize reg state: {e}")))?;
    Ok((state, invite_hash, UserId::from(user_id), email, username))
}

// ---------------------------------------------------------------------------
// login (email-first)
// ---------------------------------------------------------------------------

async fn login_begin(
    State(state): State<SharedState>,
    Json(req): Json<LoginBeginRequest>,
) -> Result<Json<CeremonyBeginResponse<RequestChallengeResponse>>, ApiError> {
    let email_norm = email::normalize(&req.email)?;

    // Resolve the user by email (the login id). NOTE: this is a timing-
    // enumeration oracle -- an unknown email (or a user with no passkeys)
    // returns immediately, while a known email with passkeys pays the cost of
    // loading them + `start_passkey_authentication` (real crypto-state
    // construction) + an INSERT, so existence is distinguishable by latency.
    // The same generic "invalid credentials" body is used for all 401 branches
    // so the response BODY leaks nothing. Accepted for closed beta (emails are
    // personally issued via invite). The pre-public-launch fix is constant-time
    // / dummy-ceremony work, not message parity alone.
    let user = users::Entity::find()
        .filter(users::Column::Email.eq(email_norm))
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

    // Load the user's live passkeys (a compromised passkey is excluded: once
    // the library reports a counter regression it must not authenticate). The
    // wrapper bakes them into the ceremony state so finish verifies the
    // assertion against the right public keys.
    let owned = passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .filter(passkeys::Column::CompromisedAt.is_null())
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
        invite_code_hash: Set(None),
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

    // The authenticated credential's id is the one in `req.credential.raw_id`.
    // Extract it up front so the compromise path below can mark the right row.
    let raw_id = req.credential.raw_id.as_slice().to_vec();

    // Verify the assertion (CPU-bound, outside the txn). The state already
    // carries the user's passkeys (baked at begin), so no set_allowed_credentials
    // reach-through is needed. The wrapper enforces the signature-counter clone
    // check (returns CredentialPossibleCompromise on a regression). Note: clone
    // detection only fires for authenticators that maintain a non-zero monotonic
    // counter; synced/software passkeys report counter == 0 and never trigger it.
    let auth_result: AuthenticationResult = match state
        .webauthn
        .finish_passkey_authentication(&req.credential, &auth_state)
    {
        Ok(r) => r,
        Err(WebauthnError::CredentialPossibleCompromise) => {
            // A counter regression means this credential may have been cloned.
            // Mark it compromised (idempotent conditional UPDATE) so it can no
            // longer authenticate, then reject. The user must re-register.
            let now = OffsetDateTime::now_utc();
            passkeys::Entity::update_many()
                .filter(passkeys::Column::CredId.eq(raw_id.clone()))
                .filter(passkeys::Column::CompromisedAt.is_null())
                .col_expr(
                    passkeys::Column::CompromisedAt,
                    sea_orm::sea_query::Expr::value(now),
                )
                .exec(&state.db)
                .await
                .map_err(map_db_err)?;
            warn!("passkey possibly cloned; marked compromised and disabled");
            return Err(ApiError::unauthorized("credential may be cloned"));
        }
        Err(e) => return Err(login_err(e)),
    };

    // Resolve the matching passkey row by credential id. Needed to advance its
    // stored counter inside the txn.
    let passkey_id = passkeys::Entity::find()
        .filter(passkeys::Column::CredId.eq(raw_id))
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::unauthorized("unknown credential"))?
        .id;

    // Mint the session token up front; its hash is inserted inside the txn.
    let session = MintedToken::generate(SESSION);
    let session_hash = session.hash().as_bytes().to_vec();
    let set_cookie = build_set_cookie(state.session_cfg, session.secret());
    let ceremony_id = req.ceremony_id;
    let user_id_for_txn = user_id.clone();
    let session_ttl = state.session_cfg.ttl;

    // One transaction: lock the challenge row FOR UPDATE (defeats a parallel
    // replay of the same ceremony_id: the loser sees the row gone -> 409),
    // advance the stored passkey under the lock (no lost-update), insert the
    // session, and delete the challenge. All-or-nothing so a transient failure
    // cannot advance the counter without issuing a session.
    let outcome = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let user_id = user_id_for_txn.clone();
            let session_hash = session_hash.clone();
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

                // Re-check the owning user is not disabled under the lock: a
                // user disabled between begin and finish must not mint a session
                // even though they began legitimately. Same generic message as
                // the begin-path check.
                let user = users::Entity::find_by_id(user_id.to_string())
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::unauthorized("invalid credentials")))?;
                if user.disabled_at.is_some() {
                    return Err(TxnError::Api(ApiError::unauthorized("invalid credentials")));
                }

                // Re-read the passkey under the lock and advance it via the
                // library helper. `None` means the cred_id no longer matches the
                // authenticated credential (the row moved under us) -- a HARD
                // error, not a silent skip: issuing a session against a
                // stale/wrong counter would lose clone detection. `Some(false)`
                // = nothing changed (most passkeys); skip the write.
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
                match stored.update_credential(&auth_result) {
                    Some(true) => {
                        let updated_json = serde_json::to_value(&stored).map_err(|e| {
                            TxnError::Db(DbErr::Custom(format!("serialize passkey: {e}")))
                        })?;
                        let mut am: passkeys::ActiveModel = current.into();
                        am.credential = Set(updated_json);
                        am.update(txn).await?;
                    }
                    Some(false) => {}
                    None => {
                        return Err(TxnError::Api(ApiError::internal(
                            "credential id mismatch on login finish",
                        )));
                    }
                }

                let now = OffsetDateTime::now_utc();
                sessions::ActiveModel {
                    token_hash: Set(session_hash),
                    user_id: Set(user_id.to_string()),
                    created_at: Set(now),
                    expires_at: Set(now + session_ttl),
                }
                .insert(txn)
                .await?;

                webauthn_challenges::Entity::delete_by_id(ceremony_id)
                    .exec(txn)
                    .await?;
                Ok(())
            })
        })
        .await;
    flatten_txn(outcome)?;

    Ok(set_cookie_response(
        &set_cookie,
        AuthFinishResponse {
            user_id: user_id.to_string(),
        },
    ))
}

// ---------------------------------------------------------------------------
// logout + /v1/me
// ---------------------------------------------------------------------------

async fn logout(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    // Require a signed-in user (cookie) so an anonymous token cannot force a
    // cookie clear. The actual session row deletion is best-effort keyed by the
    // presented cookie hash.
    require_user(&principal)?;
    let Some(cookie_value) = read_session_cookie(&headers) else {
        return Err(ApiError::unauthorized("no session"));
    };
    let hash = TokenHash::of(&cookie_value);
    sessions::Entity::delete_by_id(hash.as_bytes().to_vec())
        .exec(&state.db)
        .await
        .map_err(map_db_err)?;
    let clear = build_clear_cookie(state.session_cfg);
    let mut resp = StatusCode::OK.into_response();
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&clear)
            .map_err(|e| ApiError::internal(format_args!("bad set-cookie: {e}")))?,
    );
    Ok(resp)
}

async fn me(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<MeResponse>, ApiError> {
    let user_id = require_user(&principal)?;
    let user = users::Entity::find_by_id(user_id.to_string())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown user"))?;
    let passkey_count = passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .count(&state.db)
        .await
        .map_err(map_db_err)? as i64;
    Ok(Json(MeResponse {
        user_id: user_id.to_string(),
        display_name: user.display_name,
        username: user.username,
        email: user.email,
        created_at: user.created_at,
        passkey_count,
    }))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A registration failure is a client error (bad/invalid credential). The
/// `WebauthnError` detail is logged but NOT returned to the client — it can
/// distinguish failure modes and so leak why verification failed.
fn register_err(e: WebauthnError) -> ApiError {
    warn!(error = %e, "webauthn register failed");
    ApiError::bad_request("passkey registration failed")
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

/// Build a `200 OK` JSON response carrying `body` and a `Set-Cookie` header
/// built from `set_cookie`. Used by register/login finish to mint the session.
fn set_cookie_response<T: Serialize>(set_cookie: &str, body: T) -> Response {
    let mut resp = Json(body).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(set_cookie) {
        resp.headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    resp
}

#[cfg(test)]
mod tests {
    //! Handler-level tests for the auth glue that does NOT require a virtual
    //! authenticator: invite screening, ceremony begin, session-bearing
    //! `/v1/me`, and ceremony GC. The WebAuthn crypto ceremonies themselves are
    //! exercised end-to-end by the browser (a unit-level virtual authenticator
    //! would have to re-implement CTAP signing, which `webauthn-rs` does not
    //! ship).

    use std::time::Duration;

    use axum::Json;
    use axum::extract::State;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};
    use time::OffsetDateTime;

    use super::{
        LoginBeginRequest, MeResponse, RegisterBeginRequest, login_begin, me, register_begin,
    };
    use crate::auth::{AuthPrincipal, Principal};
    use crate::db::entity::{invite_codes, users, webauthn_challenges};
    use crate::test_helpers::{make_state, seed_user};
    use crate::token::INVITE;
    use kallip_common::authtoken::MintedToken;
    use sea_orm::EntityTrait;

    /// Insert a live invite code whose plaintext is `token`, returning its hash.
    async fn seed_invite(state: &crate::state::SharedState, token: &MintedToken) -> Vec<u8> {
        let now = OffsetDateTime::now_utc();
        let hash = token.hash().as_bytes().to_vec();
        invite_codes::ActiveModel {
            code_hash: Set(hash.clone()),
            created_at: Set(now),
            expires_at: Set(now + time::Duration::days(7)),
            consumed_at: Set(None),
            consumed_by: Set(None),
            note: Set(None),
            revoked_at: Set(None),
        }
        .insert(&state.db)
        .await
        .expect("insert invite");
        hash
    }

    /// An unknown invite is rejected at `register_begin` with 401 (and the
    /// message reveals nothing about whether the code exists).
    #[tokio::test]
    async fn register_begin_rejects_unknown_invite() {
        let state = make_state(Duration::from_secs(2)).await;
        match register_begin(
            State(state),
            Json(RegisterBeginRequest {
                invite_code: "sk-invite-bogus".to_string(),
                email: "someone@example.test".to_string(),
                username: "someone".to_string(),
                display_name: None,
            }),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 401),
            Ok(_) => panic!("unknown invite must be rejected"),
        }
    }

    /// A live invite produces a ceremony id + WebAuthn options, and persists the
    /// challenge row bound to the invite hash.
    #[tokio::test]
    async fn register_begin_accepts_live_invite() {
        let state = make_state(Duration::from_secs(2)).await;
        let token = MintedToken::generate(INVITE);
        let hash = seed_invite(&state, &token).await;

        let resp = register_begin(
            State(state.clone()),
            Json(RegisterBeginRequest {
                invite_code: token.secret().to_string(),
                email: "NewUser@Example.TEST".to_string(),
                username: "newuser".to_string(),
                display_name: None,
            }),
        )
        .await
        .expect("begin ok");
        assert!(!resp.ceremony_id.is_empty());

        // The persisted challenge carries the invite hash + a register kind.
        let rows = webauthn_challenges::Entity::find()
            .all(&state.db)
            .await
            .expect("read challenges");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "register");
        assert_eq!(rows[0].invite_code_hash.as_deref(), Some(hash.as_slice()));
        // The canonical email (local preserved, domain lowercased) rides the
        // challenge row for finish; the username rides too.
        assert_eq!(rows[0].email.as_deref(), Some("NewUser@example.test"));
        assert_eq!(rows[0].username.as_deref(), Some("newuser"));
    }

    /// `/v1/me` returns the signed-in user's profile.
    #[tokio::test]
    async fn me_returns_signed_in_user() {
        let state = make_state(Duration::from_secs(2)).await;
        let user_id = seed_user(&state, "alice", "alice@example.test").await;
        let Json(MeResponse {
            user_id: got,
            username,
            email,
            display_name,
            passkey_count,
            ..
        }) = me(
            State(state),
            AuthPrincipal(Principal::User(user_id.clone())),
        )
        .await
        .expect("me ok");
        assert_eq!(got, user_id.to_string());
        assert_eq!(username, "alice");
        assert_eq!(email, "alice@example.test");
        assert_eq!(display_name, None);
        assert_eq!(passkey_count, 0);
    }

    /// `login_begin` rejects an unknown email with 401 (accepted enumeration
    /// oracle for closed beta; see the handler doc comment).
    #[tokio::test]
    async fn login_begin_rejects_unknown_email() {
        let state = make_state(Duration::from_secs(2)).await;
        match login_begin(
            State(state),
            Json(LoginBeginRequest {
                email: "nobody@example.test".to_string(),
            }),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 401),
            Ok(_) => panic!("unknown email must be rejected"),
        }
    }

    /// A disabled account cannot start a login: same 401 as an unknown user,
    /// so the response leaks no account state.
    #[tokio::test]
    async fn login_begin_rejects_disabled_user() {
        let state = make_state(Duration::from_secs(2)).await;
        let user_id = seed_user(&state, "frozen", "frozen@example.test").await;
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
                email: "frozen@example.test".to_string(),
            }),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 401),
            Ok(_) => panic!("disabled user must be rejected"),
        }
    }

    /// `register_begin` refuses once the per-invite in-flight ceremony cap is
    /// reached, bounding `webauthn_challenges` growth against a begin flood.
    #[tokio::test]
    async fn register_begin_caps_inflight_ceremonies() {
        let state = make_state(Duration::from_secs(2)).await;
        let token = MintedToken::generate(INVITE);
        let hash = seed_invite(&state, &token).await;
        let now = OffsetDateTime::now_utc();
        // Seed exactly the cap of live register ceremonies for this invite.
        for _ in 0..super::MAX_INFLIGHT_CEREMONIES {
            webauthn_challenges::ActiveModel {
                id: Set(uuid::Uuid::new_v4()),
                kind: Set("register".to_string()),
                state: Set(serde_json::Value::Null),
                invite_code_hash: Set(Some(hash.clone())),
                user_id: Set(None),
                email: Set(None),
                username: Set(None),
                expires_at: Set(now + time::Duration::seconds(60)),
                created_at: Set(now),
            }
            .insert(&state.db)
            .await
            .expect("seed challenge");
        }
        // The next begin for the same invite is rejected with 429.
        match register_begin(
            State(state),
            Json(RegisterBeginRequest {
                invite_code: token.secret().to_string(),
                email: "newuser@example.test".to_string(),
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
}
