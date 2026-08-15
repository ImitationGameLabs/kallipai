//! OAuth (GitHub, Google) begin/finish + linked-identity management.
//!
//! Authorization-Code flow with the agora as the confidential client. The SPA
//! calls `begin` (gets an `authorize_url`), navigates there, the provider
//! redirects back to the SPA callback with `code`+`state`, and the SPA posts
//! them to `finish`. The agora exchanges the code (server-side secret + PKCE),
//! resolves `(provider, subject)`, and either logs in (existing identity),
//! begins signup (a new identity: the resolved claim is held and the SPA is
//! sent a signup token to submit a chosen username at `signup/complete`), or
//! links (an already-signed-in account binds the identity). See `crate::oauth`
//! for the provider seam.
//!
//! Security invariants (verified in tests via a mock provider):
//! - `state` is a random CSRF token; only its SHA-256 is stored, it is
//!   single-use, and it is bound to `provider` so a github state cannot redeem
//!   at the google path. For a linked identity the row is deleted on finish;
//!   for an UNLINKED identity it is transitioned exactly once to "held"
//!   (resolved claim + signup token) and a second finish on the held row is
//!   rejected. The held row is deleted at signup completion.
//! - OAuth signup NEVER merges by email and NEVER touches the `emails` table;
//!   the provider email is display-only.
//! - `signup_enabled` is enforced only at signup completion (the create step):
//!   at begin/finish we cannot tell signup from login, so an earlier check
//!   would lock out existing OAuth users. Note finish DOES distinguish a
//!   linked identity (200 login) from an unlinked one (202 needs-username)
//!   regardless of `signup_enabled`; this is an accepted existence leak,
//!   since probing requires a real provider `code` for the subject (the
//!   attacker controls that subject at the provider -- not cheaply
//!   enumerable).
//! - The last-sign-in-method guard is symmetric and race-free: both this
//!   module's unlink and `passkeys::revoke_passkey_row` lock `passkeys` THEN
//!   `external_identities` for the user (fixed order), so two concurrent
//!   revokes of the last pair cannot leave the account credentialless.

use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::{external_identities, oauth_states, users};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::oauth::{self, OAuthProvider, ProviderId, ProviderInfo, sanitize_return_path};
use crate::routes::auth::{CHALLENGE_TTL, mint_session_row, set_cookie_response};
use crate::routes::passkeys::consume_step_up;
use crate::session::read_session_cookie;
use crate::state::SharedState;
use crate::token::OAUTH_SIGNUP;
use crate::username;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use kallip_agora_common::ids::UserId;
use kallip_common::authtoken::{MintedToken, TokenHash};
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, DbErr, EntityTrait,
    QueryFilter, QuerySelect, SqlErr, TransactionError, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::warn;

/// Re-export the action constants stored on `oauth_states.action`.
pub(crate) use crate::db::entity::oauth_states::{ACTION_LINK, ACTION_SIGNIN};

/// Constraint names matched against the Postgres unique-violation message to
/// turn a simultaneous-signup race (the username probe loses because
/// `SELECT ... FOR UPDATE` gap-locks nothing on a non-existent row) into a
/// clean 409 rather than a 500 that leaks the constraint name.
const USERNAME_UNIQUE_CONSTRAINT: &str = "uniq_users_username";
const IDENTITY_UNIQUE_CONSTRAINT: &str = "uniq_external_identities_provider_subject";

/// Whether `e` is a unique-violation on `external_identities (provider, subject)`.
/// Pins the constraint-name coupling in one place; both OAuth finish paths use it
/// to turn a simultaneous-signup/link race into a clean 409.
fn is_identity_unique_violation(e: &DbErr) -> bool {
    matches!(
        e.sql_err(),
        Some(SqlErr::UniqueConstraintViolation(msg)) if msg.contains(IDENTITY_UNIQUE_CONSTRAINT)
    )
}

/// The unauthenticated discovery endpoint (which providers are enabled). For
/// the login/settings UI. Rate-limited at the router assembly.
pub fn public_router() -> Router<SharedState> {
    Router::new().route("/auth/oauth/providers", get(list_providers))
}

/// The signin ceremony begin (unauthenticated; rate-limited at assembly).
pub fn begin_router() -> Router<SharedState> {
    Router::new().route("/auth/oauth/{provider}/begin", post(signin_begin))
}

