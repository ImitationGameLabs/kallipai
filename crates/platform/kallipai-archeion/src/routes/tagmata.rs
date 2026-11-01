//! Tagma lifecycle: pending (an enrollment code) -> enrolled (a tagma pinned
//! its device key) -> revoked.
//!
//! `POST /tagmata/enroll` (unauthenticated, rate-limited) redeems a pending
//! tagma's enrollment code for a long-lived tagma token, pinning the tagma's
//! Ed25519 device public key. The tagma must sign the enrollment transcript
//! with the matching private key (proof of possession), so a stolen code alone
//! cannot pin an attacker-chosen key. The pending row is locked `FOR UPDATE`
//! and the full live predicate re-checked (not enrolled / not revoked / not
//! expired), so a concurrent redeem race is rejected (first wins, 409).
//!
//! The authenticated surface (`POST /tagmata` mint, `GET /tagmata` list,
//! `GET/PATCH/DELETE /tagmata/{id}`) is owner-scoped. `DELETE` revokes (sets
//! `revoked_at`); for an enrolled tagma that flag is checked in `resolve_bearer`
//! on every tagma request, so a revoke cuts the tagma off on its next call.
//! `GET /tagmata/{id}` serves the pinned key to the owning user (TOFU with
//! change-detection on the app side) and 404s for a still-pending tagma.

use crate::db::entity::{tagmata, users};
use crate::db::{TxnError, flatten_txn, map_db_err};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use kallipai_archeion_common::bytes::Ed25519PublicKey;
use kallipai_archeion_common::control::{EnrollRequest, EnrollResponse};
use kallipai_archeion_common::ids::{TagmaId, UserId};
use kallipai_archeion_common::mint::MintEnrollmentCodeResponse;
use kallipai_archeion_common::perm::{ARCHEION_TAGMA_READ, ARCHEION_TAGMA_WRITE};
use kallipai_archeion_common::proof::{ProofError, verify_enroll_proof};
use kallipai_common::authtoken::{MintedToken, TokenHash, mask_token};
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::warn;

use crate::auth::{AuthPrincipal, require_perm, require_user};
use crate::credentials;
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
/// tagmata, read/rename/revoke one. Cookie/bearer-authed
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
    let device_key = req.device_public_key.0.clone();

    // One transaction: lock the pending tagma row by enrollment-code hash FOR
    // UPDATE, re-check the full live predicate (defeats a parallel redeem race
    // -- the second txn blocks until the first commits, then sees enrolled_at
    // set), transition the SAME row to enrolled, and issue its tagma token.
    let result = state
        .db
        .transaction::<_, _, TxnError>(|txn| {
            let code_hash = code_hash.as_bytes().to_vec();
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

                // The tagma's one live credential record. Its permission set
                // is the default three (files read, write, deliver): a tagma
                // credential cannot reach the platform API, and this record
                // does not widen that.
                let perms = kallipai_archeion_common::perm::tagma_defaults();
                let minted = credentials::create(
                    txn,
                    TAGMA,
                    credentials::NewCredential {
                        owner: credentials::Owner::Tagma,
                        owner_id: &tagma_id,
                        name: "enrollment",
                        perms: &perms,
                        created_by: &owner,
                        actor: None,
                        expires_at: None,
                    },
                )
                .await
                .map_err(TxnError::Api)?;
                Ok((tagma_id, minted.plaintext))
            })
        })
        .await;
    let (tagma_id, tagma_token_plaintext) = flatten_txn(result)?;

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

/// Who asked for the code, for the audit row: the credential that carried the
/// call and the user it acted for. An admin mint has no credential record, so
/// it passes `None` and writes no row.
pub(crate) struct MintAudit {
    pub token_id: String,
    pub actor: String,
}

