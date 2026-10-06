//! The admin provider face: the pool rows (family and endpoint),
//! their credentials, and the write gates that keep both
//! referable.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use crate::audit::management_event::{self, ManagementEventRow};
use crate::db;
use crate::management::AdminToken;
use crate::management::deserialize_explicit_option;
use crate::management::{Deleted, SecretPut, mask_key, validate_family, validate_profile_id};
use crate::registry::{self, profile, provider};
use crate::secret::provider_credential;
use crate::state::AppState;
use kallipai_common::protocol::ApiError;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};

/// The admin view of one pool provider: identity and shape, the key only
/// as its mask (`None` = no credential stored).
#[derive(Clone, Debug, Serialize)]
pub struct ProviderView {
    pub owner: String,
    pub provider_id: String,
    pub family: String,
    pub base_url: Option<String>,
    pub api_key_masked: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderViews {
    pub providers: Vec<ProviderView>,
}

/// POST /admin/providers body: the pool row's identity and shape, plus
/// the optional seed credential (stored, never echoed back).
#[derive(Deserialize)]
pub struct ProviderCreate {
    pub provider_id: String,
    pub family: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

/// PATCH /admin/providers/{provider_id} body. A missing field leaves the
/// field unchanged; `base_url: null` clears the URL (the explicit
/// option below), a present `api_key` writes (or rewrites) the
/// credential.
#[derive(Deserialize)]
pub struct ProviderUpdate {
    pub family: Option<String>,
    #[serde(default, deserialize_with = "deserialize_explicit_option")]
    pub base_url: Option<Option<String>>,
    pub api_key: Option<String>,
}

/// The credential view on the provider face: metadata and the mask. The
/// plaintext has no path out of this face.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderCredentialView {
    pub owner: String,
    pub provider_id: String,
    pub base_url: String,
    pub api_key_masked: String,
}

/// The joined admin view for one pool row (the credential mask rides in).
async fn provider_view<C: sea_orm::ConnectionTrait>(
    txn: &C,
    row: &provider::Model,
) -> Result<ProviderView, sea_orm::DbErr> {
    let api_key_masked =
        provider_credential::Entity::find_by_id((row.owner.clone(), row.provider_id.clone()))
            .one(txn)
            .await?
            .map(|c| mask_key(&c.api_key));
    Ok(ProviderView {
        owner: row.owner.clone(),
        provider_id: row.provider_id.clone(),
        family: row.family.clone(),
        base_url: row.base_url.clone(),
        api_key_masked,
    })
}

/// The masked audit JSON for a provider row change.
fn provider_audit_json(view: &ProviderView) -> serde_json::Value {
    serde_json::json!({
        "owner": view.owner,
        "provider_id": view.provider_id,
        "family": view.family,
        "base_url": view.base_url,
        "api_key_masked": view.api_key_masked,
    })
}

/// GET /admin/providers: the catalog space's pool rows, registration
/// order.
pub async fn list_providers(
    State(state): State<AppState>,
    _admin: AdminToken,
) -> Result<Json<ProviderViews>, ApiError> {
    let owner = registry::CATALOG_OWNER.to_owned();
    let rows = provider::Entity::find()
        .filter(provider::Column::Owner.eq(owner))
        .order_by_asc(provider::Column::CreatedAt)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut providers = Vec::new();
    for row in &rows {
        providers.push(
            provider_view(&state.db, row)
                .await
                .map_err(db::map_db_err)?,
        );
    }
    Ok(Json(ProviderViews { providers }))
}

/// POST /admin/providers: mint a pool row. The id is unique per space (the
/// composite primary key); racing an existing id is a 409, not a 500.
pub async fn create_provider(
    State(state): State<AppState>,
    admin: AdminToken,
    Json(body): Json<ProviderCreate>,
) -> Result<Json<ProviderView>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    validate_profile_id(&body.provider_id)?;
    validate_family(&body.family)?;
    if let Some(url) = &body.base_url
        && reqwest::Url::parse(url).is_err()
    {
        return Err(ApiError::bad_request("base_url must be an absolute URL"));
    }
    if body.api_key.as_deref() == Some("") {
        return Err(ApiError::bad_request("api_key must not be empty"));
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = provider::ActiveModel {
        owner: Set(owner.clone()),
        provider_id: Set(body.provider_id.clone()),
        family: Set(body.family.clone()),
        base_url: Set(body.base_url.clone()),
        ..Default::default()
    };
    row.insert(&txn).await.map_err(|e| {
        if db::is_unique_violation(&e) {
            ApiError::conflict(format!(
                "provider {:?} already exists in space {:?}",
                body.provider_id, owner
            ))
        } else {
            db::map_db_err(e)
        }
    })?;
    if let Some(key) = &body.api_key {
        provider_credential::ActiveModel {
            owner: Set(owner.clone()),
            provider_id: Set(body.provider_id.clone()),
            api_key: Set(key.clone()),
        }
        .insert(&txn)
        .await
        .map_err(db::map_db_err)?;
    }
    let view = ProviderView {
        api_key_masked: body.api_key.as_deref().map(mask_key),
        owner,
        provider_id: body.provider_id.clone(),
        family: body.family.clone(),
        base_url: body.base_url.clone(),
    };
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "provider",
            entity_id: body.provider_id.clone(),
            actor,
            before: None,
            after: Some(provider_audit_json(&view)),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(view))
}
/// PATCH /admin/providers/{provider_id}: update the row's shape and/or write
/// the credential. `None` fields stay; the space's profiles re-resolve on
/// the next request (the cache clears below).
pub async fn update_provider(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(provider_id): Path<String>,
    Json(body): Json<ProviderUpdate>,
) -> Result<Json<ProviderView>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    if let Some(family) = &body.family {
        validate_family(family)?;
    }
    if let Some(Some(url)) = &body.base_url
        && reqwest::Url::parse(url).is_err()
    {
        return Err(ApiError::bad_request("base_url must be an absolute URL"));
    }
    if body.api_key.as_deref() == Some("") {
        return Err(ApiError::bad_request("api_key must not be empty"));
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {provider_id}")))?;
    let before = provider_view(&txn, &row).await.map_err(db::map_db_err)?;
    let mut active = provider::ActiveModel::from(row);
    if let Some(family) = &body.family {
        active.family = Set(family.clone());
    }
    if let Some(url) = &body.base_url {
        active.base_url = Set(url.clone());
    }
    active.update(&txn).await.map_err(db::map_db_err)?;
    if let Some(key) = &body.api_key {
        let cred = provider_credential::ActiveModel {
            owner: Set(owner.clone()),
            provider_id: Set(provider_id.clone()),
            api_key: Set(key.clone()),
        };
        let existing =
            provider_credential::Entity::find_by_id((owner.clone(), provider_id.clone()))
                .one(&txn)
                .await
                .map_err(db::map_db_err)?;
        match existing {
            Some(_) => cred.update(&txn).await.map_err(db::map_db_err)?,
            None => cred.insert(&txn).await.map_err(db::map_db_err)?,
        };
    }
    let updated = registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::internal("provider row vanished mid-update"))?;
    let after = provider_view(&txn, &updated)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "provider",
            entity_id: provider_id.clone(),
            actor,
            before: Some(provider_audit_json(&before)),
            after: Some(provider_audit_json(&after)),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(after))
}

