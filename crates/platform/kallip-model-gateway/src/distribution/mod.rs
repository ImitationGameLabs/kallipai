//! The distribution face: the proxy-key-authenticated, secret-free reads.
//!
//! Everything served from here is registry data only -- upstream endpoints
//! and provider API keys are structurally unreachable from this module
//! (they live in the secret domain and its types carry no getters). The
//! `api_key` field of a distributed profile is the presenting proxy key
//! echoed back (the design doc's "proxy-determined shape": the client
//! copies it verbatim into its provider config).
//!
//! Failure vocabulary: a missing resource is 404; a resource that exists
//! but is outside the presenting key's allowed sets is 403 (default-deny);
//! a missing/unknown bearer is 401 from the [`ProxyKey`] extractor.

use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use serde::Serialize;

use crate::registry;
use crate::secret::{self, ResolvedKey};
use crate::state::AppState;
use crate::{db, routes::ApiError};

/// Extractor: resolve the presenting `Authorization: Bearer` to a stored
/// proxy key or fail closed with 401. Carries the presented token (the
/// distribution face echoes it as the profile's `api_key`) and the
/// resolution (tagma identity + allowed sets).
#[derive(Clone, Debug)]
pub struct ProxyKey {
    pub bearer: String,
    pub resolved: ResolvedKey,
}

impl FromRequestParts<AppState> for ProxyKey {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = kallip_common::auth_header::extract_bearer_token(&parts.headers)?;
        let resolved = match secret::resolve(&state.db, &state.key_cache, bearer)
            .await
            .map_err(db::map_db_err)?
        {
            secret::Resolution::Resolved(resolved) => resolved,
            // The dedicated codes: a revoked or lapsed key tells the
            // client something actionable -- refetch the configuration
            // (the key is gone from it) or mint a fresh one. An unknown
            // bearer stays the bare 401: no existence oracle for
            // guessed keys.
            secret::Resolution::Revoked => {
                return Err(ApiError::unauthorized_with_code(
                    "this proxy key has been revoked; fetch updated configuration or mint a new key",
                    "key_revoked",
                ));
            }
            secret::Resolution::Expired => {
                return Err(ApiError::unauthorized_with_code(
                    "this proxy key has expired; mint a new key",
                    "key_expired",
                ));
            }
            secret::Resolution::Unknown => {
                return Err(ApiError::unauthorized("unknown proxy key"));
            }
        };
        Ok(Self {
            bearer: bearer.to_owned(),
            resolved,
        })
    }
}

/// A sanitized profile: the distribution payload. `base_url` is the proxy
/// itself (the `/v1`-prefixed base; clients append `chat/completions`);
/// `api_key` is the presenting key echoed. No upstream field can appear
/// here -- the type has nowhere to carry one.
#[derive(Clone, Debug, Serialize)]
pub struct SanitizedProfile {
    pub family: String,
    pub base_url: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Vec<String>,
    pub api_key: String,
}

fn sanitize(state: &AppState, p: registry::profile::Model, api_key: String) -> SanitizedProfile {
    let modalities = p
        .modalities
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default();
    SanitizedProfile {
        family: p.family,
        base_url: state.public_base_url.clone(),
        model: p.model,
        max_context_window: p.max_context_window,
        effort: p.effort,
        modalities,
        api_key,
    }
}

