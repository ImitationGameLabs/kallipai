//! Saved AI provider credentials -- the `/me/providers` self-service
//! surface for the mixed-mode provider vault.
//!
//! Each row is one provider credential entry (API key + optional base-URL
//! override) owned by the signed-in account. The `mode` column picks the
//! storage form per entry: a `plaintext` row carries the raw key, an
//! `encrypted` row carries a client-side encrypted blob. `key_material` is
//! opaque to the server in BOTH modes -- encrypt/decrypt work happens in
//! the web client, so no handler here interprets the column (an OAuth
//! session without the device key can list encrypted rows but cannot use
//! them off-device; that read-only degradation is a client-side
//! consequence, not a server gate).
//!
//! PUT is a full replacement serving three uses with one shape: metadata
//! edit, key rotation, and a mode flip (the client decrypts the old form and
//! submits the new one). The API is explicit rather than patch-shaped: the
//! caller sends every field.
//!
//! Security model: every route rides the session router -- cookie auth is
//! the gate. Nothing here triggers outbound work or is reachable
//! unauthenticated, so there is no rate layer (same rationale as the emails
//! session surface in `routes.rs`).

use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::user_providers;
use crate::db::map_db_err;
use crate::state::SharedState;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use kallip_archeion_common::ids::UserId;
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, SqlErr,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Name of the `UNIQUE (user_id, name)` index. Matched against the Postgres
/// unique-violation message to map a duplicate-name insert/replace to a clean
/// 409 (mirrors `ADDRESS_UNIQUE_CONSTRAINT` in `emails`).
const NAME_UNIQUE_CONSTRAINT: &str = "idx_user_providers_user_name";

/// Per-account cap on saved entries. Bounds DB rows per account; the value
/// mirrors `MAX_EMAILS_PER_ACCOUNT` (a vault of provider keys is naturally
/// smaller than that -- 10 is generous).
const MAX_PROVIDERS_PER_ACCOUNT: usize = 10;

/// The cookie-authed management surface: list, create, replace, delete. Rides
/// the session router (no rate layer; a signed-in user curating their own
/// vault must not be throttled).
pub fn session_router() -> Router<SharedState> {
    Router::new()
        .route("/me/providers", get(list_providers).post(create_provider))
        .route(
            "/me/providers/{id}",
            axum::routing::put(replace_provider).delete(delete_provider),
        )
}

/// Create/replace body. `key_material` is the raw key (`mode = "plaintext"`)
/// or the client-encrypted blob (`mode = "encrypted"`); `base_url` overrides
/// provider's built-in default endpoint when present.
#[derive(Deserialize)]
struct ProviderRequest {
    name: String,
    provider: String,
    base_url: Option<String>,
    key_material: String,
    mode: String,
}