/// What a minted pending tagma hands back.
pub(crate) struct PendingMint {
    pub id: TagmaId,
    pub code: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

/// Mint a pending tagma (a single-use enrollment code) bound to `owner`.
/// Enforces the per-owner live-pending cap. Shared by the self-service
/// (`POST /tagmata`) and admin (`POST /admin/tagmata`) mints so the cap
/// applies uniformly. The rows the cap counts are the ones with
/// `enrolled_at IS NULL AND revoked_at IS NULL`, so an expired code still
/// holds a slot until it is revoked.
///
/// The pending row and, when `audit` is given, its `mint` row land in one
/// transaction, and the plaintext leaves only after the commit: a mint that
/// cannot record who asked must not spend the owner's slot on a row whose
/// code nobody received.
pub(crate) async fn mint_pending_tagma(
    state: &SharedState,
    owner: &UserId,
    audit: Option<MintAudit>,
) -> Result<PendingMint, ApiError> {
    let now = OffsetDateTime::now_utc();
    // Minted before the transaction: the code is a local secret, and a failed
    // transaction never hands it out.
    let code = MintedToken::generate(ENROLLMENT).expect("getrandom failed");
    let plaintext = code.secret().to_string();
    let masked = mask_token(code.secret(), ENROLLMENT);
    let id = TagmaId::random();
    let id_string = id.to_string();
    let expires_at = now + state.limits.enrollment_code_ttl;
    let owner_id = owner.to_string();

    let outcome = state
        .db
        .transaction::<_, (), TxnError>(|txn| {
            Box::pin(async move {
                // Count live pending tagmas. The predicate matches
                // `list_tagmata`'s pending filter exactly (enrolled_at IS NULL
                // AND revoked_at IS NULL) so a user at the cap sees precisely
                // the rows that count toward it -- including
                // expired-but-unrevoked ones.
                let live = tagmata::Entity::find()
                    .filter(tagmata::Column::OwnerUserId.eq(owner_id.as_str()))
                    .filter(tagmata::Column::EnrolledAt.is_null())
                    .filter(tagmata::Column::RevokedAt.is_null())
                    .count(txn)
                    .await?;
                if live >= MAX_LIVE_PENDING_TAGMAS {
                    return Err(TxnError::Api(ApiError::too_many_requests(
                        "too many live enrollment codes",
                    )));
                }
                tagmata::ActiveModel {
                    id: Set(id_string.clone()),
                    owner_user_id: Set(owner_id.clone()),
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
                .insert(txn)
                .await?;
                if let Some(audit) = &audit {
                    credentials::record_mint(
                        txn,
                        audit.token_id.as_str(),
                        now,
                        audit.actor.as_str(),
                        &id_string,
                    )
                    .await
                    .map_err(TxnError::Api)?;
                }
                Ok(())
            })
        })
        .await;
    flatten_txn(outcome)?;
    Ok(PendingMint {
        id,
        code: plaintext,
        created_at: now,
        expires_at,
    })
}

/// Mint a pending tagma for the calling user.
///
/// The same `archeion:tagma:write` string that renames and revokes a tagma.
/// A personal access token carrying it mints under its owner; the session
/// cookie and the admin bearer carry no record and pass the string check,
/// but the admin bearer is not a user and stops at `require_user` -- the
/// admin entry to this is `/admin/tagmata`.
async fn mint(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
) -> Result<Json<MintEnrollmentCodeResponse>, ApiError> {
    require_perm(&auth, ARCHEION_TAGMA_WRITE)?;
    let user_id = require_user(&auth.principal)?;
    // A session mints with no record, so there is nothing to hang a row off.
    let audit = auth.credential.as_ref().map(|credential| MintAudit {
        token_id: credential.token.to_string(),
        actor: user_id.to_string(),
    });
    let minted = mint_pending_tagma(&state, user_id, audit).await?;
    Ok(Json(MintEnrollmentCodeResponse {
        code: minted.code,
        id: minted.id.to_string(),
        created_at: minted.created_at,
        expires_at: minted.expires_at,
    }))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// One row of the owner's tagma list. Liveness ("is a tagma tunnel currently
/// open for this tagma") is NOT part of the registry's view: it is a data-plane
/// concern, delivered to the app by the keryx on its `GET /me/events` stream as
/// `TagmaOnline`/`TagmaOffline`. Note `tagmata.last_tunnel_proof_ts` is a replay
/// guard, not a liveness signal, and is intentionally not surfaced.
#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum TagmaState {
    Pending,
    Enrolled,
}

/// List the caller's tagmata (pending + enrolled, not revoked), newest first.
/// Liveness is not included; the app learns it from the keryx `/me/events` stream.
async fn list_tagmata(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
) -> Result<Json<Vec<TagmaView>>, ApiError> {
    require_perm(&auth, ARCHEION_TAGMA_READ)?;
    let user_id = require_user(&auth.principal)?;
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
    auth: AuthPrincipal,
    Path(id): Path<String>,
) -> Result<Json<TagmaInfo>, ApiError> {
    require_perm(&auth, ARCHEION_TAGMA_READ)?;
    let user = require_user(&auth.principal)?;
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
/// `routes/auth/register.rs`.
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
    auth: AuthPrincipal,
    Path(id): Path<String>,
    Json(req): Json<RenameTagmaRequest>,
) -> Result<StatusCode, ApiError> {
    require_perm(&auth, ARCHEION_TAGMA_WRITE)?;
    let user = require_user(&auth.principal)?;
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
/// enrolled tagma the flag is enforced in `resolve_bearer` on the tagma's next
/// request; for a pending tagma it blocks redemption in `enroll`.
async fn revoke_tagma(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_perm(&auth, ARCHEION_TAGMA_WRITE)?;
    let user = require_user(&auth.principal)?;
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
        MAX_LIVE_PENDING_TAGMAS, MAX_TAGMA_LABEL_LEN, MintAudit, RenameTagmaRequest, TagmaState,
        get_tagma, list_tagmata, mint, mint_pending_tagma, rename_tagma, revoke_tagma,
    };
    use crate::auth::{AuthPrincipal, Principal};
    use crate::db::entity::tagmata;
    use crate::test_helpers::{make_state, seed_tagma, seed_user};
    use kallipai_archeion_common::bytes::Ed25519PublicKey;
    use kallipai_archeion_common::mint::MintEnrollmentCodeResponse as MintResponse;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
    use time::OffsetDateTime;

    /// A 32-byte zero key is fine for `list_tagmata` (it never verifies it).
    fn dummy_key() -> Ed25519PublicKey {
        Ed25519PublicKey([0u8; 32].to_vec())
    }

    /// Seed a personal token for `user` on `perms` and return its record id.
    async fn seed_personal_token(
        state: &crate::state::SharedState,
        user: &kallipai_archeion_common::ids::UserId,
        perms: &[&str],
    ) -> String {
        let set = kallipai_archeion_common::perm::PermSet::from_known(perms.iter().copied())
            .expect("known strings");
        crate::credentials::create(
            &state.db,
            crate::token::PAT,
            crate::credentials::NewCredential {
                owner: crate::credentials::Owner::User,
                owner_id: user.as_ref(),
                name: "script",
                perms: &set,
                created_by: user.as_ref(),
                actor: Some(user.as_ref()),
                expires_at: None,
            },
        )
        .await
        .expect("seed a personal token")
        .record
        .id
    }

    /// An owner with no tagmata gets `[]` (200, not 404).
    #[tokio::test]
    async fn empty_list() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let Json(got) = list_tagmata(State(state), AuthPrincipal::unscoped(Principal::User(user)))
            .await
            .expect("list");
        assert!(got.is_empty());
    }

    /// An enrolled tagma appears as enrolled in the list, with the pending-phase
    /// fields omitted. (Live presence is not part of the list view; the
    /// app learns it from the keryx `/me/events` stream.)
    #[tokio::test]
    async fn enrolled_tagma_lists_as_enrolled() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let (tagma, _) = seed_tagma(&state, &user, dummy_key()).await;

        let Json(got) = list_tagmata(State(state), AuthPrincipal::unscoped(Principal::User(user)))
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
        let user = seed_user(&state, "alice").await;
        let principal = AuthPrincipal::unscoped(Principal::User(user.clone()));

        let Json(MintResponse { code, id, .. }) = mint(
            State(state.clone()),
            AuthPrincipal::unscoped(Principal::User(user.clone())),
        )
        .await
        .expect("mint");
        assert!(code.starts_with("sk-enroll-"));
        let expected_hash = kallipai_common::authtoken::TokenHash::of(&code)
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
            Some(kallipai_common::authtoken::mask_token(&code, super::ENROLLMENT).as_str())
        );
        assert!(listed[0].expires_at.is_some());
    }

    /// An expired-but-unrevoked pending tagma still counts toward the cap (the
    /// predicate is enrolled/revoked, not expires_at), so minting past the cap
    /// 429s even with expired rows present.
    #[tokio::test]
    async fn cap_counts_expired_pending_tagmas() {
        let state = make_state().await;
        let user = seed_user(&state, "bob").await;
        // Seed the cap (8) of already-expired, pending (unenrolled, unrevoked)
        // tagmas directly. Each needs a distinct code hash (partial unique
        // index).
        let now = OffsetDateTime::now_utc();
        for n in 0..MAX_LIVE_PENDING_TAGMAS {
            let mut code_hash = [0u8; 32];
            code_hash[..8].copy_from_slice(&n.to_le_bytes());
            tagmata::ActiveModel {
                id: Set(kallipai_archeion_common::ids::TagmaId::random().to_string()),
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
        match mint(State(state), AuthPrincipal::unscoped(Principal::User(user))).await {
            Err(e) => assert_eq!(e.status, 429),
            Ok(_) => panic!("cap reached must 429"),
        }
    }

    /// Revoking twice is idempotent: both return 204, and `revoked_at` keeps its
    /// original timestamp.
    #[tokio::test]
    async fn revoke_is_idempotent() {
        let state = make_state().await;
        let user = seed_user(&state, "carol").await;
        let Json(MintResponse { id, .. }) = mint(
            State(state.clone()),
            AuthPrincipal::unscoped(Principal::User(user.clone())),
        )
        .await
        .expect("mint");
        let path = || axum::extract::Path(id.clone());

        let s1 = revoke_tagma(
            State(state.clone()),
            AuthPrincipal::unscoped(Principal::User(user.clone())),
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
            AuthPrincipal::unscoped(Principal::User(user.clone())),
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
        let alice = seed_user(&state, "alice").await;
        let bob = seed_user(&state, "bob").await;
        let Json(MintResponse { id, .. }) = mint(
            State(state.clone()),
            AuthPrincipal::unscoped(Principal::User(alice)),
        )
        .await
        .expect("mint");
        match revoke_tagma(
            State(state),
            AuthPrincipal::unscoped(Principal::User(bob)),
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
        let alice = seed_user(&state, "alice").await;
        let bob = seed_user(&state, "bob").await;
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
            AuthPrincipal::unscoped(Principal::User(alice.clone())),
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
            AuthPrincipal::unscoped(Principal::User(alice.clone())),
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
            AuthPrincipal::unscoped(Principal::User(alice.clone())),
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
            AuthPrincipal::unscoped(Principal::User(alice.clone())),
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
            AuthPrincipal::unscoped(Principal::User(bob)),
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
    /// A full enrollment through the handler: the pending row becomes one
    /// enrolled tagma holding exactly one live credential record, whose stored
    /// hash is the returned plaintext's, carrying the tagma defaults and the
    /// `create` event. The plaintext exists only in the response.
    #[tokio::test]
    async fn enroll_writes_one_credential_record() {
        use ed25519_dalek::{Signer, SigningKey};
        use kallipai_archeion_common::bytes::Ed25519Signature;
        use kallipai_archeion_common::control::EnrollRequest;
        use kallipai_archeion_common::proof::enroll_transcript;
        use kallipai_common::authtoken::TokenHash;

        use super::enroll;
        use crate::credentials;

        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let Json(MintResponse { code, id, .. }) = mint(
            State(state.clone()),
            AuthPrincipal::unscoped(Principal::User(user.clone())),
        )
        .await
        .expect("mint");

        let signing = SigningKey::from_bytes(&[0x24; 32]);
        let public = signing.verifying_key().to_bytes();
        let signature = signing.sign(&enroll_transcript(&code, &public));

        let Json(response) = enroll(
            State(state.clone()),
            Json(EnrollRequest {
                code,
                device_public_key: Ed25519PublicKey(public.to_vec()),
                signature: Ed25519Signature(signature.to_bytes().to_vec()),
            }),
        )
        .await
        .expect("enroll");
        assert_eq!(response.tagma_id.to_string(), id);

        let record = credentials::live_tagma_record(&state.db, &id)
            .await
            .expect("lookup")
            .expect("the enrolled tagma holds a record");
        assert_eq!(record.name, "enrollment");
        assert_eq!(
            record.token_hash,
            TokenHash::of(&response.tagma_token).as_bytes().to_vec()
        );
        assert_eq!(
            credentials::perms_of(&record).expect("stored perms"),
            kallipai_archeion_common::perm::tagma_defaults()
        );
        assert!(record.expires_at.is_none());
        assert_eq!(record.created_by, user.to_string());

        let trail = credentials::events(&state.db, &record.id)
            .await
            .expect("events");
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].kind, credentials::EVENT_CREATE);
        assert!(
            trail[0].actor_user_id.is_none(),
            "the tagma enrolled itself, so no user acted"
        );
    }

    /// Every gated tagma route answers on its own permission string: the same
    /// call with the string is a success and without it a 403, so a route that
    /// forgot its check fails here instead of shipping open. Mint is a write
    /// too: a record needs the write string, not merely a valid token.
    #[tokio::test]
    async fn each_tagma_route_requires_its_string() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let (tagma_id, _) = seed_tagma(&state, &user, dummy_key()).await;

        // The mint below writes its audit row against this token, so the
        // record has to exist: a made-up id is a foreign-key violation.
        let token_id = seed_personal_token(
            &state,
            &user,
            &[
                kallipai_archeion_common::perm::ARCHEION_TAGMA_READ,
                kallipai_archeion_common::perm::ARCHEION_TAGMA_WRITE,
            ],
        )
        .await;

        let scoped = |perms: &[&str]| {
            AuthPrincipal::scoped(
                Principal::User(user.clone()),
                kallipai_archeion_common::control_plane::CredentialScope {
                    token: kallipai_archeion_common::ids::TokenId::from(token_id.clone()),
                    perms: kallipai_archeion_common::perm::PermSet::from_known(
                        perms.iter().copied(),
                    )
                    .expect("known strings"),
                },
            )
        };
        let read_only = scoped(&[kallipai_archeion_common::perm::ARCHEION_TAGMA_READ]);
        let write = scoped(&[
            kallipai_archeion_common::perm::ARCHEION_TAGMA_READ,
            kallipai_archeion_common::perm::ARCHEION_TAGMA_WRITE,
        ]);
        let nothing: &[&str] = &[];

        let err = list_tagmata(State(state.clone()), scoped(nothing))
            .await
            .expect_err("list without the read string");
        assert_eq!(err.status, 403);
        let _ = list_tagmata(State(state.clone()), read_only.clone())
            .await
            .expect("list with the read string");

        let err = get_tagma(
            State(state.clone()),
            scoped(nothing),
            axum::extract::Path(tagma_id.to_string()),
        )
        .await
        .map(|_| ())
        .expect_err("read without the read string");
        assert_eq!(err.status, 403);
        let _ = get_tagma(
            State(state.clone()),
            read_only.clone(),
            axum::extract::Path(tagma_id.to_string()),
        )
        .await
        .expect("read with the read string");

        let rename = || {
            axum::Json(RenameTagmaRequest {
                label: Some("renamed".to_string()),
            })
        };
        let err = rename_tagma(
            State(state.clone()),
            read_only.clone(),
            axum::extract::Path(tagma_id.to_string()),
            rename(),
        )
        .await
        .expect_err("rename with only the read string");
        assert_eq!(err.status, 403);
        rename_tagma(
            State(state.clone()),
            write.clone(),
            axum::extract::Path(tagma_id.to_string()),
            rename(),
        )
        .await
        .expect("rename with the write string");

        let err = revoke_tagma(
            State(state.clone()),
            read_only.clone(),
            axum::extract::Path(tagma_id.to_string()),
        )
        .await
        .expect_err("revoke with only the read string");
        assert_eq!(err.status, 403);
        revoke_tagma(
            State(state.clone()),
            write.clone(),
            axum::extract::Path(tagma_id.to_string()),
        )
        .await
        .expect("revoke with the write string");

        let err = mint(State(state.clone()), scoped(nothing))
            .await
            .map(|_| ())
            .expect_err("mint without the write string");
        assert_eq!(err.status, 403);
        let err = mint(State(state.clone()), read_only)
            .await
            .map(|_| ())
            .expect_err("mint with only the read string");
        assert_eq!(err.status, 403);
        assert_eq!(
            err.code.as_deref(),
            Some("perm_missing"),
            "a personal token without the write string names the missing one"
        );
        let minted = mint(State(state), write)
            .await
            .expect("mint with the write string");
        assert!(minted.0.code.starts_with("sk-enroll-"), "{}", minted.0.code);
    }

    /// The two 403s here differ by `code`: a personal token without the write
    /// string, and the admin bearer -- which carries every right but is not a
    /// user, so it stops at `require_user`. A caller that only sees the status
    /// cannot tell them apart, and only the second has an answer (use a token).
    #[tokio::test]
    async fn mint_forbidden_answers_carry_distinct_codes() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let scoped = |perms: &[&str]| {
            AuthPrincipal::scoped(
                Principal::User(user.clone()),
                kallipai_archeion_common::control_plane::CredentialScope {
                    token: kallipai_archeion_common::ids::TokenId::from("fixture".to_string()),
                    perms: kallipai_archeion_common::perm::PermSet::from_known(
                        perms.iter().copied(),
                    )
                    .expect("known strings"),
                },
            )
        };
        let nothing: &[&str] = &[];

        let err = mint(State(state.clone()), scoped(nothing))
            .await
            .map(|_| ())
            .expect_err("no write string");
        assert_eq!(err.status, 403);
        assert_eq!(
            err.code.as_deref(),
            Some(kallipai_common::protocol::error::FORBIDDEN_PERM_MISSING)
        );

        let err = mint(State(state), AuthPrincipal::unscoped(Principal::Admin))
            .await
            .map(|_| ())
            .expect_err("the admin bearer is not a user");
        assert_eq!(err.status, 403);
        assert_eq!(
            err.code.as_deref(),
            Some(kallipai_common::protocol::error::FORBIDDEN_USER_REQUIRED)
        );
    }

    /// A mint through a personal token writes a `mint` row on that credential:
    /// which token asked, on whose behalf, and for which pending tagma. The
    /// pending row and the audit row share one transaction, so a successful
    /// mint leaves both; the test below stages the rollback a failure needs.
    #[tokio::test]
    async fn mint_records_the_credential_that_asked() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let perms = kallipai_archeion_common::perm::PermSet::from_known([
            kallipai_archeion_common::perm::ARCHEION_TAGMA_READ,
            kallipai_archeion_common::perm::ARCHEION_TAGMA_WRITE,
        ])
        .expect("known strings");
        let token = crate::credentials::create(
            &state.db,
            crate::token::PAT,
            crate::credentials::NewCredential {
                owner: crate::credentials::Owner::User,
                owner_id: user.as_ref(),
                name: "laptop",
                perms: &perms,
                created_by: user.as_ref(),
                actor: Some(user.as_ref()),
                expires_at: None,
            },
        )
        .await
        .expect("seed a personal token");
        let Json(MintResponse { id, .. }) = mint(
            State(state.clone()),
            AuthPrincipal::scoped(
                Principal::User(user.clone()),
                kallipai_archeion_common::control_plane::CredentialScope {
                    token: kallipai_archeion_common::ids::TokenId::from(token.record.id.clone()),
                    perms,
                },
            ),
        )
        .await
        .expect("mint with the write string");
        let trail = crate::credentials::events(&state.db, &token.record.id)
            .await
            .expect("events");
        let row = trail
            .iter()
            .find(|e| e.kind == crate::credentials::EVENT_MINT)
            .expect("a mint row on the credential that asked");
        assert_eq!(row.target_id.as_deref(), Some(id.as_str()));
        assert_eq!(row.actor_user_id.as_deref(), Some(user.as_ref()));
        let pending = tagmata::Entity::find_by_id(id.clone())
            .one(&state.db)
            .await
            .expect("query")
            .expect("the pending row committed with its audit row");
        assert_eq!(pending.owner_user_id, user.to_string());
    }

    /// The half a successful mint cannot show: when the audit row cannot
    /// land, the pending row goes with it, so no slot is spent on a code
    /// nobody receives. The foreign key on `credential_events.token_id` stages
    /// the failure, and the transaction is what undoes the insert.
    #[tokio::test]
    async fn a_mint_whose_audit_row_cannot_land_leaves_no_pending_row() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let err = match mint_pending_tagma(
            &state,
            &user,
            Some(MintAudit {
                token_id: "no-such-token".to_owned(),
                actor: user.to_string(),
            }),
        )
        .await
        {
            Ok(_) => panic!("a token row that does not exist fails the audit insert"),
            Err(err) => err,
        };
        assert_eq!(err.status, 500);
        let pending = tagmata::Entity::find()
            .filter(tagmata::Column::OwnerUserId.eq(user.to_string()))
            .all(&state.db)
            .await
            .expect("query");
        assert!(
            pending.is_empty(),
            "a rolled-back mint leaves no pending row"
        );
    }
}
