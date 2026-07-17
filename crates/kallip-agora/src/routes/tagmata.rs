//! Tagma enrollment + key distribution.
//!
//! `POST /v1/tagmata` redeems a single-use enrollment token (bound to a user)
//! for a long-lived tagma token, pinning the herald's device public key. The
//! herald must sign the enrollment transcript with the matching private key
//! (proof of possession), so a stolen token alone cannot pin an attacker-chosen
//! key. The token is consumed atomically in one Postgres transaction that locks
//! the enrollment-token row `FOR UPDATE` and re-checks the full live predicate
//! (not consumed / not revoked / not expired), so a concurrent redeem race is
//! rejected (first wins).
//!
//! `GET /v1/tagmata/{id}` serves the pinned key to the owning user (TOFU with
//! change-detection on the app side).

use crate::db::entity::{enrollment_tokens, tagma_tokens, tagmata, users};
use crate::db::{TxnError, flatten_txn, map_db_err};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control::{EnrollRequest, EnrollResponse};
use kallip_agora_common::ids::TagmaId;
use kallip_agora_common::proof::{ProofError, verify_enroll_proof};
use kallip_common::authtoken::{MintedToken, TokenHash};
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QuerySelect,
    TransactionTrait,
};
use serde::Serialize;
use time::OffsetDateTime;
use tracing::warn;

use crate::auth::{AuthPrincipal, require_user};
use crate::state::SharedState;
use crate::token::TAGMA;

/// Expected length of an Ed25519 public key, enforced at the enrollment
/// boundary (the wire newtype carries bytes without a length check).
const ED25519_PUBLIC_KEY_LEN: usize = 32;

/// The unauthenticated enroll surface: redeem an enrollment token for a tagma
/// token. Rate-limited at the mount site (it is CPU- and DB-expensive and
/// reachable by anyone holding a code).
pub fn enroll_router() -> Router<SharedState> {
    Router::new().route("/tagmata", post(enroll))
}

/// The authenticated tagma surface: owned-tagma key lookup. Cookie/bearer-authed
/// and not rate-limited (must not share the unauth bucket, same reasoning as
/// `/me`).
pub fn protected_router() -> Router<SharedState> {
    Router::new().route("/tagmata/{id}", get(get_tagma))
}

async fn enroll(
    State(state): State<SharedState>,
    Json(req): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, ApiError> {
    if req.device_public_key.0.len() != ED25519_PUBLIC_KEY_LEN {
        return Err(ApiError::bad_request(
            "device public key must be 32 bytes (Ed25519)",
        ));
    }

    // Proof of possession: the signature must verify against the key being
    // pinned. Done before opening the transaction so a bad-proof attempt does
    // not consume the token. Pure CPU, outside any DB row lock.
    verify_enroll_proof(&req.device_public_key.0, &req.code, &req.signature.0)
        .map_err(proof_to_bad_request)?;

    let code_hash = TokenHash::of(&req.code);
    let tagma_id = TagmaId::random();
    let tagma_token = MintedToken::generate(TAGMA);
    let tagma_token_plaintext = tagma_token.secret().to_string();
    let tagma_token_hash = tagma_token.hash().as_bytes().to_vec();
    let device_key = req.device_public_key.0.clone();
    let enroll_tagma_id = tagma_id.clone();

    // One transaction: lock the enrollment-token row FOR UPDATE, re-check the
    // full live predicate (defeats a parallel redeem race — the second txn
    // blocks until the first commits, then sees consumed_at set), insert the
    // tagma + tagma token, and consume the token.
    let result = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let code_hash = code_hash.as_bytes().to_vec();
            let tagma_id = enroll_tagma_id.clone();
            let tagma_token_hash = tagma_token_hash.clone();
            let device_key = device_key.clone();
            Box::pin(async move {
                let token = enrollment_tokens::Entity::find()
                    .filter(enrollment_tokens::Column::TokenHash.eq(code_hash))
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                let token = match token {
                    None => {
                        return Err(TxnError::Api(ApiError::unauthorized(
                            "invalid enrollment code",
                        )));
                    }
                    Some(t) => t,
                };
                let now = OffsetDateTime::now_utc();
                if token.consumed_at.is_some() {
                    warn!("enrollment token redeemed while already consumed");
                    return Err(TxnError::Api(ApiError::conflict(
                        "enrollment code already used",
                    )));
                }
                if token.revoked_at.is_some() {
                    return Err(TxnError::Api(ApiError::conflict("enrollment code revoked")));
                }
                if token.expires_at <= now {
                    return Err(TxnError::Api(ApiError::conflict("enrollment code expired")));
                }
                let owner = token.user_id.clone();

                // A disabled account cannot enroll a device key. Re-check the
                // owner under a row lock for a race-free read against a
                // concurrent disable. Same message as an invalid code so the
                // response leaks nothing.
                let owner_row = users::Entity::find_by_id(owner.clone())
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| {
                        TxnError::Api(ApiError::unauthorized("invalid enrollment code"))
                    })?;
                if owner_row.disabled_at.is_some() {
                    return Err(TxnError::Api(ApiError::unauthorized(
                        "invalid enrollment code",
                    )));
                }

                tagmata::ActiveModel {
                    id: Set(tagma_id.to_string()),
                    owner_user_id: Set(owner),
                    pinned_public_key: Set(device_key),
                    created_at: Set(now),
                    label: Set(None),
                    last_tunnel_proof_ts: Set(None),
                }
                .insert(txn)
                .await?;

                tagma_tokens::ActiveModel {
                    token_hash: Set(tagma_token_hash),
                    tagma_id: Set(tagma_id.to_string()),
                    issued_at: Set(now),
                    revoked_at: Set(None),
                }
                .insert(txn)
                .await?;

                let mut am: enrollment_tokens::ActiveModel = token.into();
                am.consumed_at = Set(Some(now));
                am.consumed_by_tagma = Set(Some(tagma_id.to_string()));
                am.update(txn).await?;
                Ok(())
            })
        })
        .await;
    flatten_txn(result)?;

    Ok(Json(EnrollResponse {
        tagma_id,
        tagma_token: tagma_token_plaintext,
    }))
}

/// A rejected enroll proof is a client error (malformed or invalid signature).
fn proof_to_bad_request(e: ProofError) -> ApiError {
    ApiError::bad_request(format!("invalid enrollment proof: {e}"))
}

#[derive(Serialize)]
struct TagmaInfo {
    tagma_id: String,
    pinned_public_key: Ed25519PublicKey,
}

async fn get_tagma(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
) -> Result<Json<TagmaInfo>, ApiError> {
    let user = require_user(&principal)?;
    let tagma_id = TagmaId::from(id);
    let tagma = tagmata::Entity::find_by_id(tagma_id.to_string())
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    let tagma = tagma.ok_or_else(|| ApiError::not_found("unknown tagma"))?;
    // Existence-oracle hardening: a non-owner gets the same 404 as for an
    // unknown tagma, so they cannot confirm whether a guessed tagma id exists.
    if tagma.owner_user_id != user.as_ref() {
        return Err(ApiError::not_found("unknown tagma"));
    }
    Ok(Json(TagmaInfo {
        tagma_id: tagma_id.to_string(),
        pinned_public_key: Ed25519PublicKey(tagma.pinned_public_key),
    }))
}
