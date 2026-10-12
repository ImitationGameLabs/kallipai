//! Discoverable (usernameless) login.

use axum::Json;
use axum::extract::State;
use axum::response::Response;
use kallipai_archeion_common::ids::UserId;
use kallipai_common::protocol::ApiError;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use uuid::Uuid;
use webauthn_rs::prelude::{
    DiscoverableAuthentication, DiscoverableKey, Passkey, RequestChallengeResponse,
};

use crate::db::entity::{passkeys, users, webauthn_challenges};
use crate::db::map_db_err;
use crate::state::SharedState;

use super::login::{
    LoginFinishRequest, check_login_assertion, finalize_login_and_mint_session, login_err,
    resolve_owner_passkey_id,
};
use super::session::CeremonyBeginResponse;
use super::{CHALLENGE_TTL, KIND_LOGIN_DISCOVERABLE};

/// Begin a discoverable login. No identifier is supplied -- the authenticator
/// resolves the user at finish via the assertion's `userHandle`. The wrapper
/// forces conditional mediation + an empty allowList. No per-user in-flight cap
/// is possible (no user handle at begin); the per-IP rate limiter is the gate.
pub(super) async fn login_discoverable_begin(
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
pub(super) async fn login_discoverable_finish(
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
