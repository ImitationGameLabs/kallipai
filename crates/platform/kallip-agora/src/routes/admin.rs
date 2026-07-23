//! Admin (operator) endpoints, authenticated by the admin token. The
//! invite-only entry point: mint + list + revoke invite codes, mint a pending
//! tagma (an enrollment code) on a user's behalf, and manage users + passkeys
//! (list/disable/enable users, list/revoke passkeys). The user-facing self-
//! service counterpart to the tagma mint is `POST /v1/tagmata`
//! (`routes/tagmata.rs`); the admin mint here is retained for operator use.
//!
//! User accounts are born ONLY at invite redemption + passkey binding, so there
//! is no admin user-creation endpoint.
//!
//! All request/response DTOs live in `kallip_agora_common::admin` so the
//! `kallip-agora-client` (admin CLI) shares one wire contract with the server.

use crate::db::entity::{invite_codes, passkeys, users};
use crate::db::map_db_err;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use base64::Engine as _;
use kallip_agora_common::admin::{
    CreateEnrollmentCodeRequest, CreateEnrollmentCodeResponse, CreateInviteCodeRequest, InviteCode,
    InviteCodeSummary, Page, PageQuery, PasskeySummary, UpdateUserRequest, UserSummary,
};
use kallip_agora_common::ids::UserId;
use kallip_common::authtoken::MintedToken;
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::{AuthPrincipal, require_admin};
use crate::state::SharedState;
use crate::token::INVITE;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route(
            "/invite-codes",
            post(create_invite_code).get(list_invite_codes),
        )
        .route(
            "/invite-codes/{code_hash_hex}",
            axum::routing::delete(revoke_invite_code),
        )
        .route("/tagmata", post(create_enrollment_code))
        .route("/users", get(list_users))
        .route("/users/{id}", axum::routing::patch(update_user))
        .route("/users/{id}/passkeys", get(list_user_passkeys))
        .route("/passkeys/{id}", axum::routing::delete(revoke_passkey))
        // The admin nest root. It is also the admin CLI's auth probe, so it MUST
        // be admin-gated: a bare handler here would make `kallip-admin ping`
        // report any token as valid.
        .route("/", get(admin_root))
}

/// `GET /v1/admin/` -- admin-gated probe. Returns 200 only for a valid
/// `sk-admin-` bearer; any other credential is rejected by the extractor
/// (`AuthPrincipal`) or by `require_admin`.
async fn admin_root(AuthPrincipal(principal): AuthPrincipal) -> Result<&'static str, ApiError> {
    require_admin(&principal)?;
    Ok("kallip-agora admin")
}

// ---------------------------------------------------------------------------
// shared pagination helpers
// ---------------------------------------------------------------------------

/// Page size bounds for the paginated admin list endpoints.
const DEFAULT_PAGE: u64 = 100;
const MAX_PAGE: u64 = 500;

/// Encode the `(created_at, anchor)` tuple for the last row of a page into an
/// opaque cursor. `anchor` is a stable per-row string that, combined with the
/// timestamp, breaks ties into a strict total order (the invite hash hex, or the
/// user id). `split_once('|')` in [`decode_cursor`] means an anchor containing
/// `|` still round-trips byte-for-byte, but the separators are kept out of
/// anchors by construction (hex / UUID), and the nanos prefix never contains it.
fn encode_cursor(ts: OffsetDateTime, anchor: &str) -> String {
    let plain = format!("{}|{}", ts.unix_timestamp_nanos(), anchor);
    base64::engine::general_purpose::STANDARD.encode(plain)
}

/// Decode an opaque cursor back into its `(created_at, anchor)` tuple.
fn decode_cursor(s: &str) -> Result<(OffsetDateTime, String), ApiError> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let plain = String::from_utf8(decoded).map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let (nanos, anchor) = plain
        .split_once('|')
        .ok_or_else(|| ApiError::bad_request("invalid cursor"))?;
    let nanos: i128 = nanos
        .parse()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let ts = OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    Ok((ts, anchor.to_string()))
}

