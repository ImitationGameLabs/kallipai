//! Admin (operator) endpoints, authenticated by the admin token. Mint a
//! pending tagma (an enrollment code) on a user's behalf, and manage users +
//! passkeys (list/disable/enable users, list/revoke passkeys). The user-facing
//! self-service counterpart to the tagma mint is `POST /v1/tagmata`
//! (`routes/tagmata.rs`); the admin mint here is retained for operator use.
//!
//! User accounts are created via open signup (passkey or OAuth), so there is no
//! admin user-creation endpoint.
//!
//! All request/response DTOs live in `kallip_agora_common::admin` so the
//! `kallip-agora-client` (admin CLI) shares one wire contract with the server.

use crate::db::entity::{emails, passkeys, users};
use crate::db::map_db_err;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use base64::Engine as _;
use kallip_agora_common::admin::{
    CreateEnrollmentCodeRequest, CreateEnrollmentCodeResponse, Page, PageQuery, PasskeySummary,
    UpdateUserRequest, UserSummary,
};
use kallip_agora_common::ids::UserId;
use kallip_common::protocol::ApiError;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use std::collections::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::{AuthPrincipal, require_admin};
use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
    Router::new()
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

/// `GET /v1/admin` -- admin-gated probe. Returns 200 only for a valid
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
/// timestamp, breaks ties into a strict total order (the user id). `split_once('|')`
/// in [`decode_cursor`] means an anchor containing
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
    // Batch-fetch the primary email for each user on the page (avoids an N+1
    // per-row lookup). Users without an email are absent from the map.
    let page_ids: Vec<String> = page_rows.iter().map(|r| r.id.clone()).collect();
    let primaries = primary_emails_by_account(&state.db, &page_ids).await?;
    let items = page_rows
        .into_iter()
        .map(|r| {
            let primary_email = primaries.get(&r.id).cloned();
            user_summary_from_row(r, primary_email)
        })
        .collect();
    Ok(Json(Page { items, next_cursor }))
}

fn user_summary_from_row(r: users::Model, primary_email: Option<String>) -> UserSummary {
    UserSummary {
        id: r.id,
        username: r.username,
        primary_email,
        display_name: r.display_name,
        created_at: r.created_at,
        disabled_at: r.disabled_at,
    }
}

/// Fetch the primary (`is_primary = true`) address for each of `account_ids`,
/// returning a map keyed by account id. Accounts with no primary email are
/// simply absent. Used to populate `UserSummary.primary_email` without an N+1.
async fn primary_emails_by_account(
    db: &crate::db::Db,
    account_ids: &[String],
) -> Result<HashMap<String, String>, ApiError> {
    if account_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = emails::Entity::find()
        .filter(emails::Column::IsPrimary.eq(true))
        .filter(emails::Column::AccountId.is_in(account_ids.to_vec()))
        .all(db)
        .await
        .map_err(map_db_err)?;
    Ok(rows
        .into_iter()
        .map(|e| (e.account_id, e.address))
        .collect())
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
    let refreshed = users::Entity::find_by_id(id.clone())
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown user"))?;
    let primary_email = primary_emails_by_account(&state.db, &[id])
        .await?
        .into_values()
        .next();
    Ok(Json(user_summary_from_row(refreshed, primary_email)))
}

// ---------------------------------------------------------------------------
// passkeys (list per user + revoke). The live `passkeys` table holds only
// active credentials; revoke hard-deletes the row and appends to
// `passkey_revocations` via the shared `revoke_passkey_row` helper. Admin
// force-revokes (`allow_last = true`); a now-absent id is a 404.
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
    let rows = super::passkeys::list_user_passkey_rows(&state.db, &UserId::from(user_id)).await?;
    let items = rows
        .into_iter()
        .map(super::passkeys::row_to_summary)
        .collect();
    Ok(Json(items))
}

async fn revoke_passkey(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_admin(&principal)?;
    // Resolve the owner so the shared helper can lock their live set. A truly
    // unknown id (no live row) is a 404 for the operator; a concurrently-revoked
    // row is an idempotent 204 inside the helper.
    let row = passkeys::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown passkey"))?;
    super::passkeys::revoke_passkey_row(
        &state,
        &UserId::from(row.user_id),
        id,
        true,
        crate::db::entity::passkey_revocations::REASON_REVOKED,
        crate::db::entity::passkey_revocations::REVOKED_BY_ADMIN,
    )
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    //! The legacy enrollment-code mint (which validates the user against the DB)
    //! and the user/passkey management endpoints.

    use axum::Json;
    use axum::extract::{Path, Query, State};

    use super::{
        admin_root, create_enrollment_code, list_user_passkeys, list_users, revoke_passkey,
        update_user,
    };
    use crate::auth::{AuthPrincipal, Principal};
    use crate::db::entity::passkeys;
    use crate::state::SharedState;
    use crate::test_helpers::{make_state, seed_user};
    use kallip_agora_common::admin::{CreateEnrollmentCodeRequest, PageQuery, UpdateUserRequest};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};
    use time::OffsetDateTime;
    use uuid::Uuid;

    #[tokio::test]
    async fn admin_root_requires_admin() {
        let state = make_state().await;
        // A non-admin principal (a regular user) is rejected with 403.
        let user_id = seed_user(&state, "u").await;
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
        let user_id = seed_user(&state, "owner").await;
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
            label: Set("Device".to_string()),
            created_at: Set(OffsetDateTime::now_utc()),
            last_used_at: Set(OffsetDateTime::now_utc()),
            discoverable: Set(false),
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
            all.push(seed_user(&state, &format!("u{i}")).await.to_string());
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
        let user_id = seed_user(&state, "owner").await;

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
        let user_id = seed_user(&state, "owner").await;
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
        assert_eq!(items[0].label, "Device");

        // Unknown user -> 404.
        match list_user_passkeys(State(state), admin, Path("no-such-user".to_string())).await {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("unknown user must be rejected"),
        }
    }

    /// Admin revoking a passkey hard-deletes it (force-revoke; the last-passkey
    /// guard is overridden). A second revoke of the now-absent id is a 404.
    /// Revoking an unknown passkey is 404.
    #[tokio::test]
    async fn revoke_passkey_hard_deletes_and_404s_when_gone() {
        let state = make_state().await;
        let admin = AuthPrincipal(Principal::Admin);
        let user_id = seed_user(&state, "owner").await;
        let pk_id = seed_passkey(&state, user_id.as_ref()).await;

        let status = revoke_passkey(State(state.clone()), admin.clone(), Path(pk_id))
            .await
            .expect("first revoke");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        // The live list no longer contains it.
        let items = list_user_passkeys(
            State(state.clone()),
            admin.clone(),
            Path(user_id.to_string()),
        )
        .await
        .expect("list")
        .0;
        assert!(items.is_empty());

        // Second revoke: the id is gone -> 404.
        match revoke_passkey(State(state.clone()), admin, Path(pk_id)).await {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("already-revoked passkey must 404 for admin"),
        }

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
