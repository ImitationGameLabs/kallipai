//! User self-service passkey management + the shared row-level helpers used by
//! both this surface and the admin surface.
//!
//! - `GET /v1/me/passkeys` — list the caller's live passkeys.
//! - `POST /v1/me/passkeys/register/{begin,finish}` — bind ANOTHER passkey to an
//!   EXISTING account (a second-device ceremony, distinct from the invite-gated
//!   initial registration that creates the account). Gated by a one-shot
//!   step-up: the calling session must carry a fresh `authed_at` (set by
//!   login/register finish), which the begin txn consumes — one device per
//!   re-auth.
//! - `PATCH /v1/me/passkeys/{id}` — rename (the device label).
//! - `DELETE /v1/me/passkeys/{id}` — revoke (hard-delete + audit row).
//!
//! The live `passkeys` table is filter-free; revoke deletes the row and appends
//! to `passkey_revocations` (audit + denylist). The last-live-passkey guard
//! prevents self-lockout on the user path; admin (`routes/admin.rs`) reuses the
//! same helper with `allow_last = true`.

use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::{
    external_identities, passkey_revocations, passkeys, sessions, users, webauthn_challenges,
};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::session::read_session_cookie;
use crate::state::SharedState;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch, post};
use kallip_agora_common::admin::PasskeySummary;
use kallip_agora_common::ids::UserId;
use kallip_common::authtoken::TokenHash;
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, DatabaseTransaction, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyRegistration, RegisterPublicKeyCredential,
};
use webauthn_rs_core::proto::RegistrationState;

use super::auth::{CHALLENGE_TTL, CeremonyBeginResponse, KIND_ADD_REGULAR, register_err};

/// Max length of a passkey `label` (after trim). Mirrors `display_name`'s cap:
/// the label is shown in management UI and stored, so an unbounded value is both
/// a UI and a storage concern.
const MAX_LABEL_LEN: usize = 64;

/// How long a fresh step-up (`sessions.authed_at`) authorizes a credential-
/// binding begin (add-passkey or device-pairing mint). Set slightly shorter
/// than [`CHALLENGE_TTL`] so a ceremony can never outlive the step-up that
/// authorized it.
pub(crate) const STEP_UP_WINDOW: time::Duration = time::Duration::seconds(240);

/// Ceremony-kind for the discoverable (resident-key) add-passkey flow. Stored
/// on `webauthn_challenges.kind` so `add_passkey_finish` picks the right state
/// type (the bare core `RegistrationState`, not the wrapper `PasskeyRegistration`)
/// and registers the credential through `WebauthnCore` + `Passkey::from`.
pub(crate) const KIND_ADD_DISCOVERABLE: &str = "add_discoverable";

/// Max live (unexpired) add-passkey ceremonies per user. The step-up is the
/// primary gate (one-shot, so begin-spam needs a fresh re-auth each time, which
/// is itself rate-limited); this is the storage bound for begin-succeeded-but-
/// finish-abandoned rows. Count-then-insert, so soft under true concurrency.
const MAX_INFLIGHT_ADD_CEREMONIES: u64 = 3;

/// Name of the `passkeys.cred_id` unique index (see migration). Matched against
/// the Postgres unique-violation message to map a duplicate-cred_id insert to a
/// clean 409 at add-passkey finish. Shared with `routes::device_pairing`.
pub(crate) const CRED_ID_UNIQUE_CONSTRAINT: &str = "uniq_passkeys_cred_id";

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/me/passkeys", get(list_my_passkeys))
        .route("/me/passkeys/register/begin", post(add_passkey_begin))
        .route("/me/passkeys/register/finish", post(add_passkey_finish))
        .route(
            "/me/passkeys/{id}",
            patch(rename_my_passkey).delete(revoke_my_passkey),
        )
}

// ---------------------------------------------------------------------------
// shared row-level helpers (used by admin + this surface)
// ---------------------------------------------------------------------------