// ---------------------------------------------------------------------------
// invite codes
// ---------------------------------------------------------------------------

async fn create_invite_code(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(req): Json<CreateInviteCodeRequest>,
) -> Result<Json<InviteCode>, ApiError> {
    require_admin(&principal)?;
    let ttl = Duration::from_secs(req.ttl_secs.unwrap_or(state.limits.invite_default_ttl_secs));
    let code = MintedToken::generate(INVITE);
    let now = OffsetDateTime::now_utc();
    let hash_bytes = code.hash().as_bytes().to_vec();
    let am = invite_codes::ActiveModel {
        code_hash: Set(hash_bytes.clone()),
        created_at: Set(now),
        expires_at: Set(now + ttl),
        consumed_at: Set(None),
        consumed_by: Set(None),
        note: Set(req.note.clone()),
        revoked_at: Set(None),
    };
    am.insert(&state.db).await.map_err(map_db_err)?;
    Ok(Json(InviteCode {
        code: code.secret().to_string(),
        code_hash_hex: hex::encode(&hash_bytes),
        note: req.note,
        expires_at: now + ttl,
    }))
}

async fn list_invite_codes(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Query(query): Query<PageQuery>,
) -> Result<Json<Page<InviteCodeSummary>>, ApiError> {
    require_admin(&principal)?;
    // Order by (created_at DESC, code_hash DESC); the PK code_hash makes the
    // tuple a stable cursor even when rows share a timestamp.
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let mut select = invite_codes::Entity::find()
        .order_by_desc(invite_codes::Column::CreatedAt)
        .order_by_desc(invite_codes::Column::CodeHash);
    if let Some(cursor) = &query.cursor {
        // Resume strictly after the anchor: rows that sort before it in DESC
        // order (i.e. older, or same-ts with a smaller hash).
        let (ts, anchor_hex) = decode_cursor(cursor)?;
        let hash = hex::decode(&anchor_hex).map_err(|_| ApiError::bad_request("invalid cursor"))?;
        select = select.filter(
            sea_orm::Condition::any()
                .add(invite_codes::Column::CreatedAt.lt(ts))
                .add(
                    sea_orm::Condition::all()
                        .add(invite_codes::Column::CreatedAt.eq(ts))
                        .add(invite_codes::Column::CodeHash.lt(hash)),
                ),
        );
    }
    // Fetch one extra to detect a following page without a second query.
    let rows = select
        .limit(limit + 1)
        .all(&state.db)
        .await
        .map_err(map_db_err)?;
    let has_more = rows.len() as u64 > limit;
    let page_rows: Vec<_> = rows.into_iter().take(limit as usize).collect();
    // The cursor anchors on the last row of THIS page; the resume filter is
    // "strictly before", so the next page starts right after it.
    let next_cursor = if has_more {
        let last = page_rows
            .last()
            .expect("a page with a follower is non-empty");
        Some(encode_cursor(
            last.created_at,
            &hex::encode(&last.code_hash),
        ))
    } else {
        None
    };
    let items = page_rows.into_iter().map(invite_summary_from_row).collect();
    Ok(Json(Page { items, next_cursor }))
}

fn invite_summary_from_row(r: invite_codes::Model) -> InviteCodeSummary {
    InviteCodeSummary {
        code_hash_hex: hex::encode(&r.code_hash),
        created_at: r.created_at,
        expires_at: r.expires_at,
        consumed_at: r.consumed_at,
        consumed_by: r.consumed_by,
        note: r.note,
        revoked_at: r.revoked_at,
    }
}

