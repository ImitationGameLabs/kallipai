//! Admin (operator) endpoints, authenticated by the admin token. The
//! invite-only entry point: mint + list + revoke invite codes, and mint a
//! pending tagma (an enrollment code) on a user's behalf. The user-facing
//! self-service counterpart is `POST /v1/tagmata` (`routes/tagmata.rs`); this
//! admin mint is retained for operator use (e.g. provisioning a code for a user
//! out-of-band).
//!
//! User accounts are born ONLY at invite redemption + passkey binding, so there
//! is no admin user-creation endpoint.

use crate::db::entity::{invite_codes, users};
use crate::db::map_db_err;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use base64::Engine as _;
use kallip_agora_common::ids::UserId;
use kallip_common::authtoken::MintedToken;
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use time::OffsetDateTime;

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
        // A trivial GET on the admin nest so wiring is exercised without a
        // body-bearing call.
        .route("/", get(|| async { "kallip-agora admin" }))
}

// ---------------------------------------------------------------------------
// invite codes
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct CreateInviteCodeRequest {
    #[serde(default)]
    ttl_secs: Option<u64>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Serialize)]
struct InviteCode {
    /// `sk-invite-...` plaintext; returned once. The agora retains only its hash.
    code: String,
    code_hash_hex: String,
    note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

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

#[derive(Serialize)]
struct InviteCodeSummary {
    code_hash_hex: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    consumed_at: Option<OffsetDateTime>,
    consumed_by: Option<String>,
    note: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    revoked_at: Option<OffsetDateTime>,
}

/// Page size bounds for `list_invite_codes`.
const DEFAULT_INVITE_PAGE: u64 = 100;
const MAX_INVITE_PAGE: u64 = 500;

#[derive(Deserialize, Default)]
struct InvitePageQuery {
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Serialize)]
struct InviteCodePage {
    items: Vec<InviteCodeSummary>,
    /// Set when another page follows; pass back as `cursor` to resume. Opaque,
    /// base64 of `<created_at unix nanos>|<code_hash hex>`.
    next_cursor: Option<String>,
}

/// Decode the opaque cursor into its `(created_at, code_hash)` anchor.
fn decode_invite_cursor(s: &str) -> Result<(OffsetDateTime, Vec<u8>), ApiError> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let plain = String::from_utf8(decoded).map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let (nanos_hex, hash_hex) = plain
        .split_once('|')
        .ok_or_else(|| ApiError::bad_request("invalid cursor"))?;
    let nanos: i128 = nanos_hex
        .parse()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let ts = OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let hash = hex::decode(hash_hex).map_err(|_| ApiError::bad_request("invalid cursor"))?;
    Ok((ts, hash))
}

/// Encode the anchor for the last row of a page into an opaque cursor.
fn encode_invite_cursor(ts: OffsetDateTime, hash: &[u8]) -> String {
    let plain = format!("{}|{}", ts.unix_timestamp_nanos(), hex::encode(hash));
    base64::engine::general_purpose::STANDARD.encode(plain)
}