/// The ceremony finish (not rate-limited; bounded by the single-use state).
pub fn finish_router() -> Router<SharedState> {
    Router::new()
        .route("/auth/oauth/{provider}/finish", post(finish))
        // Completes a held signup: the SPA submits the chosen username against
        // the opaque token issued at finish. Provider-less -- `(provider,
        // subject)` is recovered from the held claim, never trusted from the
        // client. Bounded by the single-use token, so not rate-limited.
        .route("/auth/oauth/signup/complete", post(signup_complete))
}

/// The cookie-authenticated surface: link a provider (step-up gated, like
/// add-passkey) and unlink an identity (last-method guarded). Not rate-limited.
pub fn session_router() -> Router<SharedState> {
    Router::new()
        .route("/me/oauth/{provider}/begin", post(link_begin))
        .route(
            "/me/external-identities/{id}",
            delete(unlink_external_identity),
        )
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BeginBody {
    #[serde(default)]
    return_path: Option<String>,
}

#[derive(Serialize)]
struct BeginResponse {
    authorize_url: String,
}

#[derive(Deserialize)]
struct FinishBody {
    state: String,
    code: String,
}

#[derive(Serialize)]
struct FinishResponse {
    user_id: String,
    /// The sanitized return path the SPA should resume to (echoed back so the
    /// callback page does not have to remember it across the provider redirect).
    #[serde(skip_serializing_if = "Option::is_none")]
    return_path: Option<String>,
}

/// finish outcome for an unlinked identity: the resolved claim is held on the
/// ceremony row and the SPA must submit a chosen username (with this token) at
/// `signup/complete`. HTTP 202 so the body's `kind` is unambiguous vs a 200
/// login.
#[derive(Serialize)]
struct NeedsUsernameResponse {
    kind: &'static str,
    signup_token: String,
    provider: String,
}

#[derive(Deserialize)]
struct SignupCompleteBody {
    signup_token: String,
    username: String,
}

/// A linked external identity as returned by `/v1/me`. Omits `subject` (the
/// provider's stable id is not a secret but a list view has no need for it).
#[derive(Serialize, Clone, Debug)]
pub(crate) struct ExternalIdentitySummary {
    pub id: String,
    pub provider: String,
    pub display_name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
}

impl From<external_identities::Model> for ExternalIdentitySummary {
    fn from(m: external_identities::Model) -> Self {
        Self {
            id: m.id.to_string(),
            provider: m.provider,
            display_name: m.display_name,
            created_at: m.created_at,
            last_used_at: m.last_used_at,
        }
    }
}

// ---------------------------------------------------------------------------
// discovery
// ---------------------------------------------------------------------------

async fn list_providers(State(state): State<SharedState>) -> Json<Vec<ProviderInfo>> {
    Json(state.oauth_providers.list())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Resolve a path segment to a configured provider, or 404 (unknown id OR a
/// known id that this deploy did not enable). The returned reference is tied to
/// `state` (the registry lives on `AppState`).
fn resolve_provider<'s>(
    state: &'s SharedState,
    provider: &str,
) -> Result<&'s dyn OAuthProvider, ApiError> {
    let id =
        ProviderId::from_path(provider).ok_or_else(|| ApiError::not_found("unknown provider"))?;
    state
        .oauth_providers
        .get(id)
        .ok_or_else(|| ApiError::not_found("provider not enabled"))
}

/// The freshly generated OAuth ceremony seed: the `state` plaintext (+ its
/// hash for storage) and, when the provider supports PKCE, the verifier/
/// challenge pair. The plaintext + challenge build the authorize URL; the hash
/// + verifier ride the ceremony row.
struct CeremonyInit {
    state_plain: String,
    state_hash: Vec<u8>,
    pkce_verifier: Option<String>,
    pkce_challenge: Option<String>,
}

/// Generate a [`CeremonyInit`] for `provider`.
fn generate_state_and_pkce(provider: &dyn OAuthProvider) -> Result<CeremonyInit, ApiError> {
    let state_plain = oauth::generate_state().map_err(oauth_fatal)?;
    let state_hash = TokenHash::of(&state_plain).as_bytes().to_vec();
    let (pkce_verifier, pkce_challenge) = if provider.supports_pkce() {
        let (v, c) = oauth::generate_pkce().map_err(oauth_fatal)?;
        (Some(v), Some(c))
    } else {
        (None, None)
    };
    Ok(CeremonyInit {
        state_plain,
        state_hash,
        pkce_verifier,
        pkce_challenge,
    })
}

/// Lock the ceremony state row FOR UPDATE, reject missing/expired, delete it
/// (single-use). Returns the consumed row (its `user_id` carries the link
/// target). A racing finish that consumed it first sees `None` -> 401.
async fn consume_state(
    txn: &DatabaseTransaction,
    state_hash: &[u8],
) -> Result<oauth_states::Model, TxnError> {
    let locked = oauth_states::Entity::find_by_id(state_hash.to_vec())
        .lock_exclusive()
        .one(txn)
        .await?;
    let row = locked.ok_or_else(|| TxnError::Api(ApiError::unauthorized("invalid state")))?;
    if row.expires_at <= OffsetDateTime::now_utc() {
        return Err(TxnError::Api(ApiError::unauthorized("invalid state")));
    }
    oauth_states::Entity::delete_by_id(state_hash.to_vec())
        .exec(txn)
        .await?;
    Ok(row)
}

/// Map an OAuth round-trip failure to a generic server error (the detail is
/// logged, never sent to the client -- OAuth failures must not leak provider
/// specifics or which step failed).
fn oauth_fatal(e: oauth::OAuthError) -> ApiError {
    warn!(error = %e, "oauth provider round-trip failed");
    ApiError::internal("oauth provider unavailable")
}

// ---------------------------------------------------------------------------
// signin begin (unauthenticated)
// ---------------------------------------------------------------------------

async fn signin_begin(
    State(state): State<SharedState>,
    Path(provider): Path<String>,
    Json(body): Json<BeginBody>,
) -> Result<Json<BeginResponse>, ApiError> {
    // signup_enabled is intentionally NOT checked here: at begin we cannot tell
    // signup from login (the subject is unknown until after the provider
    // round-trip), so a begin-time check would lock out existing OAuth users.
    // It is enforced at signup completion (signup/complete), the create step.
    let provider_handle = resolve_provider(&state, &provider)?;
    let CeremonyInit {
        state_plain,
        state_hash,
        pkce_verifier,
        pkce_challenge,
    } = generate_state_and_pkce(provider_handle)?;
    let authorize_url = provider_handle
        .authorize_url(&state_plain, pkce_challenge.as_deref())
        .to_string();
    let now = OffsetDateTime::now_utc();
    oauth_states::ActiveModel {
        state_hash: Set(state_hash),
        provider: Set(provider_handle.id().as_str().to_string()),
        action: Set(ACTION_SIGNIN.to_string()),
        return_path: Set(sanitize_return_path(body.return_path.as_deref())),
        user_id: Set(None),
        pkce_verifier: Set(pkce_verifier),
        subject: Set(None),
        claim_display_name: Set(None),
        signup_token_hash: Set(None),
        created_at: Set(now),
        expires_at: Set(now + CHALLENGE_TTL),
    }
    .insert(&state.db)
    .await
    .map_err(map_db_err)?;
    Ok(Json(BeginResponse { authorize_url }))
}

// ---------------------------------------------------------------------------
// link begin (cookie-authed, step-up gated)
// ---------------------------------------------------------------------------

async fn link_begin(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    headers: axum::http::HeaderMap,
    Path(provider): Path<String>,
    Json(body): Json<BeginBody>,
) -> Result<Json<BeginResponse>, ApiError> {
    let user_id = require_user(&principal)?;
    let provider_handle = resolve_provider(&state, &provider)?;
    // Link requires PKCE: the verifier stored on the ceremony row is the only
    // thing cryptographically binding the exchanged `code` to this ceremony, so
    // a non-PKCE provider would let a stolen link `state` + an attacker's own
    // `code` bind the attacker's identity to THIS account (takeover). Signin is
    // not affected (it never binds to a pre-existing user). Both shipped
    // providers support PKCE; this gates any future non-PKCE provider.
    if !provider_handle.supports_pkce() {
        return Err(ApiError::bad_request(
            "provider does not support secure account linking",
        ));
    }
    // Generate the state + PKCE OUTSIDE the txn (the closure must not capture
    // `state`; and the verifier/challenge derive the authorize URL, which is
    // built here too).
    let CeremonyInit {
        state_plain,
        state_hash,
        pkce_verifier,
        pkce_challenge,
    } = generate_state_and_pkce(provider_handle)?;
    let authorize_url = provider_handle
        .authorize_url(&state_plain, pkce_challenge.as_deref())
        .to_string();
    let cookie = read_session_cookie(&headers)
        .ok_or_else(|| ApiError::internal("session cookie missing on authenticated request"))?;
    let session_hash = TokenHash::of(&cookie).as_bytes().to_vec();

    // The step-up MUST be consumed atomically with the ceremony insert: a crash
    // between consuming `authed_at` and persisting the state would leak the
    // one-shot step-up without issuing a ceremony (mirrors add-passkey begin).
    let now = OffsetDateTime::now_utc();
    let user_id_for_txn = user_id.clone();
    let provider_id = provider_handle.id();
    let return_path = sanitize_return_path(body.return_path.as_deref());
    let pkce_for_txn = pkce_verifier.clone();
    let outcome = state
        .db
        .transaction::<_, (), TxnError>(|txn| {
            let user_id = user_id_for_txn.clone();
            let return_path = return_path.clone();
            let pkce = pkce_for_txn.clone();
            Box::pin(async move {
                consume_step_up(txn, &session_hash, now).await?;
                oauth_states::ActiveModel {
                    state_hash: Set(state_hash.clone()),
                    provider: Set(provider_id.as_str().to_string()),
                    action: Set(ACTION_LINK.to_string()),
                    return_path: Set(return_path),
                    user_id: Set(Some(user_id.to_string())),
                    pkce_verifier: Set(pkce),
                    subject: Set(None),
                    claim_display_name: Set(None),
                    signup_token_hash: Set(None),
                    created_at: Set(now),
                    expires_at: Set(now + CHALLENGE_TTL),
                }
                .insert(txn)
                .await?;
                Ok(())
            })
        })
        .await;
    flatten_txn(outcome)?;
    Ok(Json(BeginResponse { authorize_url }))
}

// ---------------------------------------------------------------------------
// finish (login | signup | link)
// ---------------------------------------------------------------------------

async fn finish(
    State(state): State<SharedState>,
    Path(provider_str): Path<String>,
    Json(body): Json<FinishBody>,
) -> Result<Response, ApiError> {
    let provider_handle = resolve_provider(&state, &provider_str)?;

    // Peek the ceremony row (read-only) for the action, bound user, and PKCE
    // verifier. The single-use consume happens under the lock in the txn below;
    // a racing finish that consumed it first is caught there.
    let state_hash = TokenHash::of(&body.state).as_bytes().to_vec();
    let row = oauth_states::Entity::find_by_id(state_hash.clone())
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    let row = row.ok_or_else(|| ApiError::unauthorized("invalid state"))?;
    if row.expires_at <= OffsetDateTime::now_utc() {
        return Err(ApiError::unauthorized("invalid state"));
    }
    // Bind: a state issued for one provider cannot redeem at another's path.
    if row.provider != provider_handle.id().as_str() {
        return Err(ApiError::unauthorized("invalid state"));
    }
    // Fast path: a held row (signup_token_hash set by a prior unlinked finish)
    // is no longer redeemable. Reject here to skip the provider round-trip; the
    // authoritative re-check is the held-state guard inside finish_signin.
    if row.signup_token_hash.is_some() {
        return Err(ApiError::unauthorized("invalid state"));
    }

    // Provider round-trip OUTSIDE the txn (network; no row locks held).
    let tokens = provider_handle
        .exchange(&body.code, row.pkce_verifier.as_deref(), &state.http)
        .await
        .map_err(oauth_fatal)?;
    let identity = provider_handle
        .fetch_identity(&tokens, &state.http)
        .await
        .map_err(oauth_fatal)?;

    let provider_id = provider_handle.id();
    let session_cfg = state.session_cfg.clone();
    let return_path = row.return_path.clone();

    match row.action.as_str() {
        ACTION_SIGNIN => {
            finish_signin(
                &state,
                state_hash,
                provider_id,
                identity,
                session_cfg,
                return_path,
            )
            .await
        }
        ACTION_LINK => finish_link(&state, state_hash, provider_id, identity).await,
        other => Err(ApiError::internal(format_args!(
            "unknown oauth action: {other}"
        ))),
    }
}

/// signin: log in an existing linked account, or begin signup for a new one.
///
/// Decided inside the txn (lock `(provider, subject)` -> present = login,
/// absent = hold the claim). A held claim is NOT an account: the resolved
/// identity is written onto the ceremony row with a single-use signup token,
/// and the SPA must submit a chosen username at `signup/complete` (which is
/// where creation + the `signup_enabled` gate live). Returns 200 (login) or
/// 202 (needs-username).
async fn finish_signin(
    state: &SharedState,
    state_hash: Vec<u8>,
    provider: ProviderId,
    identity: oauth::ProviderIdentity,
    session_cfg: crate::session::SessionCfg,
    return_path: Option<String>,
) -> Result<Response, ApiError> {
    /// The txn outcome: a minted login session, or a held claim awaiting a
    /// username (carrying the opaque signup token plaintext for the SPA).
    enum SignInOutcome {
        Login { set_cookie: String, user_id: UserId },
        NeedsUsername { signup_token: String },
    }
    let outcome = state
        .db
        .transaction::<_, SignInOutcome, TxnError>(|txn| {
            let subject = identity.subject.clone();
            let display_name = identity.display_name.clone();
            let session_cfg = session_cfg.clone();
            Box::pin(async move {
                // Lock the ceremony row FOR UPDATE + expiry check. Unlike
                // `consume_state` (link), the unlinked arm keeps the row, so
                // lock here and branch on delete-vs-hold.
                let row = oauth_states::Entity::find_by_id(state_hash.clone())
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::unauthorized("invalid state")))?;
                if row.expires_at <= OffsetDateTime::now_utc() {
                    return Err(TxnError::Api(ApiError::unauthorized("invalid state")));
                }
                // Held-state guard: a signup_token_hash is set only once finish
                // transitioned this row to held. A second finish on the same
                // `state` (browser back / double-submit) must NOT mint a fresh
                // token and overwrite it -- the `state` is single-use.
                if row.signup_token_hash.is_some() {
                    return Err(TxnError::Api(ApiError::unauthorized("invalid state")));
                }

                let existing = external_identities::Entity::find()
                    .filter(external_identities::Column::Provider.eq(provider.as_str()))
                    .filter(external_identities::Column::Subject.eq(subject.clone()))
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                let now = OffsetDateTime::now_utc();

                if let Some(id_row) = existing {
                    // LOGIN. Reject a disabled owner with the same generic 401
                    // as passkey login; never leak which. Lock the `users` row so
                    // a concurrent admin disable cannot commit between this read
                    // and the session mint (lock order: external_identities ->
                    // users, matched by `finish_link`).
                    let user = users::Entity::find_by_id(id_row.user_id.clone())
                        .lock_exclusive()
                        .one(txn)
                        .await?
                        .ok_or_else(|| {
                            TxnError::Api(ApiError::unauthorized("invalid credentials"))
                        })?;
                    if user.disabled_at.is_some() {
                        return Err(TxnError::Api(ApiError::unauthorized("invalid credentials")));
                    }
                    let user_id = UserId::from(user.id);
                    let mut am: external_identities::ActiveModel = id_row.into();
                    am.last_used_at = Set(Some(now));
                    am.update(txn).await?;
                    // Single-use: delete the ceremony row on login.
                    oauth_states::Entity::delete_by_id(state_hash.clone())
                        .exec(txn)
                        .await?;
                    let set_cookie = mint_session_row(txn, &user_id, &session_cfg, now).await?;
                    return Ok(SignInOutcome::Login {
                        set_cookie,
                        user_id,
                    });
                }

                // UNLINKED -> hold the claim for the username step. The provider
                // display_name is display-only (never stored in `emails`, never
                // used to merge); it rides the row to `signup/complete`.
                let token = MintedToken::generate(OAUTH_SIGNUP);
                let token_hash = token.hash().as_bytes().to_vec();
                let mut am: oauth_states::ActiveModel = row.into();
                am.subject = Set(Some(subject));
                am.claim_display_name = Set(display_name);
                am.signup_token_hash = Set(Some(token_hash));
                // Refresh the TTL so the user has a full window to type a
                // username; the row GCs if completion never comes.
                am.expires_at = Set(now + CHALLENGE_TTL);
                am.update(txn).await?;
                Ok(SignInOutcome::NeedsUsername {
                    signup_token: token.secret().to_string(),
                })
            })
        })
        .await;
    match outcome {
        Ok(SignInOutcome::Login {
            set_cookie,
            user_id,
        }) => Ok(set_cookie_response(
            &set_cookie,
            FinishResponse {
                user_id: user_id.to_string(),
                return_path,
            },
            StatusCode::OK,
        )),
        Ok(SignInOutcome::NeedsUsername { signup_token }) => {
            let body = NeedsUsernameResponse {
                kind: "needs-username",
                signup_token,
                provider: provider.as_str().to_string(),
            };
            let mut resp = Json(body).into_response();
            *resp.status_mut() = StatusCode::ACCEPTED;
            Ok(resp)
        }
        Err(TransactionError::Transaction(TxnError::Api(e))) => Err(e),
        Err(TransactionError::Transaction(TxnError::Db(e)))
        | Err(TransactionError::Connection(e)) => Err(map_db_err(e)),
    }
}