/// All live passkey rows owned by `user_id`, oldest first. The `passkeys` table
/// holds only live credentials, so no status filter is needed.
pub(crate) async fn list_user_passkey_rows(
    db: &crate::db::Db,
    user_id: &UserId,
) -> Result<Vec<passkeys::Model>, ApiError> {
    passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .order_by_asc(passkeys::Column::CreatedAt)
        .all(db)
        .await
        .map_err(map_db_err)
}

/// Project a stored passkey row into the wire summary. Shared by the user + admin
/// list handlers and the rename handler so the projection lives in one place.
pub(crate) fn row_to_summary(r: passkeys::Model) -> PasskeySummary {
    PasskeySummary {
        id: r.id.to_string(),
        label: r.label,
        created_at: r.created_at,
        last_used_at: r.last_used_at,
        discoverable: r.discoverable,
    }
}

/// Consume the one-shot step-up: lock the calling session row `FOR UPDATE`,
/// require `authed_at` is present and within [`STEP_UP_WINDOW`] of `now`, then
/// null it (one fresh UV authorizes exactly one credential-binding). Shared by
/// `add_passkey_begin` and the device-pairing code mint.
pub(crate) async fn consume_step_up(
    txn: &DatabaseTransaction,
    session_hash: &[u8],
    now: OffsetDateTime,
) -> Result<(), TxnError> {
    let session = sessions::Entity::find()
        .filter(sessions::Column::TokenHash.eq(session_hash.to_vec()))
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| TxnError::Api(ApiError::unauthorized("invalid session")))?;
    let Some(authed_at) = session.authed_at else {
        return Err(TxnError::Api(ApiError::forbidden("reauth-required")));
    };
    if now - authed_at > STEP_UP_WINDOW {
        return Err(TxnError::Api(ApiError::forbidden("reauth-required")));
    }
    let mut am: sessions::ActiveModel = session.into();
    am.authed_at = Set(None);
    am.update(txn).await?;
    Ok(())
}

/// Which enrollment flavor a bound credential is: a regular (server-stored,
/// non-resident) passkey or a discoverable (resident-key) one. Carried into
/// `bind_passkey_to_user` so call sites read as a named intent rather than a
/// positional `bool` (which reads as a mystery flag at cross-module callers).
#[derive(Clone, Copy)]
pub(crate) enum CredentialFlavor {
    Regular,
    Discoverable,
}

impl CredentialFlavor {
    fn is_discoverable(&self) -> bool {
        matches!(self, Self::Discoverable)
    }
}

/// Bind a verified credential to an existing user: denylist the cred_id against
/// `passkey_revocations`, then insert a `passkeys` row. Shared by
/// `add_passkey_finish` (session + step-up) and `device_pairing_finish` (pairing
/// code). The caller verifies the credential (CPU, outside the txn), normalizes
/// the label, and maps the unique-violation → 409 at the call site; this helper
/// owns only the denylist + insert.
///
/// The denylist check is advisory: it refuses to re-bind a cred_id that was
/// previously revoked (a sane authenticator honors `excludeCredentials` and
/// never re-issues one, so this is the normal path). The hard backstop against a
/// duplicate live credential is the `uniq_passkeys_cred_id` index — a revoked
/// cred_id has no live row, so the unique constraint does not fire for it; the
/// denylist query above is the only gate for that narrow re-issue case.
pub(crate) async fn bind_passkey_to_user(
    txn: &DatabaseTransaction,
    user_id: &UserId,
    cred_id: Vec<u8>,
    credential_json: serde_json::Value,
    label: String,
    flavor: CredentialFlavor,
) -> Result<(), TxnError> {
    // Denylist: refuse to re-bind a previously-revoked cred_id.
    let revoked = passkey_revocations::Entity::find()
        .filter(passkey_revocations::Column::UserId.eq(user_id.to_string()))
        .filter(passkey_revocations::Column::CredId.eq(cred_id.clone()))
        .one(txn)
        .await?;
    if revoked.is_some() {
        return Err(TxnError::Api(ApiError::conflict(
            "credential previously revoked",
        )));
    }
    let now = OffsetDateTime::now_utc();
    passkeys::ActiveModel {
        id: Set(Uuid::new_v4()),
        user_id: Set(user_id.to_string()),
        cred_id: Set(cred_id),
        credential: Set(credential_json),
        label: Set(label),
        created_at: Set(now),
        last_used_at: Set(now),
        discoverable: Set(flavor.is_discoverable()),
    }
    .insert(txn)
    .await?;
    Ok(())
}

