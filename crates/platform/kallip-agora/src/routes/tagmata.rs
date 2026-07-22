//! Tagma lifecycle: pending (an enrollment code) -> enrolled (a herald pinned
//! its device key) -> revoked.
//!
//! `POST /v1/tagmata/enroll` (unauthenticated, rate-limited) redeems a pending
//! tagma's enrollment code for a long-lived tagma token, pinning the herald's
//! Ed25519 device public key. The herald must sign the enrollment transcript
//! with the matching private key (proof of possession), so a stolen code alone
//! cannot pin an attacker-chosen key. The pending row is locked `FOR UPDATE`
//! and the full live predicate re-checked (not enrolled / not revoked / not
//! expired), so a concurrent redeem race is rejected (first wins, 409).
//!
//! The authenticated surface (`POST /v1/tagmata` mint, `GET /v1/tagmata` list,
//! `GET/PATCH/DELETE /v1/tagmata/{id}`) is owner-scoped. `DELETE` revokes (sets
//! `revoked_at`); for an enrolled tagma that flag is checked in `resolve_bearer`
//! on every herald request, so a revoke cuts the device off on its next call.
//! `GET /v1/tagmata/{id}` serves the pinned key to the owning user (TOFU with
//! change-detection on the app side) and 404s for a still-pending tagma.

use crate::db::entity::{tagma_tokens, tagmata, users};
use crate::db::{TxnError, flatten_txn, map_db_err};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control::{EnrollRequest, EnrollResponse};
use kallip_agora_common::ids::{TagmaId, UserId};
use kallip_agora_common::proof::{ProofError, verify_enroll_proof};
use kallip_common::authtoken::{MintedToken, TokenHash, mask_token};
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::warn;

use crate::auth::{AuthPrincipal, require_user};
use crate::state::SharedState;
use crate::token::{ENROLLMENT, TAGMA};

/// Expected length of an Ed25519 public key, enforced at the enrollment
/// boundary (the wire newtype carries bytes without a length check).
const ED25519_PUBLIC_KEY_LEN: usize = 32;

/// Max live (pending: unenrolled + unrevoked) tagmas per owner. Bounds
/// `tagmata` growth against a user who mints and forgets; count-then-insert,
/// so the cap is soft under true concurrency. Enforced in [`mint_pending_tagma`]
/// for both the self-service and admin mints. Revoked/enrolled rows do not
/// count; an expired-but-unrevoked pending row still counts (mirrors the
/// original enrollment-code cap).
const MAX_LIVE_PENDING_TAGMAS: u64 = 8;

/// The unauthenticated enroll surface: redeem an enrollment code (transition a
/// pending tagma to enrolled). Rate-limited at the mount site (CPU- and
/// DB-expensive and reachable by anyone holding a code).
pub fn enroll_router() -> Router<SharedState> {
    Router::new().route("/tagmata/enroll", post(enroll))
}

/// The authenticated tagma surface: mint a pending tagma, list the owner's
/// tagmata (with live presence), read/rename/revoke one. Cookie/bearer-authed
/// and not rate-limited (must not share the unauth bucket, same reasoning as
/// `/me`).
pub fn protected_router() -> Router<SharedState> {
    Router::new()
        .route("/tagmata", get(list_tagmata).post(mint))
        .route(
            "/tagmata/{id}",
            get(get_tagma).patch(rename_tagma).delete(revoke_tagma),
        )
}