/// GET /profiles/{profile_id}.
pub async fn get_profile(
    State(state): State<AppState>,
    key: ProxyKey,
    Path(profile_id): Path<String>,
) -> Result<Json<SanitizedProfile>, ApiError> {
    let profile = registry::profile_by_id(&state.db, &profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such profile"))?;
    let authorized =
        registry::profile_in_authorized_sets(&state.db, &profile_id, &key.resolved.allowed_sets)
            .await
            .map_err(db::map_db_err)?;
    if !authorized {
        return Err(ApiError::forbidden(
            "profile is outside the key's allowed sets",
        ));
    }
    Ok(Json(sanitize(&state, profile, key.bearer)))
}

/// One set entry with its ordered members (the failover order, verbatim).
#[derive(Clone, Debug, Serialize)]
pub struct SanitizedSet {
    pub name: String,
    pub description: String,
    pub profiles: Vec<SanitizedProfile>,
}

/// GET /sets/{name}.
pub async fn get_set(
    State(state): State<AppState>,
    key: ProxyKey,
    Path(name): Path<String>,
) -> Result<Json<SanitizedSet>, ApiError> {
    let set = registry::set_by_name(&state.db, &name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such set"))?;
    if !key.resolved.allowed_sets.contains(&name) {
        return Err(ApiError::forbidden("set is outside the key's allowed sets"));
    }
    let profiles = registry::set_members_ordered(&state.db, &name)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(SanitizedSet {
        name: set.name,
        description: set.description,
        profiles: profiles
            .into_iter()
            .map(|p| sanitize(&state, p, key.bearer.clone()))
            .collect(),
    }))
}

/// GET /sets: the presenting key's allowed set names that exist.
#[derive(Clone, Debug, Serialize)]
pub struct SetNames {
    pub sets: Vec<String>,
}

pub async fn get_sets(
    State(state): State<AppState>,
    key: ProxyKey,
) -> Result<Json<SetNames>, ApiError> {
    let existing = registry::sets_existing_among(&state.db, &key.resolved.allowed_sets)
        .await
        .map_err(db::map_db_err)?;
    let mut names: Vec<String> = existing.into_iter().map(|s| s.name).collect();
    names.sort();
    Ok(Json(SetNames { sets: names }))
}

/// GET /parking: parked draft profiles, any valid key.
#[derive(Clone, Debug, Serialize)]
pub struct Parking {
    pub profiles: Vec<SanitizedProfile>,
}

pub async fn get_parking(
    State(state): State<AppState>,
    key: ProxyKey,
) -> Result<Json<Parking>, ApiError> {
    let parked = registry::parked_profiles(&state.db)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(Parking {
        profiles: parked
            .into_iter()
            .map(|p| sanitize(&state, p, key.bearer.clone()))
            .collect(),
    }))
}

/// GET /default: the registry-level default set marker.
#[derive(Clone, Debug, Serialize)]
pub struct DefaultSet {
    pub default_set: String,
}