async fn revoke_invite_code(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(code_hash_hex): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_admin(&principal)?;
    // A SHA-256 hash is exactly 64 hex chars; reject anything else before
    // decoding (a path segment has no body-limit cap).
    if code_hash_hex.len() != 64 {
        return Err(ApiError::bad_request("code_hash_hex must be 64 hex chars"));
    }
    let hash = hex::decode(&code_hash_hex)
        .map_err(|_| ApiError::bad_request("code_hash_hex must be hex"))?;
    let row = invite_codes::Entity::find()
        .filter(invite_codes::Column::CodeHash.eq(hash.clone()))
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown invite code"))?;
    // Idempotent AND race-free: a conditional UPDATE only touches rows whose
    // `revoked_at` is still NULL, so two concurrent revokes cannot clobber the
    // first-revoked timestamp. A row already revoked (by this call or a racing
    // one) is left with its original timestamp. 204 either way.
    if row.revoked_at.is_none() {
        let now = OffsetDateTime::now_utc();
        invite_codes::Entity::update_many()
            .filter(invite_codes::Column::CodeHash.eq(hash))
            .filter(invite_codes::Column::RevokedAt.is_null())
            .col_expr(
                invite_codes::Column::RevokedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .exec(&state.db)
            .await
            .map_err(map_db_err)?;
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// enrollment codes (operator mint of a pending tagma on a user's behalf; users
// self-mint via POST /v1/tagmata). Reuses the shared `mint_pending_tagma` so the
// per-owner live-pending cap applies uniformly.
// ---------------------------------------------------------------------------

async fn create_enrollment_code(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(req): Json<CreateEnrollmentCodeRequest>,
) -> Result<Json<CreateEnrollmentCodeResponse>, ApiError> {
    require_admin(&principal)?;
    let user_id = UserId::from(req.user_id);
    // Users live in the durable store. Admin surface (not public): distinguish
    // the two cases clearly. Unknown user -> 404, disabled user -> 409.
    let user = users::Entity::find_by_id(user_id.to_string())
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    let Some(user) = user else {
        return Err(ApiError::not_found("unknown user_id"));
    };
    if user.disabled_at.is_some() {
        return Err(ApiError::conflict("user is disabled"));
    }
    let (_id, plaintext, _created_at, _expires_at) =
        super::tagmata::mint_pending_tagma(&state, &user_id).await?;
    Ok(Json(CreateEnrollmentCodeResponse { code: plaintext }))
}

// ---------------------------------------------------------------------------
// users (list + disable/enable). The `disabled_at` column is enforced on every
// auth path (login, session, enroll); these endpoints are the missing write
// side. No admin user-creation by design.
// ---------------------------------------------------------------------------

async fn list_users(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Query(query): Query<PageQuery>,
) -> Result<Json<Page<UserSummary>>, ApiError> {
    require_admin(&principal)?;
    // Order by (created_at DESC, id DESC); the TEXT PK makes the tuple a stable
    // cursor even when rows share a timestamp.
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let mut select = users::Entity::find()
        .order_by_desc(users::Column::CreatedAt)
        .order_by_desc(users::Column::Id);
    if let Some(cursor) = &query.cursor {
        let (ts, anchor) = decode_cursor(cursor)?;
        select = select.filter(
            sea_orm::Condition::any()
                .add(users::Column::CreatedAt.lt(ts))
                .add(
                    sea_orm::Condition::all()
                        .add(users::Column::CreatedAt.eq(ts))
                        .add(users::Column::Id.lt(anchor)),
                ),
        );
    }
    let rows = select
        .limit(limit + 1)
        .all(&state.db)
        .await
        .map_err(map_db_err)?;
    let has_more = rows.len() as u64 > limit;
    let page_rows: Vec<_> = rows.into_iter().take(limit as usize).collect();
    let next_cursor = if has_more {
        let last = page_rows
            .last()
            .expect("a page with a follower is non-empty");
        Some(encode_cursor(last.created_at, &last.id))
    } else {
        None
    };
    let items = page_rows.into_iter().map(user_summary_from_row).collect();
    Ok(Json(Page { items, next_cursor }))
}

fn user_summary_from_row(r: users::Model) -> UserSummary {
    UserSummary {
        id: r.id,
        username: r.username,
        email: r.email,
        display_name: r.display_name,
        created_at: r.created_at,
        disabled_at: r.disabled_at,
    }
}

async fn update_user(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<UserSummary>, ApiError> {
    require_admin(&principal)?;
    // Existence check first -> 404 for an unknown user (PATCH on a missing row
    // would otherwise silently 200 with a synthesized summary).
    let _ = users::Entity::find_by_id(id.clone())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown user"))?;
    if req.disabled {
        // Conditional: only set disabled_at where it is still NULL, so a repeated
        // disable (or a racing one) cannot clobber the first-disabled timestamp.
        users::Entity::update_many()
            .filter(users::Column::Id.eq(id.clone()))
            .filter(users::Column::DisabledAt.is_null())
            .col_expr(
                users::Column::DisabledAt,
                sea_orm::sea_query::Expr::value(OffsetDateTime::now_utc()),
            )
            .exec(&state.db)
            .await
            .map_err(map_db_err)?;
    } else {
        // Re-enable: clear disabled_at unconditionally.
        users::Entity::update_many()
            .filter(users::Column::Id.eq(id.clone()))
            .col_expr(
                users::Column::DisabledAt,
                sea_orm::sea_query::Expr::value(None::<OffsetDateTime>),
            )
            .exec(&state.db)
            .await
            .map_err(map_db_err)?;
    }
    // update_many returns rows-affected, not the row; re-read to return the
    // refreshed (idempotent) state.
    let refreshed = users::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown user"))?;
    Ok(Json(user_summary_from_row(refreshed)))
}

// ---------------------------------------------------------------------------
// passkeys (list per user + revoke). A compromised passkey is already filtered
// out of `login_begin`; revoke here marks `compromised_at` so the credential can
// no longer authenticate, forcing the user to re-register.
// ---------------------------------------------------------------------------

async fn list_user_passkeys(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(user_id): Path<String>,
) -> Result<Json<Vec<PasskeySummary>>, ApiError> {
    require_admin(&principal)?;
    // 404 if the user doesn't exist, keeping the existence-oracle consistent
    // with create_enrollment_code above.
    let _ = users::Entity::find_by_id(user_id.clone())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown user"))?;
    let rows = passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id))
        .order_by_asc(passkeys::Column::CreatedAt)
        .all(&state.db)
        .await
        .map_err(map_db_err)?;
    let items = rows
        .into_iter()
        .map(|r| PasskeySummary {
            id: r.id.to_string(),
            created_at: r.created_at,
            compromised_at: r.compromised_at,
        })
        .collect();
    Ok(Json(items))
}

async fn revoke_passkey(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_admin(&principal)?;
    let row = passkeys::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown passkey"))?;
    // Idempotent + race-free, mirroring revoke_invite_code: only touch rows whose
    // compromised_at is still NULL, so the first-revoked timestamp is preserved.
    if row.compromised_at.is_none() {
        passkeys::Entity::update_many()
            .filter(passkeys::Column::Id.eq(row.id))
            .filter(passkeys::Column::CompromisedAt.is_null())
            .col_expr(
                passkeys::Column::CompromisedAt,
                sea_orm::sea_query::Expr::value(OffsetDateTime::now_utc()),
            )
            .exec(&state.db)
            .await
            .map_err(map_db_err)?;
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    //! Admin invite-code CRUD round-trip, the legacy enrollment-code mint (which
    //! validates the user against the DB), and the user/passkey management
    //! endpoints.

    use axum::Json;
    use axum::extract::{Path, Query, State};

    use super::{
        admin_root, create_enrollment_code, create_invite_code, list_invite_codes,
        list_user_passkeys, list_users, revoke_invite_code, revoke_passkey, update_user,
    };
    use crate::auth::{AuthPrincipal, Principal};
    use crate::db::entity::passkeys;
    use crate::state::SharedState;
    use crate::test_helpers::{make_state, seed_user};
    use kallip_agora_common::admin::{
        CreateEnrollmentCodeRequest, CreateInviteCodeRequest, InviteCodeSummary, Page, PageQuery,
        UpdateUserRequest,
    };
    use kallip_common::authtoken::TokenHash;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};
    use time::OffsetDateTime;
    use uuid::Uuid;

    #[tokio::test]
    async fn admin_root_requires_admin() {
        let state = make_state().await;
        // A non-admin principal (a regular user) is rejected with 403.
        let user_id = seed_user(&state, "u", "u@example.test").await;
        let err = admin_root(AuthPrincipal(Principal::User(user_id)))
            .await
            .expect_err("non-admin must be rejected");
        assert_eq!(err.status, 403);
        // The admin principal passes.
        assert_eq!(
            admin_root(AuthPrincipal(Principal::Admin))
                .await
                .expect("admin passes"),
            "kallip-agora admin"
        );
    }

    #[tokio::test]
    async fn invite_code_crud_round_trip() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);

        // Mint one invite.
        let created = create_invite_code(
            State(state.clone()),
            admin.clone(),
            Json(CreateInviteCodeRequest {
                ttl_secs: Some(3600),
                note: Some("ops".to_string()),
            }),
        )
        .await
        .expect("create")
        .0;
        // The plaintext hashes to the returned hex.
        let expected_hex = hex::encode(TokenHash::of(&created.code).as_bytes());
        assert_eq!(created.code_hash_hex, expected_hex);

        // List shows it, unconsumed, with the note.
        let Page {
            items: listed,
            next_cursor,
        } = list_invite_codes(
            State(state.clone()),
            admin.clone(),
            Query(PageQuery::default()),
        )
        .await
        .expect("list")
        .0;
        assert_eq!(listed.len(), 1);
        assert!(next_cursor.is_none());
        let InviteCodeSummary {
            consumed_at,
            revoked_at,
            note,
            ..
        } = &listed[0];
        assert!(consumed_at.is_none());
        assert!(revoked_at.is_none());
        assert_eq!(note.as_deref(), Some("ops"));

        // Revoke by hex hash -> 204, then the row is revoked.
        let status = revoke_invite_code(
            State(state.clone()),
            admin.clone(),
            Path(created.code_hash_hex.clone()),
        )
        .await
        .expect("revoke");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        let Page { items: listed, .. } =
            list_invite_codes(State(state.clone()), admin, Query(PageQuery::default()))
                .await
                .expect("list")
                .0;
        assert!(listed[0].revoked_at.is_some());
    }

    /// The legacy enrollment-code mint now rejects an unknown user (users live
    /// in the DB; there is no longer an in-memory index).
    #[tokio::test]
    async fn enrollment_code_rejects_unknown_user() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        match create_enrollment_code(
            State(state),
            admin,
            Json(CreateEnrollmentCodeRequest {
                user_id: "no-such-user".to_string(),
            }),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("unknown user must be rejected"),
        }
    }

    /// A known user can be minted an enrollment code for.
    #[tokio::test]
    async fn enrollment_code_for_known_user() {
        let state = make_state().await;
        let user_id = seed_user(&state, "owner", "owner@example.test").await;
        let admin = AuthPrincipal(Principal::Admin);
        let resp = create_enrollment_code(
            State(state),
            admin,
            Json(CreateEnrollmentCodeRequest {
                user_id: user_id.to_string(),
            }),
        )
        .await
        .expect("create")
        .0;
        assert!(resp.code.starts_with("sk-enroll-"));
    }

    /// Revoke is idempotent: a second revoke returns 204 and does not clobber
    /// the original `revoked_at` (audit-relevant).
    #[tokio::test]
    async fn revoke_is_idempotent_preserving_timestamp() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        let created = create_invite_code(
            State(state.clone()),
            admin.clone(),
            Json(CreateInviteCodeRequest::default()),
        )
        .await
        .expect("create")
        .0;
        let hex = created.code_hash_hex.clone();

        revoke_invite_code(State(state.clone()), admin.clone(), Path(hex.clone()))
            .await
            .expect("first revoke");
        let first = list_invite_codes(
            State(state.clone()),
            admin.clone(),
            Query(PageQuery::default()),
        )
        .await
        .expect("list")
        .0
        .items[0]
            .revoked_at
            .expect("revoked once");

        revoke_invite_code(State(state.clone()), admin.clone(), Path(hex))
            .await
            .expect("second revoke");
        let second = list_invite_codes(State(state.clone()), admin, Query(PageQuery::default()))
            .await
            .expect("list")
            .0
            .items[0]
            .revoked_at
            .expect("still revoked");
        assert_eq!(first, second, "revoked_at must not be clobbered");
    }

    /// A non-64-hex path segment is rejected before any decode or DB work.
    #[tokio::test]
    async fn revoke_rejects_bad_hex_length() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        match revoke_invite_code(State(state), admin, Path("deadbeef".to_string())).await {
            Err(e) => assert_eq!(e.status, 400),
            Ok(_) => panic!("short hex must be rejected"),
        }
    }

    /// `list_invite_codes` paginates: a full first page yields a cursor, the
    /// second page returns the remainder with no cursor, and concatenation
    /// reconstructs the whole set.
    #[tokio::test]
    async fn list_invite_codes_paginates() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        // Mint 3 codes with a 2-per-page limit.
        let mut all = Vec::new();
        for _ in 0..3 {
            let created = create_invite_code(
                State(state.clone()),
                admin.clone(),
                Json(CreateInviteCodeRequest::default()),
            )
            .await
            .expect("create")
            .0;
            all.push(created.code_hash_hex);
        }

        let page1 = list_invite_codes(
            State(state.clone()),
            admin.clone(),
            Query(PageQuery {
                limit: Some(2),
                cursor: None,
            }),
        )
        .await
        .expect("page1")
        .0;
        assert_eq!(page1.items.len(), 2);
        let cursor = page1.next_cursor.expect("a following page exists");

        let page2 = list_invite_codes(
            State(state.clone()),
            admin,
            Query(PageQuery {
                limit: Some(2),
                cursor: Some(cursor),
            }),
        )
        .await
        .expect("page2")
        .0;
        assert_eq!(page2.items.len(), 1);
        assert!(page2.next_cursor.is_none());

        let mut seen: Vec<String> = page1
            .items
            .into_iter()
            .chain(page2.items)
            .map(|s| s.code_hash_hex)
            .collect();
        seen.sort();
        all.sort();
        assert_eq!(seen, all, "pages cover the full set with no dupes");
    }

    // --- users / passkeys --------------------------------------------------

    /// Insert a passkey row for `user_id` (test fixture; the credential JSONB is
    /// a placeholder — the admin surface never reads it).
    async fn seed_passkey(state: &SharedState, user_id: &str) -> Uuid {
        let id = Uuid::new_v4();
        passkeys::ActiveModel {
            id: Set(id),
            user_id: Set(user_id.to_string()),
            cred_id: Set(vec![1, 2, 3]),
            credential: Set(serde_json::json!({})),
            created_at: Set(OffsetDateTime::now_utc()),
            compromised_at: Set(None),
        }
        .insert(&state.db)
        .await
        .expect("insert passkey");
        id
    }

    /// `list_users` paginates and returns seeded users.
    #[tokio::test]
    async fn users_list_paginates() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        let mut all = Vec::new();
        for i in 0..3 {
            all.push(
                seed_user(&state, &format!("u{i}"), &format!("u{i}@example.test"))
                    .await
                    .to_string(),
            );
        }

        let page1 = list_users(
            State(state.clone()),
            admin.clone(),
            Query(PageQuery {
                limit: Some(2),
                cursor: None,
            }),
        )
        .await
        .expect("page1")
        .0;
        assert_eq!(page1.items.len(), 2);
        let cursor = page1.next_cursor.expect("a following page exists");

        let page2 = list_users(
            State(state.clone()),
            admin,
            Query(PageQuery {
                limit: Some(2),
                cursor: Some(cursor),
            }),
        )
        .await
        .expect("page2")
        .0;
        assert_eq!(page2.items.len(), 1);
        assert!(page2.next_cursor.is_none());

        let mut seen: Vec<String> = page1
            .items
            .into_iter()
            .chain(page2.items)
            .map(|s| s.id)
            .collect();
        seen.sort();
        all.sort();
        assert_eq!(seen, all, "pages cover the full set with no dupes");
    }

    /// Disabling then re-enabling a user round-trips through `disabled_at`,
    /// and the disable is idempotent (the timestamp is preserved).
    #[tokio::test]
    async fn disable_then_enable_user_round_trips() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        let user_id = seed_user(&state, "owner", "owner@example.test").await;

        // Disable -> disabled_at is set.
        let disabled = update_user(
            State(state.clone()),
            admin.clone(),
            Path(user_id.to_string()),
            Json(UpdateUserRequest { disabled: true }),
        )
        .await
        .expect("disable")
        .0;
        let first_disabled_at = disabled.disabled_at.expect("disabled");

        // Disabling again returns the same timestamp (idempotent).
        let disabled_again = update_user(
            State(state.clone()),
            admin.clone(),
            Path(user_id.to_string()),
            Json(UpdateUserRequest { disabled: true }),
        )
        .await
        .expect("disable again")
        .0;
        assert_eq!(
            disabled_again.disabled_at,
            Some(first_disabled_at),
            "disabled_at must not be clobbered"
        );

        // Re-enable -> disabled_at cleared.
        let enabled = update_user(
            State(state.clone()),
            admin,
            Path(user_id.to_string()),
            Json(UpdateUserRequest { disabled: false }),
        )
        .await
        .expect("enable")
        .0;
        assert!(enabled.disabled_at.is_none(), "re-enabled user is active");
    }

    /// PATCH on an unknown user returns 404.
    #[tokio::test]
    async fn update_unknown_user_404() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        match update_user(
            State(state),
            admin,
            Path("no-such-user".to_string()),
            Json(UpdateUserRequest { disabled: true }),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("unknown user must be rejected"),
        }
    }

    /// `list_user_passkeys` returns a seeded passkey, and 404s for an unknown
    /// user.
    #[tokio::test]
    async fn list_passkeys_for_user() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        let user_id = seed_user(&state, "owner", "owner@example.test").await;
        let pk_id = seed_passkey(&state, user_id.as_ref()).await;

        let items = list_user_passkeys(
            State(state.clone()),
            admin.clone(),
            Path(user_id.to_string()),
        )
        .await
        .expect("list")
        .0;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, pk_id.to_string());
        assert!(items[0].compromised_at.is_none());

        // Unknown user -> 404.
        match list_user_passkeys(State(state), admin, Path("no-such-user".to_string())).await {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("unknown user must be rejected"),
        }
    }

    /// Revoking a passkey marks it compromised; a second revoke is idempotent
    /// (the timestamp is preserved). Revoking an unknown passkey is 404.
    #[tokio::test]
    async fn revoke_passkey_is_idempotent() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        let user_id = seed_user(&state, "owner", "owner@example.test").await;
        let pk_id = seed_passkey(&state, user_id.as_ref()).await;

        let status = revoke_passkey(State(state.clone()), admin.clone(), Path(pk_id))
            .await
            .expect("first revoke");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        let first = list_user_passkeys(
            State(state.clone()),
            admin.clone(),
            Path(user_id.to_string()),
        )
        .await
        .expect("list")
        .0[0]
            .compromised_at
            .expect("compromised once");

        let status = revoke_passkey(State(state.clone()), admin.clone(), Path(pk_id))
            .await
            .expect("second revoke");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        let second = list_user_passkeys(State(state.clone()), admin, Path(user_id.to_string()))
            .await
            .expect("list")
            .0[0]
            .compromised_at
            .expect("still compromised");
        assert_eq!(first, second, "compromised_at must not be clobbered");

        // Unknown passkey -> 404.
        match revoke_passkey(
            State(make_state().await),
            AuthPrincipal(Principal::Admin),
            Path(Uuid::new_v4()),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("unknown passkey must be rejected"),
        }
    }
}