// ---------------------------------------------------------------------------
// enroll (pending -> enrolled)
// ---------------------------------------------------------------------------

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
    // not touch the row. Pure CPU, outside any DB row lock.
    verify_enroll_proof(&req.device_public_key.0, &req.code, &req.signature.0)
        .map_err(proof_to_bad_request)?;

    let code_hash = TokenHash::of(&req.code);
    let tagma_token = MintedToken::generate(TAGMA);
    let tagma_token_plaintext = tagma_token.secret().to_string();
    let tagma_token_hash = tagma_token.hash().as_bytes().to_vec();
    let device_key = req.device_public_key.0.clone();

    // One transaction: lock the pending tagma row by enrollment-code hash FOR
    // UPDATE, re-check the full live predicate (defeats a parallel redeem race
    // -- the second txn blocks until the first commits, then sees enrolled_at
    // set), transition the SAME row to enrolled, and issue its tagma token.
    let result = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let code_hash = code_hash.as_bytes().to_vec();
            let tagma_token_hash = tagma_token_hash.clone();
            let device_key = device_key.clone();
            Box::pin(async move {
                let row = tagmata::Entity::find()
                    .filter(tagmata::Column::EnrollmentCodeHash.eq(code_hash))
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                let Some(row) = row else {
                    return Err(TxnError::Api(ApiError::unauthorized(
                        "invalid enrollment code",
                    )));
                };
                let now = OffsetDateTime::now_utc();
                if row.enrolled_at.is_some() {
                    warn!("enrollment code redeemed while already enrolled");
                    return Err(TxnError::Api(ApiError::conflict(
                        "enrollment code already used",
                    )));
                }
                // Revoked / expired / missing expiry are all uniform 401 "invalid
                // enrollment code" so the response leaks nothing about which.
                if row.revoked_at.is_some() || row.expires_at.is_none_or(|e| e <= now) {
                    return Err(TxnError::Api(ApiError::unauthorized(
                        "invalid enrollment code",
                    )));
                }
                let owner = row.owner_user_id.clone();
                let tagma_id = row.id.clone();

                // A disabled account cannot enroll a device key. Re-check the
                // owner under a row lock for a race-free read against a
                // concurrent disable. Same message as an invalid code.
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

                // Transition the locked row: pin the key, mark enrolled, clear
                // the one-time code fields. `row.into()` keeps the PK (and
                // every other column) `Unchanged`, so only the Set fields write.
                let mut am: tagmata::ActiveModel = row.into();
                am.pinned_public_key = Set(Some(device_key));
                am.enrolled_at = Set(Some(now));
                am.enrollment_code_hash = Set(None);
                am.enrollment_code_masked = Set(None);
                am.expires_at = Set(None);
                am.update(txn).await?;

                tagma_tokens::ActiveModel {
                    token_hash: Set(tagma_token_hash),
                    tagma_id: Set(tagma_id.clone()),
                    issued_at: Set(now),
                }
                .insert(txn)
                .await?;
                Ok(tagma_id)
            })
        })
        .await;
    let tagma_id = flatten_txn(result)?;

    Ok(Json(EnrollResponse {
        tagma_id: TagmaId::from(tagma_id),
        tagma_token: tagma_token_plaintext,
    }))
}

/// A rejected enroll proof is a client error (malformed or invalid signature).
fn proof_to_bad_request(e: ProofError) -> ApiError {
    ApiError::bad_request(format!("invalid enrollment proof: {e}"))
}

// ---------------------------------------------------------------------------
// mint (create a pending tagma / enrollment code)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MintResponse {
    /// `sk-enroll-...` plaintext; returned once. The agora retains only its
    /// hash (the masked form is persisted on the row for the list endpoint).
    code: String,
    /// The new tagma's id. Stable across the enroll transition.
    id: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

/// Mint a pending tagma (a single-use enrollment code) bound to `owner`.
/// Enforces the per-owner live-pending cap. Shared by the self-service
/// (`POST /v1/tagmata`) and admin (`POST /v1/admin/tagmata`) mints so the cap
/// applies uniformly. Returns the new id + the once-shown plaintext + the
/// created/expiry timestamps.
pub(crate) async fn mint_pending_tagma(
    state: &SharedState,
    owner: &UserId,
) -> Result<(TagmaId, String, OffsetDateTime, OffsetDateTime), ApiError> {
    let now = OffsetDateTime::now_utc();

    // Count live pending tagmas. The predicate matches `list_tagmata`'s pending
    // filter exactly (enrolled_at IS NULL AND revoked_at IS NULL) so a user at
    // the cap sees precisely the rows that count toward it -- including
    // expired-but-unrevoked ones.
    let live = tagmata::Entity::find()
        .filter(tagmata::Column::OwnerUserId.eq(owner.to_string()))
        .filter(tagmata::Column::EnrolledAt.is_null())
        .filter(tagmata::Column::RevokedAt.is_null())
        .count(&state.db)
        .await
        .map_err(map_db_err)?;
    if live >= MAX_LIVE_PENDING_TAGMAS {
        return Err(ApiError::too_many_requests(
            "too many live enrollment codes",
        ));
    }

    let code = MintedToken::generate(ENROLLMENT);
    let plaintext = code.secret().to_string();
    let masked = mask_token(code.secret(), ENROLLMENT);
    let id = TagmaId::random();
    let expires_at = now + state.limits.enrollment_code_ttl;
    tagmata::ActiveModel {
        id: Set(id.to_string()),
        owner_user_id: Set(owner.to_string()),
        pinned_public_key: Set(None),
        created_at: Set(now),
        label: Set(None),
        last_tunnel_proof_ts: Set(None),
        revoked_at: Set(None),
        enrolled_at: Set(None),
        enrollment_code_hash: Set(Some(code.hash().as_bytes().to_vec())),
        enrollment_code_masked: Set(Some(masked)),
        expires_at: Set(Some(expires_at)),
    }
    .insert(&state.db)
    .await
    .map_err(map_db_err)?;
    Ok((id, plaintext, now, expires_at))
}