pub async fn get_default(
    State(state): State<AppState>,
    _key: ProxyKey,
) -> Result<Json<DefaultSet>, ApiError> {
    let name = registry::default_set_name(&state.db)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no default set configured"))?;
    Ok(Json(DefaultSet { default_set: name }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TEST_BEARER, migrated_test_db};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn get(path: &str, bearer: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri(path);
        if let Some(bearer) = bearer {
            builder = builder.header("authorization", format!("Bearer {bearer}"));
        }
        builder.body(Body::empty()).expect("request")
    }

    /// The whole distribution surface against the seeded fixture registry:
    /// sanitization shape, allowed-set scoping, and the failure vocabulary.
    #[tokio::test]
    async fn distribution_round_trip_scopes_and_sanitizes() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let app = router_for_test(AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            quota: std::sync::Arc::new(crate::quota::QuotaLedger::new()),
            management: crate::test_support::test_management(),
            key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
        });

        // /sets: the key's allowed sets that exist.
        let response = app
            .clone()
            .oneshot(get("/sets", Some(TEST_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["sets"], serde_json::json!(["alpha"]));

        // /sets/alpha: ordered members (failover order verbatim), sanitized.
        let response = app
            .clone()
            .oneshot(get("/sets/alpha", Some(TEST_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["name"], "alpha");
        let profiles = body["profiles"].as_array().expect("profiles array");
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0]["model"], "deepseek-chat");
        assert_eq!(profiles[1]["model"], "deepseek-researcher");
        assert_eq!(profiles[0]["base_url"], "http://gw.test:7501");
        assert_eq!(profiles[0]["api_key"], TEST_BEARER);
        assert_eq!(profiles[0]["modalities"], serde_json::json!(["text"]));
        let text = body.to_string();
        assert!(
            !text.contains("upstream"),
            "no upstream field may leak: {text}"
        );

        // /profiles/<id>: allowed, unauthorized, unknown.
        let response = app
            .clone()
            .oneshot(get("/profiles/p1", Some(TEST_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(get("/profiles/p3", Some(TEST_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(get("/profiles/ghost", Some(TEST_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // /sets/beta: exists but outside the key's allowed sets.
        let response = app
            .clone()
            .oneshot(get("/sets/beta", Some(TEST_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // /parking: parked drafts.
        let response = app
            .clone()
            .oneshot(get("/parking", Some(TEST_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["profiles"][0]["model"], "draft-x");

        // /default: the registry marker.
        let response = app
            .clone()
            .oneshot(get("/default", Some(TEST_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["default_set"], "alpha");

        // Failure vocabulary on the bearer: missing and unknown -> 401.
        let response = app.clone().oneshot(get("/sets", None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app.oneshot(get("/sets", Some("not-a-key"))).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    use sea_orm::{ActiveModelTrait, Set};

    fn req(
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(t) = bearer {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        match body {
            Some(v) => builder
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&v).expect("test body serializes"),
                ))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        }
    }

    async fn json_of(response: axum::response::Response) -> serde_json::Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap_or(serde_json::Value::Null)
    }

    /// Mint through the admin face; returns (token, key_id).
    async fn mint_key(app: &axum::Router, expires_at: Option<String>) -> (String, String) {
        let mut payload = serde_json::json!({
            "tagma_id": "tagma-under-test",
            "allowed_sets": ["alpha"],
        });
        if let Some(e) = expires_at {
            payload["expires_at"] = serde_json::json!(e);
        }
        let response = app
            .clone()
            .oneshot(req(
                "POST",
                "/admin/keys",
                Some(crate::test_support::TEST_MANAGEMENT_TOKEN),
                Some(payload),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        (
            body["token"].as_str().expect("token").to_owned(),
            body["key_id"].as_str().expect("key_id").to_owned(),
        )
    }

    /// The distribution face's rejection vocabulary: a revoked key answers
    /// with the dedicated code even when the resolution cache was warm --
    /// the management write cleared it, the store carries only the revoke
    /// event, and the client can tell "fetch fresh config" apart from
    /// "never heard of you".
    #[tokio::test]
    async fn revoked_key_answers_with_the_dedicated_code_through_a_warm_cache() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let state = AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            quota: std::sync::Arc::new(crate::quota::QuotaLedger::new()),
            management: crate::test_support::test_management(),
            key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
        };
        let (app, mgmt) = routers_for_test(state);
        let (token, key_id) = mint_key(&mgmt, None).await;

        // Warm the cache: the minted key distributes fine.
        let response = app
            .clone()
            .oneshot(get("/sets", Some(&token)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Revoke through the admin face.
        let response = mgmt
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/admin/keys/{key_id}"),
                Some(crate::test_support::TEST_MANAGEMENT_TOKEN),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // The stale cache cannot resurrect the key.
        let response = app
            .clone()
            .oneshot(get("/sets", Some(&token)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_of(response).await;
        assert_eq!(body["error"]["code"], "key_revoked", "{body}");
    }

    /// A lapsed key: the dedicated code on the store path too.
    #[tokio::test]
    async fn expired_key_answers_with_the_dedicated_code() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let state = AppState {
            db: db.clone(),
            public_base_url: "http://gw.test:7501".to_string(),
            quota: std::sync::Arc::new(crate::quota::QuotaLedger::new()),
            management: crate::test_support::test_management(),
            key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
        };
        let (app, mgmt) = routers_for_test(state);
        let (token, _) = mint_key(&mgmt, Some("2099-01-01T00:00:00Z".to_owned())).await;

        // Lapse the key behind the admin API's future-only validation.
        let hash = kallip_common::authtoken::TokenHash::of(&token)
            .as_bytes()
            .to_vec();
        let patch = crate::secret::proxy_key::ActiveModel {
            key_hash: Set(hash),
            expires_at: Set(Some(
                time::OffsetDateTime::now_utc() - time::Duration::seconds(60),
            )),
            ..Default::default()
        };
        patch.update(&db).await.expect("lapse");

        let response = app
            .clone()
            .oneshot(get("/sets", Some(&token)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_of(response).await;
        assert_eq!(body["error"]["code"], "key_expired", "{body}");
    }

    /// The unknown bearer keeps the bare 401: no code, no existence oracle.
    #[tokio::test]
    async fn unknown_bearer_stays_the_bare_unauthorized() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let app = router_for_test(AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            quota: std::sync::Arc::new(crate::quota::QuotaLedger::new()),
            management: crate::test_support::test_management(),
            key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
        });
        let response = app
            .clone()
            .oneshot(get("/sets", Some("never-minted-bearer")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_of(response).await;
        assert!(body["error"]["code"].is_null(), "{body}");
    }

    /// A cached key dies on the wall clock: the hit re-checks `expires_at`
    /// and evicts itself, no management write required.
    #[tokio::test]
    async fn cached_key_lapses_on_the_wall_clock() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let state = AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            quota: std::sync::Arc::new(crate::quota::QuotaLedger::new()),
            management: crate::test_support::test_management(),
            key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
        };
        let (app, mgmt) = routers_for_test(state);
        let deadline = (time::OffsetDateTime::now_utc() + time::Duration::seconds(3))
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339 formats");
        let (token, _) = mint_key(&mgmt, Some(deadline)).await;

        let response = app
            .clone()
            .oneshot(get("/sets", Some(&token)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "warms the cache");

        tokio::time::sleep(std::time::Duration::from_millis(6000)).await;

        let response = app
            .clone()
            .oneshot(get("/sets", Some(&token)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_of(response).await;
        assert_eq!(body["error"]["code"], "key_expired", "{body}");
    }

    /// Every management write clears the cache after its commit; the store
    /// stays the source of truth and the next request warms it again.
    #[tokio::test]
    async fn management_registry_write_clears_the_key_cache() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let state = AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            quota: std::sync::Arc::new(crate::quota::QuotaLedger::new()),
            management: crate::test_support::test_management(),
            key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
        };
        let cache = state.key_cache.clone();
        let app = router_for_test(state.clone());
        let mgmt = management_router_for_test(state);

        let response = app
            .clone()
            .oneshot(get("/sets", Some(TEST_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(cache.len(), 1, "the distribution hit warmed the cache");

        let response = mgmt
            .clone()
            .oneshot(req(
                "PUT",
                "/admin/sets/alpha",
                Some(crate::test_support::TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"description": "updated"})),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(cache.len(), 0, "the write cleared the cache");

        let response = app.oneshot(get("/sets", Some(TEST_BEARER))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "resolves again");
        assert_eq!(cache.len(), 1, "the next hit warms it back");
    }

    fn router_for_test(state: AppState) -> axum::Router {
        crate::routes::data_plane_router(state, "")
    }

    fn management_router_for_test(state: AppState) -> axum::Router {
        crate::routes::management_plane_router(state, "")
    }

    /// Both planes over one state: mints and revokes go through the
    /// management router, the asserted distribution requests through the
    /// data router -- the physical separation the batch installs.
    fn routers_for_test(state: AppState) -> (axum::Router, axum::Router) {
        (
            router_for_test(state.clone()),
            management_router_for_test(state),
        )
    }
}