/// Complete a held signup: validate the opaque token, then either create the
/// account (chosen username) or, if a race linked the identity in the window,
/// sign the user in. Account creation -- and thus the `signup_enabled` gate --
/// happens only here. Lock order `oauth_states -> external_identities -> users`
/// matches `finish_signin`/`finish_link`. The provider is recovered from the
/// held row (set server-side at finish) and is NOT re-resolved via the registry:
/// the provider round-trip already happened at finish, and re-checking here
/// would wrongly fail a complete if the operator disabled the provider in the
/// window (the identity is already verified).
async fn signup_complete(
    State(state): State<SharedState>,
    Json(body): Json<SignupCompleteBody>,
) -> Result<Response, ApiError> {
    let signup_token_hash = TokenHash::of(&body.signup_token).as_bytes().to_vec();
    let session_cfg = state.session_cfg.clone();
    let signup_enabled = state.signup_enabled;
    enum CompleteOutcome {
        SignIn {
            set_cookie: String,
            user_id: UserId,
            return_path: Option<String>,
        },
        SignUp {
            set_cookie: String,
            user_id: UserId,
            return_path: Option<String>,
        },
    }
    let outcome = state
        .db
        .transaction::<_, CompleteOutcome, TxnError>(|txn| {
            let username_raw = body.username.clone();
            Box::pin(async move {
                let row = oauth_states::Entity::find()
                    .filter(oauth_states::Column::SignupTokenHash.eq(signup_token_hash.clone()))
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| {
                        TxnError::Api(ApiError::unauthorized("invalid or expired signup"))
                    })?;
                if row.expires_at <= OffsetDateTime::now_utc() {
                    return Err(TxnError::Api(ApiError::unauthorized(
                        "invalid or expired signup",
                    )));
                }
                // A held row is always a signin ceremony; assert it fail-closed.
                if row.action != ACTION_SIGNIN {
                    return Err(TxnError::Api(ApiError::internal(
                        "signup token on non-signin ceremony",
                    )));
                }
                let provider = ProviderId::from_path(&row.provider).ok_or_else(|| {
                    TxnError::Api(ApiError::internal("held signup with unknown provider"))
                })?;
                let subject = row.subject.clone().ok_or_else(|| {
                    TxnError::Api(ApiError::internal("held signup missing subject"))
                })?;
                let claim_display_name = row.claim_display_name.clone();
                let return_path = row.return_path.clone();
                let now = OffsetDateTime::now_utc();

                // Re-lock `(provider, subject)`: another finish/link could have
                // bound this identity between finish and complete. If so, just
                // sign the user in (no duplicate).
                if let Some(id_row) = external_identities::Entity::find()
                    .filter(external_identities::Column::Provider.eq(provider.as_str()))
                    .filter(external_identities::Column::Subject.eq(subject.clone()))
                    .lock_exclusive()
                    .one(txn)
                    .await?
                {
                    let user = users::Entity::find_by_id(id_row.user_id.clone())
                        .lock_exclusive()
                        .one(txn)
                        .await?
                        .ok_or_else(|| {
                            TxnError::Api(ApiError::unauthorized("invalid credentials"))
                        })?;
                    if user.disabled_at.is_some() {
                        return Err(TxnError::Api(ApiError::unauthorized("invalid credentials")));
                    }
                    let user_id = UserId::from(user.id);
                    let mut am: external_identities::ActiveModel = id_row.into();
                    am.last_used_at = Set(Some(now));
                    am.update(txn).await?;
                    oauth_states::Entity::delete_by_id(row.state_hash.clone())
                        .exec(txn)
                        .await?;
                    let set_cookie = mint_session_row(txn, &user_id, &session_cfg, now).await?;
                    return Ok(CompleteOutcome::SignIn {
                        set_cookie,
                        user_id,
                        return_path,
                    });
                }

                // Account creation -- the only place `signup_enabled` is
                // enforced. No enumeration concern: the token is opaque and
                // single-use.
                if !signup_enabled {
                    return Err(TxnError::Api(ApiError::forbidden("signup is disabled")));
                }
                let username = username::normalize(&username_raw)
                    .map_err(|e| TxnError::Api(ApiError::from(e)))?;
                // Probe uniqueness; the `uniq_users_username` index backstops a
                // simultaneous-signup race the probe loses (SELECT FOR UPDATE
                // gap-locks nothing on a non-existent row).
                let taken = users::Entity::find()
                    .filter(users::Column::Username.eq(&username))
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .is_some();
                if taken {
                    return Err(TxnError::Api(ApiError::conflict("username already taken")));
                }
                let user_id = UserId::random();
                users::ActiveModel {
                    id: Set(user_id.to_string()),
                    username: Set(username),
                    display_name: Set(None),
                    created_at: Set(now),
                    disabled_at: Set(None),
                }
                .insert(txn)
                .await?;
                external_identities::ActiveModel {
                    id: Set(uuid::Uuid::new_v4()),
                    user_id: Set(user_id.to_string()),
                    provider: Set(provider.as_str().to_string()),
                    subject: Set(subject),
                    display_name: Set(claim_display_name),
                    created_at: Set(now),
                    last_used_at: Set(Some(now)),
                }
                .insert(txn)
                .await?;
                oauth_states::Entity::delete_by_id(row.state_hash.clone())
                    .exec(txn)
                    .await?;
                let set_cookie = mint_session_row(txn, &user_id, &session_cfg, now).await?;
                Ok(CompleteOutcome::SignUp {
                    set_cookie,
                    user_id,
                    return_path,
                })
            })
        })
        .await;
    let out = match outcome {
        Ok(o) => o,
        Err(TransactionError::Transaction(TxnError::Api(e))) => return Err(e),
        Err(TransactionError::Transaction(TxnError::Db(e)))
        | Err(TransactionError::Connection(e)) => {
            // A simultaneous-signup race surfaces as a unique-constraint
            // violation; discriminate by constraint name -> clean 409 (retry).
            if is_identity_unique_violation(&e) {
                return Err(ApiError::conflict(
                    "identity already linked; retry to sign in",
                ));
            }
            if let Some(SqlErr::UniqueConstraintViolation(msg)) = e.sql_err()
                && msg.contains(USERNAME_UNIQUE_CONSTRAINT)
            {
                return Err(ApiError::conflict("username already taken"));
            }
            return Err(map_db_err(e));
        }
    };
    let (set_cookie, user_id, return_path, status) = match out {
        CompleteOutcome::SignIn {
            set_cookie,
            user_id,
            return_path,
        } => (set_cookie, user_id, return_path, StatusCode::OK),
        CompleteOutcome::SignUp {
            set_cookie,
            user_id,
            return_path,
        } => (set_cookie, user_id, return_path, StatusCode::CREATED),
    };
    Ok(set_cookie_response(
        &set_cookie,
        FinishResponse {
            user_id: user_id.to_string(),
            return_path,
        },
        status,
    ))
}