async fn mint(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<MintResponse>, ApiError> {
    let user_id = require_user(&principal)?;
    let (id, plaintext, created_at, expires_at) = mint_pending_tagma(&state, user_id).await?;
    Ok(Json(MintResponse {
        code: plaintext,
        id: id.to_string(),
        created_at,
        expires_at,
    }))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// One row of the owner's tagma list. Liveness ("is a herald tunnel currently
/// open for this tagma") is NOT part of the registry's view: it is a data-plane
/// concern, delivered to the app via the relay's `GET /me/events` stream as
/// `TagmaOnline`/`TagmaOffline`. Note `tagmata.last_tunnel_proof_ts` is a replay
/// guard, not a liveness signal, and is intentionally not surfaced.
#[derive(Serialize)]
struct TagmaView {
    tagma_id: String,
    label: Option<String>,
    state: TagmaState,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    /// Pending-phase display mask (`sk-enroll-abc***xyz`). Present only while
    /// pending; omitted for enrolled rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    code_masked: Option<String>,
    /// Pending-phase code expiry. Present only while pending; omitted for
    /// enrolled rows.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    expires_at: Option<OffsetDateTime>,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum TagmaState {
    Pending,
    Enrolled,
}

/// List the caller's tagmata (pending + enrolled, not revoked), newest first.
/// Liveness is not included; the app learns it from the relay's event stream.
async fn list_tagmata(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<Vec<TagmaView>>, ApiError> {
    let user_id = require_user(&principal)?;
    let rows = tagmata::Entity::find()
        .filter(tagmata::Column::OwnerUserId.eq(user_id.to_string()))
        .filter(tagmata::Column::RevokedAt.is_null())
        .order_by_desc(tagmata::Column::CreatedAt)
        .all(&state.db)
        .await
        .map_err(map_db_err)?;
    let items = rows
        .into_iter()
        .map(|r| {
            let id = TagmaId::from(r.id.clone());
            let enrolled = r.enrolled_at.is_some();
            let state = if enrolled {
                TagmaState::Enrolled
            } else {
                TagmaState::Pending
            };
            // Phase fields are meaningful only while pending.
            let (code_masked, expires_at) = if enrolled {
                (None, None)
            } else {
                (r.enrollment_code_masked, r.expires_at)
            };
            TagmaView {
                tagma_id: id.to_string(),
                label: r.label,
                state,
                created_at: r.created_at,
                code_masked,
                expires_at,
            }
        })
        .collect();
    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// get (pinned key TOFU)
// ---------------------------------------------------------------------------

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
    // A pending tagma has no pinned key to serve. The app only fetches the key
    // for an enrolled tagma (TOFU); 404 keeps the existence-oracle uniform.
    let Some(key) = tagma.pinned_public_key else {
        return Err(ApiError::not_found("unknown tagma"));
    };
    Ok(Json(TagmaInfo {
        tagma_id: tagma_id.to_string(),
        pinned_public_key: Ed25519PublicKey(key),
    }))
}

// ---------------------------------------------------------------------------
// rename (label)
// ---------------------------------------------------------------------------

/// Max tagma label length (after trim). Mirrors `MAX_DISPLAY_NAME_LEN` in
/// `routes/auth.rs`.
const MAX_TAGMA_LABEL_LEN: usize = 64;

#[derive(Deserialize)]
struct RenameTagmaRequest {
    #[serde(default)]
    label: Option<String>,
}

impl RenameTagmaRequest {
    /// Resolve the request to the label to store: `Some(trimmed)` for a
    /// non-empty value, `None` for empty/whitespace (clears the label), or an
    /// error if the trimmed value exceeds the cap. Shared by the rename handler
    /// and tests.
    fn resolve(&self) -> Result<Option<String>, ApiError> {
        Ok(match self.label.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => {
                if s.chars().count() > MAX_TAGMA_LABEL_LEN {
                    return Err(ApiError::bad_request(format!(
                        "label longer than {MAX_TAGMA_LABEL_LEN} chars"
                    )));
                }
                Some(s.to_string())
            }
            // None or empty-after-trim -> clear the label.
            _ => None,
        })
    }
}

/// Set or clear the caller's tagma label (works for both pending and enrolled).
/// Owner-scoped: an unknown or other-owner id is a 404 (no cross-user existence
/// oracle), mirroring `get_tagma`. An empty/whitespace label clears it (the
/// card title falls back to "Unnamed tagma"); a value over the cap is a 400.
async fn rename_tagma(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
    Json(req): Json<RenameTagmaRequest>,
) -> Result<StatusCode, ApiError> {
    let user = require_user(&principal)?;
    let resolved = req.resolve()?;
    let tagma_id = TagmaId::from(id);
    let tagma = tagmata::Entity::find_by_id(tagma_id.to_string())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown tagma"))?;
    // Same existence-oracle hardening as `get_tagma`: a non-owner gets the 404 an
    // unknown-id would, so they cannot confirm a guessed id exists.
    if tagma.owner_user_id != user.as_ref() {
        return Err(ApiError::not_found("unknown tagma"));
    }
    // `row.into()` keeps the PK (and every other column) `Unchanged`, so only
    // `label` is written -- pinned_public_key / owner / timestamps are untouched.
    let mut am: tagmata::ActiveModel = tagma.into();
    am.label = Set(resolved);
    am.update(&state.db).await.map_err(map_db_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// revoke (pending or enrolled)
// ---------------------------------------------------------------------------

/// Revoke one of the caller's tagmas (pending or enrolled). Owner-scoped: a
/// missing or other-owner id returns 404 (no existence oracle across users).
/// Idempotent + race-free via a conditional UPDATE that only touches rows still
/// live, so two concurrent revokes cannot clobber the first-revoked timestamp.
/// 204 either way for a row that exists and is/was the caller's. For an
/// enrolled tagma the flag is enforced in `resolve_bearer` on the herald's next
/// request; for a pending tagma it blocks redemption in `enroll`.
async fn revoke_tagma(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user = require_user(&principal)?;
    let tagma_id = TagmaId::from(id);
    // Existence + ownership check first so an unknown / other-user id is a clean
    // 404 rather than a silent 204 (the conditional UPDATE alone could not tell
    // those apart from an already-revoked own row).
    let owned = tagmata::Entity::find()
        .filter(tagmata::Column::Id.eq(tagma_id.to_string()))
        .filter(tagmata::Column::OwnerUserId.eq(user.to_string()))
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    if owned.is_none() {
        return Err(ApiError::not_found("unknown tagma"));
    }
    // Only advance `revoked_at` if still NULL; a second revoke leaves the
    // original timestamp intact (audit-relevant).
    tagmata::Entity::update_many()
        .filter(tagmata::Column::Id.eq(tagma_id.to_string()))
        .filter(tagmata::Column::OwnerUserId.eq(user.to_string()))
        .filter(tagmata::Column::RevokedAt.is_null())
        .col_expr(
            tagmata::Column::RevokedAt,
            sea_orm::sea_query::Expr::value(OffsetDateTime::now_utc()),
        )
        .exec(&state.db)
        .await
        .map_err(map_db_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    //! `list_tagmata` presence + isolation, `mint` round-trip + cap, and
    //! `revoke` idempotence + owner isolation.

    use axum::Json;
    use axum::extract::State;
    use axum::http::StatusCode;

    use super::{
        MAX_LIVE_PENDING_TAGMAS, MAX_TAGMA_LABEL_LEN, MintResponse, RenameTagmaRequest, TagmaState,
        list_tagmata, mint, rename_tagma, revoke_tagma,
    };
    use crate::auth::{AuthPrincipal, Principal};
    use crate::db::entity::tagmata;
    use crate::test_helpers::{make_state, seed_tagma, seed_user};
    use kallip_agora_common::bytes::Ed25519PublicKey;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    use time::OffsetDateTime;

    /// A 32-byte zero key is fine for `list_tagmata` (it never verifies it).
    fn dummy_key() -> Ed25519PublicKey {
        Ed25519PublicKey([0u8; 32].to_vec())
    }

    /// An owner with no tagmata gets `[]` (200, not 404).
    #[tokio::test]
    async fn empty_list() {
        let state = make_state().await;
        let user = seed_user(&state, "alice", "alice@example.test").await;
        let Json(got) = list_tagmata(State(state), AuthPrincipal(Principal::User(user)))
            .await
            .expect("list");
        assert!(got.is_empty());
    }

    /// An enrolled tagma appears as enrolled in the list, with the pending-phase
    /// fields omitted. (Live presence is no longer part of the list view; the
    /// app learns it from the relay's event stream.)
    #[tokio::test]
    async fn enrolled_tagma_lists_as_enrolled() {
        let state = make_state().await;
        let user = seed_user(&state, "alice", "alice@example.test").await;
        let (tagma, _) = seed_tagma(&state, &user, dummy_key()).await;

        let Json(got) = list_tagmata(State(state), AuthPrincipal(Principal::User(user)))
            .await
            .expect("list");
        assert_eq!(got.len(), 1);
        let row = &got[0];
        assert_eq!(row.tagma_id, tagma.as_ref());
        assert!(matches!(row.state, TagmaState::Enrolled));
        assert!(row.code_masked.is_none());
        assert!(row.expires_at.is_none());
    }

    /// A minted pending tagma appears in the list as pending with its masked
    /// code, and the once-returned plaintext hashes to the stored row.
    #[tokio::test]
    async fn mint_then_list_round_trip() {
        let state = make_state().await;
        let user = seed_user(&state, "alice", "alice@example.test").await;
        let principal = AuthPrincipal(Principal::User(user.clone()));

        let Json(MintResponse { code, id, .. }) = mint(State(state.clone()), principal.clone())
            .await
            .expect("mint");
        assert!(code.starts_with("sk-enroll-"));
        let expected_hash = kallip_common::authtoken::TokenHash::of(&code)
            .as_bytes()
            .to_vec();
        let row = tagmata::Entity::find_by_id(id.clone())
            .one(&state.db)
            .await
            .expect("read")
            .expect("pending row exists");
        assert_eq!(row.enrollment_code_hash.expect("code hash"), expected_hash);

        let Json(listed) = list_tagmata(State(state), principal).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].tagma_id, id);
        assert!(matches!(listed[0].state, TagmaState::Pending));
        assert_eq!(
            listed[0].code_masked.as_deref(),
            Some(kallip_common::authtoken::mask_token(&code, super::ENROLLMENT).as_str())
        );
        assert!(listed[0].expires_at.is_some());
    }

    /// An expired-but-unrevoked pending tagma still counts toward the cap (the
    /// predicate is enrolled/revoked, not expires_at), so minting past the cap
    /// 429s even with expired rows present.
    #[tokio::test]
    async fn cap_counts_expired_pending_tagmas() {
        let state = make_state().await;
        let user = seed_user(&state, "bob", "bob@example.test").await;
        let principal = AuthPrincipal(Principal::User(user.clone()));
        // Seed the cap (8) of already-expired, pending (unenrolled, unrevoked)
        // tagmas directly. Each needs a distinct code hash (partial unique
        // index).
        let now = OffsetDateTime::now_utc();
        for n in 0..MAX_LIVE_PENDING_TAGMAS {
            let mut code_hash = [0u8; 32];
            code_hash[..8].copy_from_slice(&n.to_le_bytes());
            tagmata::ActiveModel {
                id: Set(kallip_agora_common::ids::TagmaId::random().to_string()),
                owner_user_id: Set(user.to_string()),
                pinned_public_key: Set(None),
                created_at: Set(now - time::Duration::days(2)),
                label: Set(None),
                last_tunnel_proof_ts: Set(None),
                revoked_at: Set(None),
                enrolled_at: Set(None),
                enrollment_code_hash: Set(Some(code_hash.to_vec())),
                enrollment_code_masked: Set(Some("seed-mask".to_string())),
                expires_at: Set(Some(now - time::Duration::days(1))),
            }
            .insert(&state.db)
            .await
            .expect("seed expired pending tagma");
        }
        match mint(State(state), principal).await {
            Err(e) => assert_eq!(e.status, 429),
            Ok(_) => panic!("cap reached must 429"),
        }
    }

    /// Revoking twice is idempotent: both return 204, and `revoked_at` keeps its
    /// original timestamp.
    #[tokio::test]
    async fn revoke_is_idempotent() {
        let state = make_state().await;
        let user = seed_user(&state, "carol", "carol@example.test").await;
        let Json(MintResponse { id, .. }) = mint(
            State(state.clone()),
            AuthPrincipal(Principal::User(user.clone())),
        )
        .await
        .expect("mint");
        let path = || axum::extract::Path(id.clone());

        let s1 = revoke_tagma(
            State(state.clone()),
            AuthPrincipal(Principal::User(user.clone())),
            path(),
        )
        .await
        .expect("first revoke");
        assert_eq!(s1, StatusCode::NO_CONTENT);
        let first = tagmata::Entity::find_by_id(id.clone())
            .one(&state.db)
            .await
            .expect("read")
            .expect("row")
            .revoked_at
            .expect("revoked once");

        let s2 = revoke_tagma(
            State(state.clone()),
            AuthPrincipal(Principal::User(user.clone())),
            path(),
        )
        .await
        .expect("second revoke");
        assert_eq!(s2, StatusCode::NO_CONTENT);
        let second = tagmata::Entity::find_by_id(id.clone())
            .one(&state.db)
            .await
            .expect("read")
            .expect("row")
            .revoked_at
            .expect("still revoked");
        assert_eq!(first, second, "revoked_at must not be clobbered");
    }

    /// Revoking another user's tagma is a 404 (no cross-user existence oracle).
    #[tokio::test]
    async fn revoke_is_owner_scoped() {
        let state = make_state().await;
        let alice = seed_user(&state, "alice", "alice@example.test").await;
        let bob = seed_user(&state, "bob", "bob@example.test").await;
        let Json(MintResponse { id, .. }) =
            mint(State(state.clone()), AuthPrincipal(Principal::User(alice)))
                .await
                .expect("mint");
        match revoke_tagma(
            State(state),
            AuthPrincipal(Principal::User(bob)),
            axum::extract::Path(id),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("cross-user revoke must 404"),
        }
    }

    /// Rename sets (from None), overwrites (with trim), clears on whitespace,
    /// rejects over-length, and is owner-scoped (non-owner -> 404, no mutation).
    #[tokio::test]
    async fn rename_tagma_sets_clears_and_is_owner_scoped() {
        let state = make_state().await;
        let alice = seed_user(&state, "alice", "alice@example.test").await;
        let bob = seed_user(&state, "bob", "bob@example.test").await;
        let (tagma_id, _) = seed_tagma(&state, &alice, dummy_key()).await;
        let id = tagma_id.to_string();
        let path = || axum::extract::Path(id.clone());
        let row_label = || async {
            tagmata::Entity::find_by_id(id.clone())
                .one(&state.db)
                .await
                .expect("read")
                .expect("row")
                .label
        };

        // Set from None.
        let s = rename_tagma(
            State(state.clone()),
            AuthPrincipal(Principal::User(alice.clone())),
            path(),
            Json(RenameTagmaRequest {
                label: Some("laptop".to_string()),
            }),
        )
        .await
        .expect("set");
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(row_label().await.as_deref(), Some("laptop"));

        // Overwrite, with surrounding whitespace trimmed.
        rename_tagma(
            State(state.clone()),
            AuthPrincipal(Principal::User(alice.clone())),
            path(),
            Json(RenameTagmaRequest {
                label: Some("  server ".to_string()),
            }),
        )
        .await
        .expect("overwrite");
        assert_eq!(row_label().await.as_deref(), Some("server"));

        // Whitespace-only clears the label.
        rename_tagma(
            State(state.clone()),
            AuthPrincipal(Principal::User(alice.clone())),
            path(),
            Json(RenameTagmaRequest {
                label: Some("   ".to_string()),
            }),
        )
        .await
        .expect("clear");
        assert!(row_label().await.is_none());

        // Over-length (after trim) -> 400.
        let too_long = "x".repeat(MAX_TAGMA_LABEL_LEN + 1);
        match rename_tagma(
            State(state.clone()),
            AuthPrincipal(Principal::User(alice.clone())),
            path(),
            Json(RenameTagmaRequest {
                label: Some(too_long),
            }),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 400),
            Ok(_) => panic!("over-length must 400"),
        }

        // Non-owner -> 404 and no mutation.
        match rename_tagma(
            State(state.clone()),
            AuthPrincipal(Principal::User(bob)),
            path(),
            Json(RenameTagmaRequest {
                label: Some("evil".to_string()),
            }),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("non-owner must 404"),
        }
        assert!(row_label().await.is_none());
    }
}