/// Revoke (hard-delete) one of `user_id`'s live passkeys and append a
/// `passkey_revocations` audit row, in one `FOR UPDATE` transaction over the
/// owner's whole live set (so the last-live guard is race-free).
///
/// `allow_last = false` enforces the last-live-passkey guard (user path —
/// prevents self-lockout); admin passes `true` to force-revoke. A target that is
/// not in the owner's live set (already revoked concurrently, or not theirs) is
/// a silent no-op — `Ok(())` — so the caller returns an idempotent 204 without
/// leaking whether the id ever existed.
pub(crate) async fn revoke_passkey_row(
    state: &SharedState,
    user_id: &UserId,
    passkey_id: Uuid,
    allow_last: bool,
    reason: &str,
    revoked_by: &str,
) -> Result<(), ApiError> {
    let user_id_str = user_id.to_string();
    let reason = reason.to_string();
    let revoked_by = revoked_by.to_string();
    let outcome = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let user_id_str = user_id_str.clone();
            let reason = reason.clone();
            let revoked_by = revoked_by.clone();
            Box::pin(async move {
                // Lock the owner's whole live set. Two concurrent revokes of the
                // two last passkeys serialize here: the loser re-counts under
                // the lock and hits the guard.
                let live: Vec<passkeys::Model> = passkeys::Entity::find()
                    .filter(passkeys::Column::UserId.eq(user_id_str.clone()))
                    .lock_exclusive()
                    .all(txn)
                    .await?;
                let Some(target) = live.iter().find(|r| r.id == passkey_id) else {
                    // Not live: already revoked or unknown. Idempotent no-op.
                    return Ok(());
                };
                if !allow_last && live.len() <= 1 {
                    // Symmetric last-sign-in-method guard: removing the last
                    // passkey is allowed only if the account still has an
                    // external identity to sign in with. Lock
                    // external_identities AFTER passkeys (the fixed order
                    // `oauth::unlink_external_identity` also uses) so two
                    // concurrent last-pair removals -- one of each kind --
                    // cannot both pass and leave the account credentialless.
                    let identities = external_identities::Entity::find()
                        .filter(external_identities::Column::UserId.eq(user_id_str.clone()))
                        .lock_exclusive()
                        .all(txn)
                        .await?;
                    if identities.is_empty() {
                        return Err(TxnError::Api(ApiError::conflict(
                            "cannot remove the last sign-in method",
                        )));
                    }
                }
                let cred_id = target.cred_id.clone();
                let now = OffsetDateTime::now_utc();
                passkeys::Entity::delete_by_id(passkey_id).exec(txn).await?;
                passkey_revocations::ActiveModel {
                    id: Set(Uuid::new_v4()),
                    user_id: Set(user_id_str),
                    cred_id: Set(cred_id),
                    reason: Set(reason),
                    revoked_by: Set(revoked_by),
                    revoked_at: Set(now),
                }
                .insert(txn)
                .await?;
                Ok(())
            })
        })
        .await;
    flatten_txn(outcome)
}

// ---------------------------------------------------------------------------
// GET /v1/me/passkeys
// ---------------------------------------------------------------------------