async fn list_invite_codes(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    axum::extract::Query(query): axum::extract::Query<InvitePageQuery>,
) -> Result<Json<InviteCodePage>, ApiError> {
    require_admin(&principal)?;
    // Order by (created_at DESC, code_hash DESC); the PK code_hash makes the
    // tuple a stable cursor even when rows share a timestamp.
    let limit = query
        .limit
        .unwrap_or(DEFAULT_INVITE_PAGE)
        .clamp(1, MAX_INVITE_PAGE);
    let mut select = invite_codes::Entity::find()
        .order_by_desc(invite_codes::Column::CreatedAt)
        .order_by_desc(invite_codes::Column::CodeHash);
    if let Some(cursor) = &query.cursor {
        // Resume strictly after the anchor: rows that sort before it in DESC
        // order (i.e. older, or same-ts with a smaller hash).
        let (ts, hash) = decode_invite_cursor(cursor)?;
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
        Some(encode_invite_cursor(last.created_at, &last.code_hash))
    } else {
        None
    };
    let items = page_rows.into_iter().map(invite_summary_from_row).collect();
    Ok(Json(InviteCodePage { items, next_cursor }))
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

#[derive(Deserialize)]
struct CreateEnrollmentCodeRequest {
    user_id: String,
}

#[derive(Serialize)]
struct CreateEnrollmentCodeResponse {
    /// `sk-enroll-...` single-use, short-TTL; returned once.
    code: String,
}

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

#[cfg(test)]
mod tests {
    //! Admin invite-code CRUD round-trip + the legacy enrollment-code mint
    //! (which now validates the user against the DB).

    use std::time::Duration;

    use axum::Json;
    use axum::extract::State;

    use super::{
        CreateEnrollmentCodeRequest, CreateInviteCodeRequest, InviteCodePage, InviteCodeSummary,
        InvitePageQuery, create_enrollment_code, create_invite_code, list_invite_codes,
        revoke_invite_code,
    };
    use crate::auth::{AuthPrincipal, Principal};
    use crate::test_helpers::{make_state, seed_user};
    use kallip_common::authtoken::TokenHash;

    #[tokio::test]
    async fn invite_code_crud_round_trip() {
        let state = make_state(Duration::from_secs(2)).await;
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
        let InviteCodePage {
            items: listed,
            next_cursor,
        } = list_invite_codes(
            State(state.clone()),
            admin.clone(),
            axum::extract::Query(InvitePageQuery::default()),
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
            axum::extract::Path(created.code_hash_hex.clone()),
        )
        .await
        .expect("revoke");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        let InviteCodePage { items: listed, .. } = list_invite_codes(
            State(state.clone()),
            admin,
            axum::extract::Query(InvitePageQuery::default()),
        )
        .await
        .expect("list")
        .0;
        assert!(listed[0].revoked_at.is_some());
    }

    /// The legacy enrollment-code mint now rejects an unknown user (users live
    /// in the DB; there is no longer an in-memory index).
    #[tokio::test]
    async fn enrollment_code_rejects_unknown_user() {
        let state = make_state(Duration::from_secs(2)).await;
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
        let state = make_state(Duration::from_secs(2)).await;
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
        let state = make_state(Duration::from_secs(2)).await;
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

        revoke_invite_code(
            State(state.clone()),
            admin.clone(),
            axum::extract::Path(hex.clone()),
        )
        .await
        .expect("first revoke");
        let first = list_invite_codes(
            State(state.clone()),
            admin.clone(),
            axum::extract::Query(InvitePageQuery::default()),
        )
        .await
        .expect("list")
        .0
        .items[0]
            .revoked_at
            .expect("revoked once");

        revoke_invite_code(
            State(state.clone()),
            admin.clone(),
            axum::extract::Path(hex),
        )
        .await
        .expect("second revoke");
        let second = list_invite_codes(
            State(state.clone()),
            admin,
            axum::extract::Query(InvitePageQuery::default()),
        )
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
        let state = make_state(Duration::from_secs(2)).await;
        let admin = AuthPrincipal(Principal::Admin);
        match revoke_invite_code(
            State(state),
            admin,
            axum::extract::Path("deadbeef".to_string()),
        )
        .await
        {
            Err(e) => assert_eq!(e.status, 400),
            Ok(_) => panic!("short hex must be rejected"),
        }
    }

    /// `list_invite_codes` paginates: a full first page yields a cursor, the
    /// second page returns the remainder with no cursor, and concatenation
    /// reconstructs the whole set.
    #[tokio::test]
    async fn list_invite_codes_paginates() {
        let state = make_state(Duration::from_secs(2)).await;
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
            axum::extract::Query(InvitePageQuery {
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
            axum::extract::Query(InvitePageQuery {
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
}