/// link: bind `(provider, subject)` to the account captured at link begin.
/// 409 if the subject is already linked to a different account; idempotent if
/// already linked to THIS account. No session is minted (the user is already
/// signed in).
async fn finish_link(
    state: &SharedState,
    state_hash: Vec<u8>,
    provider: ProviderId,
    identity: oauth::ProviderIdentity,
) -> Result<Response, ApiError> {
    let outcome = state
        .db
        .transaction::<_, (), TxnError>(|txn| {
            let subject = identity.subject.clone();
            let display_name = identity.display_name.clone();
            Box::pin(async move {
                let row = consume_state(txn, &state_hash).await?;
                let target = row.user_id.ok_or_else(|| {
                    TxnError::Api(ApiError::internal("link ceremony missing bound user"))
                })?;
                // Lock the (provider, subject) identity row BEFORE the `users`
                // row. `finish_signin` and `signup_complete` lock in the same
                // order (external_identities -> users); matching them here keeps
                // the OAuth finish paths deadlock-free if two run for the same
                // user.
                // 409 if the subject is already linked to another account;
                // idempotent no-op if already linked to this one.
                if let Some(existing) = external_identities::Entity::find()
                    .filter(external_identities::Column::Provider.eq(provider.as_str()))
                    .filter(external_identities::Column::Subject.eq(subject.clone()))
                    .lock_exclusive()
                    .one(txn)
                    .await?
                {
                    if existing.user_id == target {
                        // Idempotent: already linked to this account. Still sync
                        // a non-empty display_name (a provider-side rename), so a
                        // re-link does not leave the stored name stale. A
                        // transient `None` must NOT erase an existing name.
                        if let Some(name) = display_name.clone() {
                            let mut am: external_identities::ActiveModel = existing.into();
                            am.display_name = Set(Some(name));
                            am.update(txn).await?;
                        }
                        return Ok(());
                    }
                    return Err(TxnError::Api(ApiError::conflict(
                        "provider already linked to another account",
                    )));
                }
                // Reject a disabled target, locking the `users` row so a
                // concurrent admin disable cannot commit before the identity is
                // bound. Lock order: external_identities -> users.
                let user = users::Entity::find_by_id(target.clone())
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::unauthorized("invalid credentials")))?;
                if user.disabled_at.is_some() {
                    return Err(TxnError::Api(ApiError::unauthorized("invalid credentials")));
                }
                let now = OffsetDateTime::now_utc();
                external_identities::ActiveModel {
                    id: Set(uuid::Uuid::new_v4()),
                    user_id: Set(target),
                    provider: Set(provider.as_str().to_string()),
                    subject: Set(subject),
                    display_name: Set(display_name),
                    created_at: Set(now),
                    last_used_at: Set(None),
                }
                .insert(txn)
                .await?;
                Ok(())
            })
        })
        .await;
    // A simultaneous-link race (two finishes for the same novel (provider,
    // subject): SELECT FOR UPDATE gap-locks nothing on a non-existent row)
    // surfaces as a unique-constraint violation. Discriminate the constraint
    // name -> clean 409 instead of leaking it through a generic 500. Mirrors
    // `finish_signin` (link inserts no users row, so only the identity
    // constraint can fire).
    match outcome {
        Ok(()) => {}
        Err(TransactionError::Transaction(TxnError::Api(e))) => return Err(e),
        Err(TransactionError::Transaction(TxnError::Db(e)))
        | Err(TransactionError::Connection(e)) => {
            // Only the identity constraint can fire (link inserts no users row);
            // a retry binds idempotently (same account) or 409s (another).
            if is_identity_unique_violation(&e) {
                return Err(ApiError::conflict("identity already linked; retry to link"));
            }
            return Err(map_db_err(e));
        }
    };
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// unlink (cookie-authed, last-method guarded)
// ---------------------------------------------------------------------------