async fn list_my_passkeys(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<Vec<PasskeySummary>>, ApiError> {
    let user_id = require_user(&principal)?;
    let rows = list_user_passkey_rows(&state.db, user_id).await?;
    let items = rows.into_iter().map(row_to_summary).collect();
    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// add passkey (authenticated second-device ceremony)
// ---------------------------------------------------------------------------

async fn add_passkey_begin(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    headers: HeaderMap,
    Query(params): Query<AddPasskeyBeginParams>,
) -> Result<Json<CeremonyBeginResponse<CreationChallengeResponse>>, ApiError> {
    let discoverable = params.discoverable.unwrap_or(false);
    let user_id = require_user(&principal)?;
    let user = users::Entity::find_by_id(user_id.to_string())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown user"))?;
    // Fast-fail a user disabled between the extractor and this handler, so a
    // disabled account does not consume a one-shot step-up or open a ceremony.
    // The authoritative disabled-check is under the lock in `add_passkey_finish`
    // (mirrors login/register: begin screens, finish re-checks).
    if user.disabled_at.is_some() {
        return Err(ApiError::forbidden("account disabled"));
    }

    // Resolve the calling session: its `authed_at` is the step-up marker. The
    // extractor already proved the cookie is valid; we re-hash it to lock the
    // exact session row inside the begin txn.
    let cookie = read_session_cookie(&headers)
        .ok_or_else(|| ApiError::internal("session cookie missing on authenticated request"))?;
    let session_hash = TokenHash::of(&cookie).as_bytes().to_vec();

    // Build excludeCredentials from the user's existing live cred_ids. The table
    // is live-only, so "all" == "live". This is a UX/authenticator hint; the real
    // anti-duplicate guarantee is `uniq_passkeys_cred_id`.
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
    // The WebAuthn `displayName` MUST be non-empty; fall back to the username.
    let display = match user.display_name.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => user.username.clone(),
    };
    let (options, state_value, kind) = if discoverable {
        // Discoverable (resident-key) add-passkey: share the signup discoverable
        // challenge builder. State is the BARE core RegistrationState (a different
        // shape from PasskeyRegistration), tagged with its own kind so finish
        // rehydrates the right type.
        let (ccr, rs) = super::auth::discoverable_registration_challenge(
            &state,
            user_uuid,
            &user.username,
            &display,
            Some(exclude),
        )?;
        let sv = serde_json::to_value(&rs)
            .map_err(|e| ApiError::internal(format_args!("serialize reg state: {e}")))?;
        (ccr, sv, KIND_ADD_DISCOVERABLE)
    } else {
        let (ccr, rs) = state
            .webauthn
            .start_passkey_registration(user_uuid, &user.username, &display, Some(exclude))
            .map_err(register_err)?;
        let sv = serde_json::to_value(&rs)
            .map_err(|e| ApiError::internal(format_args!("serialize reg state: {e}")))?;
        (ccr, sv, KIND_ADD_REGULAR)
    };

    let ceremony_id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let user_id_for_txn = user_id.clone();
    let kind = kind.to_string();
    // NOTE: unlike `register_begin` (which inserts the challenge row outside any
    // txn), this begin wraps the challenge insert in a transaction because the
    // step-up MUST be consumed atomically with the insert -- otherwise a crash
    // between consuming `authed_at` and persisting the ceremony would leak the
    // one-shot step-up without issuing a challenge. `register_begin` has no such
    // atomic-consume requirement (it only screens the invite; finish consumes it).
    let outcome = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let session_hash = session_hash.clone();
            let state_value = state_value.clone();
            let user_id = user_id_for_txn.clone();
            let kind = kind.clone();
            Box::pin(async move {
                // Step-up: check `authed_at` freshness and CONSUME it (NULL) so
                // one fresh login authorizes exactly one credential-binding.
                consume_step_up(txn, &session_hash, now).await?;

                // Storage bound on in-flight add-passkey ceremonies for this
                // user. Counts BOTH add kinds (discoverable + regular) against
                // one shared budget so a user cannot bypass the cap by
                // alternating kinds.
                let in_flight = webauthn_challenges::Entity::find()
                    .filter(webauthn_challenges::Column::UserId.eq(user_id.to_string()))
                    .filter(
                        Condition::any()
                            .add(webauthn_challenges::Column::Kind.eq(KIND_ADD_REGULAR))
                            .add(webauthn_challenges::Column::Kind.eq(KIND_ADD_DISCOVERABLE)),
                    )
                    .filter(webauthn_challenges::Column::ExpiresAt.gt(now))
                    .count(txn)
                    .await?;
                if in_flight >= MAX_INFLIGHT_ADD_CEREMONIES {
                    return Err(TxnError::Api(ApiError::too_many_requests(
                        "too many in-flight add-passkey ceremonies",
                    )));
                }

                webauthn_challenges::ActiveModel {
                    id: Set(ceremony_id),
                    kind: Set(kind),
                    state: Set(state_value),
                    pairing_code_hash: Set(None),
                    user_id: Set(Some(user_id.to_string())),
                    username: Set(None),
                    expires_at: Set(now + CHALLENGE_TTL),
                    created_at: Set(now),
                }
                .insert(txn)
                .await?;
                Ok(())
            })
        })
        .await;
    flatten_txn(outcome)?;

    Ok(Json(CeremonyBeginResponse {
        ceremony_id: ceremony_id.to_string(),
        options,
    }))
}