/// List/row response. `key_material` is returned as stored: a plaintext row
/// yields the raw key (any session of the account can read it), an encrypted
/// row yields the blob (useless without the device key).
#[derive(Serialize, Clone, Debug)]
struct ProviderSummary {
    id: Uuid,
    name: String,
    provider: String,
    base_url: Option<String>,
    key_material: String,
    mode: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

impl From<user_providers::Model> for ProviderSummary {
    fn from(m: user_providers::Model) -> Self {
        Self {
            id: m.id,
            name: m.name,
            provider: m.provider,
            base_url: m.base_url,
            key_material: m.key_material,
            mode: m.mode,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

/// `key_material` is only meaningful alongside its `mode`; accept exactly the
/// two storage forms and reject anything else up front (the column is TEXT,
/// not a DB enum -- no enum-column precedent in archeion).
fn validate_mode(mode: &str) -> Result<(), ApiError> {
    if matches!(mode, "plaintext" | "encrypted") {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "mode must be \"plaintext\" or \"encrypted\"",
        ))
    }
}

async fn list_providers(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<Vec<ProviderSummary>>, ApiError> {
    let user_id = require_user(&principal)?;
    let owned = load_owned(&state, user_id).await?;
    Ok(Json(owned.into_iter().map(ProviderSummary::from).collect()))
}

async fn create_provider(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(req): Json<ProviderRequest>,
) -> Result<Json<ProviderSummary>, ApiError> {
    let user_id = require_user(&principal)?;
    validate_mode(&req.mode)?;

    // Per-account cap: bounds DB rows (no outbound amplifier to bound here,
    // unlike the emails cap).
    let owned_count = user_providers::Entity::find()
        .filter(user_providers::Column::UserId.eq(user_id.to_string()))
        .count(&state.db)
        .await
        .map_err(map_db_err)?;
    if owned_count >= MAX_PROVIDERS_PER_ACCOUNT as u64 {
        return Err(ApiError::conflict("too many providers on this account"));
    }

    let now = OffsetDateTime::now_utc();
    let insert_result = user_providers::ActiveModel {
        id: Set(Uuid::new_v4()),
        user_id: Set(user_id.to_string()),
        name: Set(req.name),
        provider: Set(req.provider),
        base_url: Set(req.base_url),
        key_material: Set(req.key_material),
        mode: Set(req.mode),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(&state.db)
    .await;
    let model = match insert_result {
        Ok(m) => m,
        Err(e) => {
            // A duplicate (user, name) -> 409. The pair is account-scoped, so
            // the conflict is always this account's own naming; one generic
            // message suffices.
            if let Some(SqlErr::UniqueConstraintViolation(msg)) = e.sql_err()
                && msg.contains(NAME_UNIQUE_CONSTRAINT)
            {
                return Err(ApiError::conflict("provider name already used"));
            }
            return Err(map_db_err(e));
        }
    };
    Ok(Json(ProviderSummary::from(model)))
}

/// Full replacement: metadata edit, key rotation, or mode flip (the client
/// decrypts the old form and submits the new one) -- one explicit shape for
/// all three. Every field is sent; omitted-means-cleared is up to the client
/// UI, not this API.
async fn replace_provider(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<Uuid>,
    Json(req): Json<ProviderRequest>,
) -> Result<Json<ProviderSummary>, ApiError> {
    let user_id = require_user(&principal)?;
    validate_mode(&req.mode)?;

    // Load-then-update (not update-by-filter): the 404 ownership check must
    // fire before any write, and cross-account ids must not reveal whether
    // the row exists (404 either way, mirroring `load_owned` in `emails`).
    let owned = user_providers::Entity::find_by_id(id)
        .filter(user_providers::Column::UserId.eq(user_id.to_string()))
        .one(&state.db)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown provider"))?;

    let mut am: user_providers::ActiveModel = owned.into();
    am.name = Set(req.name);
    am.provider = Set(req.provider);
    am.base_url = Set(req.base_url);
    am.key_material = Set(req.key_material);
    am.mode = Set(req.mode);
    am.updated_at = Set(OffsetDateTime::now_utc());
    let model = match am.update(&state.db).await {
        Ok(m) => m,
        Err(e) => {
            // Renaming onto a sibling's name -> the same 409 as create.
            if let Some(SqlErr::UniqueConstraintViolation(msg)) = e.sql_err()
                && msg.contains(NAME_UNIQUE_CONSTRAINT)
            {
                return Err(ApiError::conflict("provider name already used"));
            }
            return Err(map_db_err(e));
        }
    };
    Ok(Json(ProviderSummary::from(model)))
}

async fn delete_provider(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ProviderSummary>>, ApiError> {
    let user_id = require_user(&principal)?;

    // Scoped delete: a foreign row is simply not matched (404, not 403).
    let result = user_providers::Entity::delete_many()
        .filter(user_providers::Column::Id.eq(id))
        .filter(user_providers::Column::UserId.eq(user_id.to_string()))
        .exec(&state.db)
        .await
        .map_err(map_db_err)?;
    if result.rows_affected == 0 {
        return Err(ApiError::not_found("unknown provider"));
    }
    let owned = load_owned(&state, user_id).await?;
    Ok(Json(owned.into_iter().map(ProviderSummary::from).collect()))
}

/// All entries owned by `user_id`, oldest first.
async fn load_owned(
    state: &SharedState,
    user_id: &UserId,
) -> Result<Vec<user_providers::Model>, ApiError> {
    user_providers::Entity::find()
        .filter(user_providers::Column::UserId.eq(user_id.to_string()))
        .order_by_asc(user_providers::Column::CreatedAt)
        .all(&state.db)
        .await
        .map_err(map_db_err)
}

#[cfg(test)]
mod tests;