async fn unlink_external_identity(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<uuid::Uuid>,
) -> Result<StatusCode, ApiError> {
    let user_id = require_user(&principal)?;
    let user_id_str = user_id.to_string();
    let outcome = state
        .db
        .transaction::<_, bool, TxnError>(|txn| {
            let user_id_str = user_id_str.clone();
            Box::pin(async move {
                // Fixed lock order passkeys -> external_identities (matches
                // revoke_passkey_row) so concurrent unlink + passkey-revoke of
                // the last pair cannot deadlock nor both pass the guard.
                let live_passkeys = crate::db::entity::passkeys::Entity::find()
                    .filter(crate::db::entity::passkeys::Column::UserId.eq(user_id_str.clone()))
                    .lock_exclusive()
                    .all(txn)
                    .await?;
                let identities = external_identities::Entity::find()
                    .filter(external_identities::Column::UserId.eq(user_id_str.clone()))
                    .lock_exclusive()
                    .all(txn)
                    .await?;
                // Idempotent: a missing (already-unlinked) id is a silent 204.
                let Some(_target) = identities.iter().find(|i| i.id == id) else {
                    return Ok(false);
                };
                // Last-method guard: removing this identity must leave at least
                // one sign-in method (a passkey or another identity).
                if identities.len() == 1 && live_passkeys.is_empty() {
                    return Err(TxnError::Api(ApiError::conflict(
                        "cannot remove the last sign-in method",
                    )));
                }
                external_identities::Entity::delete_by_id(id)
                    .exec(txn)
                    .await?;
                Ok(true)
            })
        })
        .await;
    flatten_txn(outcome)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests;