/// `?discoverable=true` on `POST /v1/me/passkeys/register/begin` switches the
/// add-passkey ceremony to resident-key (discoverable) registration. Absent =>
/// the regular non-discoverable add-passkey flow (byte-identical to pre-Phase-B).
#[derive(Deserialize)]
struct AddPasskeyBeginParams {
    discoverable: Option<bool>,
}

#[derive(Deserialize)]
struct AddPasskeyFinishRequest {
    ceremony_id: Uuid,
    credential: RegisterPublicKeyCredential,
    /// User-supplied device label, trimmed + capped at finish (the begin txn
    /// never sees it; no `label` column on `webauthn_challenges`).
    label: String,
}

async fn add_passkey_finish(
    State(state): State<SharedState>,
    Json(req): Json<AddPasskeyFinishRequest>,
) -> Result<StatusCode, ApiError> {
    // Rehydrate the ceremony (read without a lock; the txn is the authority).
    let row = webauthn_challenges::Entity::find_by_id(req.ceremony_id)
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown ceremony"))?;
    if row.kind != KIND_ADD_REGULAR && row.kind != KIND_ADD_DISCOVERABLE {
        return Err(ApiError::bad_request("ceremony is not a registration"));
    }
    if row.expires_at <= OffsetDateTime::now_utc() {
        return Err(ApiError::unauthorized("ceremony expired"));
    }
    let user_id = UserId::from(
        row.user_id
            .clone()
            .ok_or_else(|| ApiError::internal(format_args!("add ceremony missing user id")))?,
    );

    // Verify the credential (CPU-bound, outside the txn). The discoverable kind
    // rehydrates the BARE core RegistrationState (a different shape from
    // PasskeyRegistration) and finishes through WebauthnCore; the resulting
    // Credential is wrapped into a Passkey via danger-credential-internals so it
    // lands in the same `passkeys.credential` column and works in both flows.
    let (credential_json, cred_id, flavor) = if row.kind == KIND_ADD_DISCOVERABLE {
        let reg_state: RegistrationState = serde_json::from_value(row.state)
            .map_err(|e| ApiError::internal(format_args!("deserialize reg state: {e}")))?;
        let cred = state
            .webauthn_core
            .register_credential(&req.credential, &reg_state, None)
            .map_err(register_err)?;
        let passkey = Passkey::from(cred);
        let cj = serde_json::to_value(&passkey)
            .map_err(|e| ApiError::internal(format_args!("serialize passkey: {e}")))?;
        let cid = passkey.cred_id().as_slice().to_vec();
        (cj, cid, CredentialFlavor::Discoverable)
    } else {
        let reg_state: PasskeyRegistration = serde_json::from_value(row.state)
            .map_err(|e| ApiError::internal(format_args!("deserialize reg state: {e}")))?;
        let passkey = state
            .webauthn
            .finish_passkey_registration(&req.credential, &reg_state)
            .map_err(register_err)?;
        let cj = serde_json::to_value(&passkey)
            .map_err(|e| ApiError::internal(format_args!("serialize passkey: {e}")))?;
        let cid = passkey.cred_id().as_slice().to_vec();
        (cj, cid, CredentialFlavor::Regular)
    };
    let label = normalize_label(req.label)?;

    let ceremony_id = req.ceremony_id;
    let user_id_for_txn = user_id.clone();
    let outcome = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let credential_json = credential_json.clone();
            let cred_id = cred_id.clone();
            let label = label.clone();
            let user_id = user_id_for_txn.clone();
            Box::pin(async move {
                // Lock the challenge; a parallel finish loses on the row lock.
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

                // Re-check the owner is not disabled, taking a `users` row lock.
                // This narrows the disabled-check race to a single ordering
                // (whoever wins `FOR UPDATE` first proceeds); mirrors
                // `finalize_login_and_mint_session`.
                let user = users::Entity::find_by_id(user_id.to_string())
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::unauthorized("invalid credentials")))?;
                if user.disabled_at.is_some() {
                    return Err(TxnError::Api(ApiError::unauthorized("invalid credentials")));
                }

                // Denylist + insert (shared with device-pairing finish). The
                // unique-violation on cred_id is mapped to a 409 at the call site.
                bind_passkey_to_user(
                    txn,
                    &user_id,
                    cred_id.clone(),
                    credential_json.clone(),
                    label.clone(),
                    flavor,
                )
                .await?;

                webauthn_challenges::Entity::delete_by_id(ceremony_id)
                    .exec(txn)
                    .await?;
                Ok(())
            })
        })
        .await;
    // A duplicate cred_id (an authenticator that ignored `excludeCredentials`
    // and re-issued a live credential id) loses to `uniq_passkeys_cred_id`;
    // surface that as a clean 409 instead of a 500.
    match outcome {
        Ok(()) => Ok(StatusCode::CREATED),
        Err(sea_orm::TransactionError::Transaction(TxnError::Api(e))) => Err(e),
        Err(sea_orm::TransactionError::Transaction(TxnError::Db(e)))
        | Err(sea_orm::TransactionError::Connection(e)) => {
            if let Some(sea_orm::SqlErr::UniqueConstraintViolation(msg)) = e.sql_err()
                && msg.contains(CRED_ID_UNIQUE_CONSTRAINT)
            {
                return Err(ApiError::conflict("credential already registered"));
            }
            Err(map_db_err(e))
        }
    }
}