/// DELETE /admin/providers/{provider_id}: remove the row (its credential
/// goes with it, the FK cascade). Refused while any profile in the space
/// still points at the row -- a provider is deletable only when nothing
/// depends on it.
pub async fn delete_provider(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(provider_id): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {provider_id}")))?;
    let sharers = profile::Entity::find()
        .filter(profile::Column::Owner.eq(owner.clone()))
        .filter(profile::Column::ProviderId.eq(provider_id.clone()))
        .count(&txn)
        .await
        .map_err(db::map_db_err)?;
    if sharers > 0 {
        return Err(ApiError::conflict(format!(
            "provider still serves {sharers} profiles; re-point them before deleting it"
        )));
    }
    let before = provider_view(&txn, &row).await.map_err(db::map_db_err)?;
    provider::Entity::delete_by_id((owner.clone(), provider_id.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "provider",
            entity_id: provider_id.clone(),
            actor,
            before: Some(provider_audit_json(&before)),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(Deleted {
        deleted: true,
        warning: None,
    }))
}
/// PUT /admin/providers/{provider_id}/credential: write the credential
/// half (the base URL rides the same provider row). The plaintext is
/// accepted here and never comes back: the response and the audit row
/// carry the mask only.
pub async fn put_provider_credential(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(provider_id): Path<String>,
    Json(body): Json<SecretPut>,
) -> Result<Json<ProviderCredentialView>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    if body.upstream_api_key.is_empty() {
        return Err(ApiError::bad_request("upstream_api_key must not be empty"));
    }
    if reqwest::Url::parse(&body.upstream_base_url).is_err() {
        return Err(ApiError::bad_request(
            "upstream_base_url must be an absolute URL",
        ));
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {provider_id}")))?;
    let existing = provider_credential::Entity::find_by_id((owner.clone(), provider_id.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?;
    let before = existing
        .as_ref()
        .map(|c| {
            provider_credential_view(
                &owner,
                &provider_id,
                &row.base_url.clone().unwrap_or_default(),
                &c.api_key,
            )
        })
        .map(|v| provider_credential_audit_json(&v));
    // The base URL rides the provider row; the key is the credential
    // itself. Upsert both halves.
    let mut active = provider::ActiveModel::from(row);
    active.base_url = Set(Some(body.upstream_base_url.clone()));
    active.update(&txn).await.map_err(db::map_db_err)?;
    let cred = provider_credential::ActiveModel {
        owner: Set(owner.clone()),
        provider_id: Set(provider_id.clone()),
        api_key: Set(body.upstream_api_key.clone()),
    };
    match existing {
        Some(_) => cred.update(&txn).await.map_err(db::map_db_err)?,
        None => cred.insert(&txn).await.map_err(db::map_db_err)?,
    };
    let view = provider_credential_view(
        &owner,
        &provider_id,
        &body.upstream_base_url,
        &body.upstream_api_key,
    );
    management_event::record(
        &txn,
        ManagementEventRow {
            action: if before.is_some() { "update" } else { "create" },
            entity: "provider_credential",
            entity_id: provider_id.clone(),
            actor,
            before,
            after: Some(provider_credential_audit_json(&view)),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(view))
}

/// DELETE /admin/providers/{provider_id}/credential: clear the key without
/// touching the row. 404 when no credential is stored.
pub async fn delete_provider_credential(
    State(state): State<AppState>,
    admin: AdminToken,
    Path(provider_id): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let actor = admin.operator.as_str();
    let owner = registry::CATALOG_OWNER.to_owned();
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let row = registry::provider_by_id(&txn, &owner, &provider_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found(format!("no such provider: {provider_id}")))?;
    let existing = provider_credential::Entity::find_by_id((owner.clone(), provider_id.clone()))
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no credential for this provider"))?;
    let view = provider_credential_view(
        &owner,
        &provider_id,
        &row.base_url.clone().unwrap_or_default(),
        &existing.api_key,
    );
    provider_credential::Entity::delete_by_id((owner, provider_id.clone()))
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "provider_credential",
            entity_id: provider_id,
            actor,
            before: Some(provider_credential_audit_json(&view)),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    Ok(Json(Deleted {
        deleted: true,
        warning: None,
    }))
}

/// The credential view on the provider face (identity plus the mask).
pub(crate) fn provider_credential_view(
    owner: &str,
    provider_id: &str,
    base_url: &str,
    key: &str,
) -> ProviderCredentialView {
    ProviderCredentialView {
        owner: owner.to_owned(),
        provider_id: provider_id.to_owned(),
        base_url: base_url.to_owned(),
        api_key_masked: mask_key(key),
    }
}

/// The masked audit JSON for a credential change.
pub(crate) fn provider_credential_audit_json(view: &ProviderCredentialView) -> serde_json::Value {
    serde_json::json!({
        "owner": view.owner,
        "provider_id": view.provider_id,
        "base_url": view.base_url,
        "api_key_masked": view.api_key_masked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::testkit::*;
    use axum::http::StatusCode;
    use sea_orm::{ConnectionTrait, EntityTrait};

    #[tokio::test]
    async fn provider_credential_crud_never_returns_the_plaintext() {
        let state = seeded_state().await;
        catalog_provider(&state, "a4", "deepseek").await;
        let secret_body = serde_json::json!({
            "upstream_base_url": "https://api.upstream.test",
            "upstream_api_key": "sk-plaintext-abcdef123456"
        });
        // Write (create path).
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/providers/a4/credential",
                Some(TEST_ADMIN_BEARER),
                Some(secret_body.clone()),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["provider_id"], "a4");
        assert!(
            body["api_key_masked"]
                .as_str()
                .expect("mask")
                .contains("***")
        );
        // Rewrite (update path).
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/providers/a4/credential",
                Some(TEST_ADMIN_BEARER),
                Some(secret_body),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The list is masked: the plaintext appears nowhere in the body.
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/admin/providers", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = body.to_string();
        assert!(!text.contains("sk-plaintext-abcdef123456"));
        assert!(text.contains("a4"));
        // The store itself holds the plaintext (injection depends on it).
        let stored = provider_credential::Entity::find_by_id((
            registry::CATALOG_OWNER.to_owned(),
            "a4".to_owned(),
        ))
        .one(&state.db)
        .await
        .expect("credential read")
        .expect("credential stored");
        assert_eq!(stored.api_key, "sk-plaintext-abcdef123456");
        // The audit rows carry the mask, never the plaintext.
        let rows = audit_rows(&state.db).await;
        let secret_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "provider_credential" && r.entity_id == "a4")
            .collect();
        assert_eq!(secret_rows.len(), 2);
        assert_eq!(secret_rows[0].action, "create");
        assert_eq!(secret_rows[1].action, "update");
        for r in &secret_rows {
            let raw = format!("{:?}", r.before);
            assert!(!raw.contains("sk-plaintext-abcdef123456"));
        }
        // Delete: once ok, twice 404.
        let (status, _) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/providers/a4/credential",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            app(state),
            req(
                "DELETE",
                "/admin/providers/a4/credential",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn provider_credential_write_requires_an_existing_provider() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state),
            req(
                "PUT",
                "/admin/providers/ghost/credential",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({
                    "upstream_base_url": "https://api.upstream.test",
                    "upstream_api_key": "sk-orphan"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The delete guard: a provider still referenced by a profile in
    /// the admin space refuses with 409 (the planted a4 carries one).
    #[tokio::test]
    async fn deleting_a_referenced_provider_conflicts() {
        let state = seeded_state().await;
        catalog_profile(&state, "a4", "draft-x", true).await;
        let (status, body) = send(
            app(state),
            req(
                "DELETE",
                "/admin/providers/a4",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    /// The explicit-option converter: absent leaves the field (None),
    /// null clears (Some(None)), a value writes (Some(Some)).
    #[test]
    fn explicit_option_deserializes_to_three_states() {
        let absent: ProviderUpdate = serde_json::from_str(r#"{"family": "x"}"#).expect("absent");
        assert_eq!(absent.base_url, None);
        let cleared: ProviderUpdate = serde_json::from_str(r#"{"base_url": null}"#).expect("null");
        assert_eq!(cleared.base_url, Some(None));
        let written: ProviderUpdate =
            serde_json::from_str(r#"{"base_url": "https://api.test"}"#).expect("value");
        assert_eq!(written.base_url, Some(Some("https://api.test".to_owned())));
    }

    /// The three-state base_url end to end: a value writes, an
    /// explicit null clears, an absent field leaves the row alone.
    #[tokio::test]
    async fn provider_base_url_three_state_round_trip() {
        let state = seeded_state().await;
        catalog_provider(&state, "a4", "openai-compatible").await;
        // A value writes the URL.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/providers/a4",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"base_url": "https://api.new.test"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["base_url"], "https://api.new.test");
        // An explicit null clears it.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PATCH",
                "/admin/providers/a4",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"base_url": null})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["base_url"].is_null(), "{body}");
        // An absent field leaves the cleared value alone.
        let (status, body) = send(
            app(state),
            req(
                "PATCH",
                "/admin/providers/a4",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"family": "openai-compatible"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["base_url"].is_null(), "{body}");
    }

    /// A multi-byte upstream key rides every masked exit without panicking:
    /// store, list, overwrite, plain delete, and the delete-parked cascade.
    /// Byte 3 and byte len-3 of the key both fall inside a character --
    /// masking must stay character-safe at every exit.
    #[tokio::test]
    async fn multibyte_key_masks_across_every_exit() {
        let state = seeded_state().await;
        catalog_profile(&state, "a4", "draft-x", true).await;
        catalog_provider(&state, "a2", "deepseek").await;
        catalog_set(&state, "beta2").await;
        catalog_member(&state, "beta2", "a4", 0).await;
        let key = "éééé"; // 4 chars, 8 bytes: both slice points split a char
        let secret_body = serde_json::json!({
            "upstream_base_url": "https://api.upstream.test",
            "upstream_api_key": key,
        });
        // Store (create path): the response carries the mask only.
        let (status, stored) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/providers/a4/credential",
                Some(TEST_ADMIN_BEARER),
                Some(secret_body.clone()),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{stored}");
        assert!(
            stored["api_key_masked"]
                .as_str()
                .expect("mask")
                .contains("***")
        );
        assert!(!stored.to_string().contains(key));
        // List: a poisoned row is masked and served, not fatal.
        let (status, listed) = send(
            app(state.clone()),
            req("GET", "/admin/providers", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!listed.to_string().contains(key));
        // Overwrite: the update path masks the old value for its before row.
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/providers/a4/credential",
                Some(TEST_ADMIN_BEARER),
                Some(secret_body),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Plain delete: rewrite a seeded credential with the key first, so
        // the delete path masks a multi-byte stored value on its way out.
        let p2_body = serde_json::json!({
            "upstream_base_url": "https://api.upstream.test",
            "upstream_api_key": key,
        });
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/providers/a2/credential",
                Some(TEST_ADMIN_BEARER),
                Some(p2_body),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/providers/a2/credential",
                Some(TEST_ADMIN_BEARER),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The cascade exit: the seeded beta2 membership rides the parked
        // delete, which masks the credential snapshot inside the cascade;
        // the delete succeeds with the stored value in place.
        let (status, deleted) = send(
            app(state.clone()),
            req("DELETE", "/admin/parking/a4", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{deleted}");
        // No audit row anywhere carries the plaintext.
        let rows = audit_rows(&state.db).await;
        for r in &rows {
            let raw = format!("{:?}", (&r.before, &r.after));
            assert!(!raw.contains(key), "plaintext leaked: {r:?}");
        }
        // The cascade's membership row: beta's order before and after.
        let member_rows: Vec<_> = rows.iter().filter(|r| r.entity == "set_member").collect();
        assert_eq!(member_rows.len(), 1);
        assert_eq!(member_rows[0].entity_id, "beta2");
        assert_eq!(
            member_rows[0].before.as_ref().expect("before")["members"],
            serde_json::json!(["a4"])
        );
        assert_eq!(
            member_rows[0].after.as_ref().expect("after")["members"],
            serde_json::json!([])
        );
    }

    /// The fail-closed pin: when the audit row cannot land, the whole
    /// change rolls back. The forwarding path's best-effort posture is the
    /// deliberate opposite; this test keeps the two from drifting.
    #[tokio::test]
    async fn audit_failure_rolls_back_the_change() {
        use sea_orm::Statement;
        let state = seeded_state().await;
        state
            .db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "DROP TABLE management_events".to_owned(),
            ))
            .await
            .expect("audit table dropped");
        let (status, _) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/collections/baseline/sets",
                Some(TEST_ADMIN_BEARER),
                Some(serde_json::json!({"name": "gamma", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // No set row survived the failed audit.
        let (status, _) = send(
            app(state),
            req("GET", "/admin/sets/gamma", Some(TEST_ADMIN_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The domain is a wall, not a filter: the parking and provider
    /// routes are pinned to the catalog space, so rows planted one
    /// space over answer 404 on every route.
    #[tokio::test]
    async fn parking_and_provider_routes_refuse_cross_space_rows() {
        let state = seeded_state().await;
        foreign_parked(&state, "fx").await;
        for (method, path, body) in [
            ("GET", "/admin/parking/fx", None),
            (
                "PUT",
                "/admin/parking/fx",
                Some(serde_json::json!({
                    "model": "draft-x",
                    "max_context_window": 8192,
                    "effort": null,
                    "modalities": ["text"],
                    "parked": false
                })),
            ),
            ("DELETE", "/admin/parking/fx", None),
            ("DELETE", "/admin/providers/fx", None),
            (
                "PUT",
                "/admin/providers/fx/credential",
                Some(serde_json::json!({
                    "upstream_base_url": "https://api.upstream.test",
                    "upstream_api_key": "sk-cross-space"
                })),
            ),
        ] {
            let (status, body_out) = send(
                app(state.clone()),
                req(method, path, Some(TEST_ADMIN_BEARER), body),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {body_out}");
        }
    }

    /// A provider still serving profiles refuses deletion with a 409
    /// and keeps its row.
    #[tokio::test]
    async fn user_delete_provider_in_use_conflicts() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/providers",
                user,
                true,
                Some(serde_json::json!({
                    "provider_id": "up1",
                    "family": "deepseek",
                    "base_url": "https://api.user.test",
                    "api_key": "sk-user-secret"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/profiles",
                user,
                true,
                Some(serde_json::json!({
                    "profile_id": "p-user",
                    "provider_id": "up1",
                    "model": "deepseek-chat"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The guard holds while the profile points here.
        let (status, body) = send(
            app(state.clone()),
            cookie_req("DELETE", "/user/providers/up1", user, true, None),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        // The refused delete leaves the row in place.
        let (status, body) = send(
            app(state.clone()),
            cookie_req("GET", "/user/providers", user, false, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["providers"].as_array().expect("list").len(), 1);
    }

    /// The update contract on the user face mirrors the admin face:
    /// an absent `base_url` leaves the field alone, an explicit null
    /// clears it.
    #[tokio::test]
    async fn user_update_provider_base_url_tristate() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/providers",
                user,
                true,
                Some(serde_json::json!({
                    "provider_id": "up1",
                    "family": "deepseek",
                    "base_url": "https://api.user.test"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Absent: the stored URL survives the update.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                "/user/providers/up1",
                user,
                true,
                Some(serde_json::json!({"family": "deepseek"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["base_url"], "https://api.user.test");
        // Explicit null: the URL clears.
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PATCH",
                "/user/providers/up1",
                user,
                true,
                Some(serde_json::json!({"base_url": null})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["base_url"], serde_json::Value::Null);
    }

    /// The credential fill refuses a base_url that is not an absolute
    /// URL (the same gate the admin face applies).
    #[tokio::test]
    async fn user_credential_rejects_a_relative_base_url() {
        let state = seeded_state().await;
        let user = Some(TEST_USER_COOKIE);
        let (status, _) = send(
            app(state.clone()),
            cookie_req(
                "POST",
                "/user/providers",
                user,
                true,
                Some(serde_json::json!({"provider_id": "up1", "family": "deepseek"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(
            app(state.clone()),
            cookie_req(
                "PUT",
                "/user/providers/up1/credential",
                user,
                true,
                Some(serde_json::json!({
                    "upstream_base_url": "not-a-url",
                    "upstream_api_key": "sk-user-secret"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
}