// ---------------------------------------------------------------------------
// PATCH /v1/me/passkeys/{id}  +  DELETE /v1/me/passkeys/{id}
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RenamePasskeyRequest {
    label: String,
}

async fn rename_my_passkey(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<Uuid>,
    Json(req): Json<RenamePasskeyRequest>,
) -> Result<Json<PasskeySummary>, ApiError> {
    let user_id = require_user(&principal)?;
    let label = normalize_label(req.label)?;
    // Owner-scoped: only touch a row belonging to the caller.
    let row = passkeys::Entity::find()
        .filter(passkeys::Column::Id.eq(id))
        .filter(passkeys::Column::UserId.eq(user_id.to_string()))
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown passkey"))?;
    let mut am: passkeys::ActiveModel = row.into();
    am.label = Set(label);
    let updated = am.update(&state.db).await.map_err(map_db_err)?;
    Ok(Json(row_to_summary(updated)))
}

async fn revoke_my_passkey(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let user_id = require_user(&principal)?;
    revoke_passkey_row(
        &state,
        user_id,
        id,
        false,
        passkey_revocations::REASON_REVOKED,
        passkey_revocations::REVOKED_BY_USER,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Trim and cap a client-supplied label. An empty result is allowed (the UI
/// falls back to a generic name); only an over-length label is rejected. Shared
/// with `routes::device_pairing`.
pub(crate) fn normalize_label(raw: String) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    if trimmed.chars().count() > MAX_LABEL_LEN {
        return Err(ApiError::bad_request(format!(
            "label longer than {MAX_LABEL_LEN} chars"
        )));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests;
