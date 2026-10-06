//! The distribution face: the tagma-identity-authenticated, secret-free
//! reads.
//!
//! Everything served from here is registry data only -- provider
//! credentials (endpoint and key) are structurally unreachable from
//! this module: they live in the secret domain and its types carry no
//! getters. The
//! `api_key` field of a distributed profile is the presenting tagma
//! bearer echoed back (the proxy-determined shape: the client copies it
//! verbatim into its provider config).
//!
//! Failure vocabulary: a missing resource is 404; a resource that exists
//! but is outside the identity's visibility domain is 403 (default-deny);
//! a missing/unknown bearer is 401 from the [`TagmaIdentity`] extractor.

pub mod visibility;
use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use serde::Deserialize;
use serde::Serialize;

use crate::registry;
use crate::secret;
use crate::state::AppState;
use crate::{db, routes::ApiError};

/// Extractor: resolve the presenting `Authorization: Bearer` to the
/// tagma's platform identity (the archeion verifies the token and the
/// enrollment read binds the account) or fail closed with 401. Carries
/// the presented token (the distribution face echoes it as the
/// profile's `api_key`) and the visibility viewer the identity's
/// account resolves to. The bearer is platform credential material,
/// so `Debug` is a manual impl printing it as `[REDACTED]`.
#[derive(Clone)]
pub struct TagmaIdentity {
    pub bearer: String,
    /// The tagma behind the bearer (the selection store's key; not
    /// credential material, so `Debug` prints it plainly).
    pub tagma_id: String,
    pub viewer: visibility::Viewer,
}

impl std::fmt::Debug for TagmaIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TagmaIdentity")
            .field("bearer", &"[REDACTED]")
            .field("tagma_id", &self.tagma_id)
            .field("viewer", &self.viewer)
            .finish()
    }
}

impl TagmaIdentity {
    /// The shared resolution: the presenting bearer to the platform
    /// identity. The extractor below wraps this for the axum handlers;
    /// the pingora data plane calls it directly with the session's
    /// headers -- one authentication chain, one implementation, the
    /// same failure vocabulary on both planes.
    pub async fn from_headers(
        state: &AppState,
        headers: &axum::http::HeaderMap,
    ) -> Result<Self, ApiError> {
        let bearer = kallipai_common::auth_header::extract_bearer_token(headers)?;
        let identity = match secret::resolve(state, bearer).await? {
            secret::Resolution::Resolved(identity) => identity,
            // The bare 401: no existence oracle for guessed tagma
            // tokens, revoked credentials, or tagmas the archeion
            // cannot address -- the enrollment state stays hidden.
            secret::Resolution::Unknown => {
                return Err(ApiError::unauthorized("unknown tagma token"));
            }
        };
        let viewer = visibility::Viewer::resolve(&state.db, identity.account_id.clone())
            .await
            .map_err(db::map_db_err)?;
        Ok(Self {
            bearer: bearer.to_owned(),
            viewer,
            tagma_id: identity.tagma_id,
        })
    }
}

impl FromRequestParts<AppState> for TagmaIdentity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Self::from_headers(state, &parts.headers).await
    }
}

/// A sanitized profile: the distribution payload. `base_url` is the proxy
/// itself (the `/v1`-prefixed base; clients append `chat/completions`);
/// `api_key` is the presenting bearer echoed. No upstream field can
/// appear here -- the type has nowhere to carry one.
#[derive(Clone, Debug, Serialize)]
pub struct SanitizedProfile {
    pub family: String,
    pub base_url: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    /// The responses wire's transcript tri-state, verbatim from the
    /// profile (`None` = the client's request body decides).
    pub store: Option<bool>,
    pub modalities: Vec<String>,
    pub api_key: String,
}

fn sanitize(
    state: &AppState,
    served: registry::ServedProfile,
    api_key: String,
) -> SanitizedProfile {
    let modalities = served
        .modalities
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default();
    SanitizedProfile {
        family: served.family.clone(),
        base_url: state.public_base_url.clone(),
        model: served.model.clone(),
        max_context_window: served.max_context_window,
        effort: served.effort.clone(),
        store: served.store,
        modalities,
        api_key,
    }
}

/// GET /profiles/{profile_id}.
pub async fn get_profile(
    State(state): State<AppState>,
    identity: TagmaIdentity,
    Path(profile_id): Path<String>,
) -> Result<Json<SanitizedProfile>, ApiError> {
    // The row: the id is globally unique, so this is the one row, in
    // whatever space owns it. A user-space row can serve here now;
    // the grant check below decides what this identity may see of it.
    let mut rows = registry::profiles_by_id(&state.db, &profile_id)
        .await
        .map_err(db::map_db_err)?;
    let Some(profile) = rows.pop() else {
        return Err(ApiError::not_found("no such profile"));
    };
    let served = registry::served_profile(&state.db, profile)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such profile"))?;
    // The grant: invisible answers 404 for a user-space row (no
    // existence oracle across accounts) and 403 for a catalog row
    // (the row is catalog knowledge; the identity's domain just does
    // not reach it).
    let grant = visibility::profile_grant(
        &state.db,
        &identity.viewer,
        &served.provider_owner,
        &profile_id,
    )
    .await
    .map_err(db::map_db_err)?;
    match grant {
        None if served.provider_owner == registry::CATALOG_OWNER => Err(ApiError::forbidden(
            "profile is outside the visibility domain",
        )),
        None => Err(ApiError::not_found("no such profile")),
        Some(_) => Ok(Json(sanitize(&state, served, identity.bearer))),
    }
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
    identity: TagmaIdentity,
    Path(name): Path<String>,
) -> Result<Json<SanitizedSet>, ApiError> {
    // The row: set names are globally unique, so one row, in whatever
    // space owns it. The members read follows that space, not a
    // catalog assumption.
    let mut rows = registry::sets_by_name(&state.db, &name)
        .await
        .map_err(db::map_db_err)?;
    let Some(set) = rows.pop() else {
        return Err(ApiError::not_found("no such set"));
    };
    if !visibility::set_visible(&state.db, &identity.viewer, &name)
        .await
        .map_err(db::map_db_err)?
    {
        return Err(ApiError::forbidden("set is outside the visibility domain"));
    }
    Ok(Json(sanitized_set(&state, &identity, &set).await?))
}

/// Assemble one sanitized set (the /sets/{name} and the
/// selected-collection answer shape): the owner space's ordered
/// members, the sanitized
/// profiles.
async fn sanitized_set(
    state: &AppState,
    identity: &TagmaIdentity,
    set: &registry::profile_set::Model,
) -> Result<SanitizedSet, ApiError> {
    let profiles = registry::set_members_ordered(&state.db, &set.owner, &set.name)
        .await
        .map_err(db::map_db_err)?;
    Ok(SanitizedSet {
        name: set.name.clone(),
        description: set.description.clone(),
        profiles: profiles
            .into_iter()
            .map(|p| sanitize(state, p, identity.bearer.clone()))
            .collect(),
    })
}

/// GET /sets: the set names the identity's visibility domain reaches.
#[derive(Clone, Debug, Serialize)]
pub struct SetNames {
    pub sets: Vec<String>,
}

pub async fn get_sets(
    State(state): State<AppState>,
    identity: TagmaIdentity,
) -> Result<Json<SetNames>, ApiError> {
    let names = visibility::visible_set_names(&state.db, &identity.viewer)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(SetNames { sets: names }))
}

/// GET /parking: the catalog's parked drafts, any valid identity (a
/// parked draft joins the face only through set membership).
#[derive(Clone, Debug, Serialize)]
pub struct Parking {
    pub profiles: Vec<SanitizedProfile>,
}

pub async fn get_parking(
    State(state): State<AppState>,
    identity: TagmaIdentity,
) -> Result<Json<Parking>, ApiError> {
    let parked = registry::catalog_parked_profiles(&state.db)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(Parking {
        profiles: parked
            .into_iter()
            .map(|p| sanitize(&state, p, identity.bearer.clone()))
            .collect(),
    }))
}

// -- the per-tagma collection selection (the single store) -----------

/// PUT /selection body. The owner is optional: an absent owner means
/// the account's own space; any other owner addresses a shared or
/// platform collection.
#[derive(Debug, Deserialize)]
pub struct SelectionPut {
    #[serde(default)]
    pub owner: Option<String>,
    pub collection: String,
}

/// The stored pointer, echoed by the write.
#[derive(Clone, Debug, Serialize)]
pub struct SelectionView {
    pub owner: String,
    pub collection: String,
}

/// PUT /selection: point the tagma at one collection of its
/// account's visibility domain. An unknown and an out-of-domain
/// collection answer the same 404 (no existence oracle across
/// spaces); a re-PUT replaces the pointer whole.
pub async fn put_selection(
    State(state): State<AppState>,
    identity: TagmaIdentity,
    Json(body): Json<SelectionPut>,
) -> Result<Json<SelectionView>, ApiError> {
    let owner = body
        .owner
        .unwrap_or_else(|| identity.viewer.account_id.clone());
    use sea_orm::EntityTrait;
    let exists = registry::collection::Entity::find_by_id((owner.clone(), body.collection.clone()))
        .one(&state.db)
        .await
        .map_err(db::map_db_err)?
        .is_some();
    if !exists
        || !visibility::collection_visible(&state.db, &identity.viewer, &owner, &body.collection)
            .await
            .map_err(db::map_db_err)?
    {
        return Err(ApiError::not_found("no such collection"));
    }
    let row = registry::gateway_selection::ActiveModel {
        tagma_id: sea_orm::Set(identity.tagma_id.clone()),
        owner: sea_orm::Set(owner.clone()),
        collection_name: sea_orm::Set(body.collection.clone()),
        updated_at: sea_orm::Set(time::OffsetDateTime::now_utc()),
    };
    // The upsert keeps the one-row shape without racing two writers
    // into a duplicate.
    registry::gateway_selection::Entity::insert(row)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(registry::gateway_selection::Column::TagmaId)
                .update_columns([
                    registry::gateway_selection::Column::Owner,
                    registry::gateway_selection::Column::CollectionName,
                    registry::gateway_selection::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(&state.db)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(SelectionView {
        owner,
        collection: body.collection,
    }))
}

/// DELETE /selection: clear the pointer. Idempotent -- an absent
/// selection deletes nothing and still succeeds.
pub async fn delete_selection(
    State(state): State<AppState>,
    identity: TagmaIdentity,
) -> Result<axum::http::StatusCode, ApiError> {
    use sea_orm::EntityTrait;
    registry::gateway_selection::Entity::delete_by_id(identity.tagma_id.clone())
        .exec(&state.db)
        .await
        .map_err(db::map_db_err)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// GET /selected-collection: the collection the tagma selected,
/// with its member sets (each sanitized like /sets/{name}) and its
/// default-set anchor, in one read. The members filter to the
/// visibility domain: a publication revoked under the pointer
/// degrades the answer instead of leaking the members.
#[derive(Clone, Debug, Serialize)]
pub struct SelectedCollection {
    pub owner: String,
    pub collection: String,
    pub description: String,
    pub sets: Vec<SanitizedSet>,
    /// The effective default: the anchor when it names a member
    /// set this viewer can see, else the sole visible member set
    /// (derived, never written back), else `null`. A dangling
    /// anchor is absent from the answer, so the name always
    /// resolves inside `sets`.
    pub default_set: Option<String>,
}

pub async fn get_selected_collection(
    State(state): State<AppState>,
    identity: TagmaIdentity,
) -> Result<Json<SelectedCollection>, ApiError> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
    let Some(pointer) = registry::gateway_selection::Entity::find_by_id(identity.tagma_id.clone())
        .one(&state.db)
        .await
        .map_err(db::map_db_err)?
    else {
        return Err(ApiError::not_found("no collection selected"));
    };
    let Some(collection_row) = registry::collection::Entity::find_by_id((
        pointer.owner.clone(),
        pointer.collection_name.clone(),
    ))
    .one(&state.db)
    .await
    .map_err(db::map_db_err)?
    else {
        return Err(ApiError::not_found(
            "the selected collection no longer exists",
        ));
    };
    if !visibility::collection_visible(
        &state.db,
        &identity.viewer,
        &pointer.owner,
        &pointer.collection_name,
    )
    .await
    .map_err(db::map_db_err)?
    {
        return Err(ApiError::forbidden(
            "the selected collection is outside the visibility domain",
        ));
    }
    let mut member_names: Vec<String> = registry::profile_set::Entity::find()
        .filter(registry::profile_set::Column::Owner.eq(pointer.owner.clone()))
        .filter(registry::profile_set::Column::CollectionName.eq(pointer.collection_name.clone()))
        .order_by_asc(registry::profile_set::Column::Name)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?
        .into_iter()
        .map(|set| set.name)
        .collect();
    member_names.sort();
    member_names.dedup();
    let domain: std::collections::HashSet<String> =
        visibility::visible_set_names(&state.db, &identity.viewer)
            .await
            .map_err(db::map_db_err)?
            .into_iter()
            .collect();
    let mut sets = Vec::with_capacity(member_names.len());
    for name in member_names {
        if !domain.contains(&name) {
            continue;
        }
        let Some(set_row) = registry::sets_by_name(&state.db, &name)
            .await
            .map_err(db::map_db_err)?
            .pop()
        else {
            continue;
        };
        sets.push(sanitized_set(&state, &identity, &set_row).await?);
    }
    let default_set = match collection_row.default_set_name {
        Some(name) if sets.iter().any(|set| set.name == name) => Some(name),
        _ if sets.len() == 1 => Some(sets[0].name.clone()),
        _ => None,
    };
    Ok(Json(SelectedCollection {
        owner: pointer.owner,
        collection: pointer.collection_name,
        description: collection_row.description,
        default_set,
        sets,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TEST_TAGMA_BEARER, migrated_test_db};
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

    async fn json_of(response: axum::response::Response) -> serde_json::Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap_or(serde_json::Value::Null)
    }

    fn state_for(db: crate::db::Db) -> AppState {
        AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            management: crate::test_support::test_management(),
            identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
            metrics: crate::metrics::Metrics::private(),
        }
    }

    fn router_for_test(state: AppState) -> axum::Router {
        // The real management router: the distribution reads are mounted
        // on it, so the visibility and failure-vocabulary pins run
        // against the serving face itself. The full listener wiring
        // stays pinned in `data_plane::tests`.
        crate::routes::management_plane_router(state, "")
    }

    /// The whole distribution surface against the seeded fixture registry:
    /// sanitization shape, domain scoping, and the failure vocabulary.
    #[tokio::test]
    async fn distribution_round_trip_scopes_and_sanitizes() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let app = router_for_test(state_for(db));

        // /sets: the domain the bare test account reaches -- the
        // baseline catalog reach (it owns no user space).
        let response = app
            .clone()
            .oneshot(get("/sets", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body["sets"],
            serde_json::json!(["alpha", "beta", "user-set"])
        );

        // /sets/alpha: ordered members (failover order verbatim), sanitized.
        let response = app
            .clone()
            .oneshot(get("/sets/alpha", Some(TEST_TAGMA_BEARER)))
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
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[2]["model"], "gpt-x-responses");
        assert_eq!(profiles[0]["model"], "deepseek-chat");
        assert_eq!(profiles[1]["model"], "deepseek-researcher");
        assert_eq!(profiles[0]["base_url"], "http://gw.test:7501");
        assert_eq!(profiles[0]["api_key"], TEST_TAGMA_BEARER);
        assert_eq!(profiles[0]["modalities"], serde_json::json!(["text"]));
        let text = body.to_string();
        assert!(
            !text.contains("upstream"),
            "no upstream field may leak: {text}"
        );

        // /profiles/<id>: visible, outside the domain, unknown. A
        // beta-set catalog row is in the domain; the platform-group
        // pro catalog row is not (403 -- catalog knowledge, no grant);
        // a ghost is 404.
        let response = app
            .clone()
            .oneshot(get("/profiles/p1", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(get("/profiles/p3", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(get("/profiles/p7", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(get("/profiles/ghost", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // /sets/pro-set: exists but outside the domain -- 403
        // default-deny (set names are globally unique, so no per-space
        // oracle applies).
        let response = app
            .clone()
            .oneshot(get("/sets/pro-set", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // /parking: parked drafts.
        let response = app
            .clone()
            .oneshot(get("/parking", Some(TEST_TAGMA_BEARER)))
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

        // Failure vocabulary on the bearer: missing and unknown -> 401.
        let response = app.clone().oneshot(get("/sets", None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .oneshot(get("/sets", Some("not-a-token")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// The unknown bearer keeps the bare 401: no code, no existence oracle.
    #[tokio::test]
    async fn unknown_bearer_stays_the_bare_unauthorized() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let app = router_for_test(state_for(db));
        let response = app
            .oneshot(get("/sets", Some("never-minted-bearer")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_of(response).await;
        assert!(body["error"]["code"].is_null(), "{body}");
    }

    /// The visibility domains at the HTTP face: the bare account, the
    /// bound account with its publications, and the platform-group
    /// member's extra catalog reach. The faces split the failure
    /// codes: the profile face answers 404 for a cross-account row (no
    /// existence oracle across accounts) and 403 for a catalog row
    /// outside the domain, while the set face answers 403 default-deny
    /// for every set outside the domain (set names are globally
    /// unique, so no per-space oracle applies).
    #[tokio::test]
    async fn visibility_domains_at_the_face() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let management = crate::management::AdminAuth::Platform(std::sync::Arc::new(
            crate::test_support::MockVerifier::enrolled(&[
                ("tagma-under-test", "user-plain"),
                ("tagma-bound", "acc-bound"),
                ("tagma-other", "acc-other"),
            ])
            .with_tagmas(&[
                (crate::test_support::TEST_TAGMA_BEARER, "tagma-under-test"),
                (crate::test_support::TEST_TAGMA_BEARER_BOUND, "tagma-bound"),
                (crate::test_support::TEST_TAGMA_BEARER_OTHER, "tagma-other"),
            ]),
        ));
        let state = AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            management,
            identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
            metrics: crate::metrics::Metrics::private(),
        };
        let app = router_for_test(state);

        // The bare account: the catalog plus the everyone-publication.
        let response = app
            .clone()
            .oneshot(get("/sets", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        let body = json_of(response).await;
        assert_eq!(
            body["sets"],
            serde_json::json!(["alpha", "beta", "user-set"])
        );
        let response = app
            .clone()
            .oneshot(get("/profiles/u1", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // The bound account: its own publications join the face.
        let bound = crate::test_support::TEST_TAGMA_BEARER_BOUND;
        let response = app
            .clone()
            .oneshot(get("/sets", Some(bound)))
            .await
            .unwrap();
        let body = json_of(response).await;
        assert_eq!(
            body["sets"],
            serde_json::json!(["alpha", "beta", "grp-set", "own-set", "user-set"])
        );
        let response = app
            .clone()
            .oneshot(get("/profiles/u1", Some(bound)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(get("/profiles/u2", Some(bound)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = app
            .clone()
            .oneshot(get("/sets/other-set", Some(bound)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(get("/sets/ghost", Some(bound)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // The platform-group member: the group's catalog joins the face.
        let other = crate::test_support::TEST_TAGMA_BEARER_OTHER;
        let response = app
            .clone()
            .oneshot(get("/sets", Some(other)))
            .await
            .unwrap();
        let body = json_of(response).await;
        assert_eq!(
            body["sets"],
            serde_json::json!([
                "alpha",
                "beta",
                "grp-set",
                "other-set",
                "pro-set",
                "user-set"
            ])
        );
    }

    /// A tagma whose owner account is disabled fails the archeion's
    /// bearer verification: the same bare 401 an unknown token gets
    /// (no oracle on the account state), and other tagmas on the same
    /// deployment keep distributing.
    #[tokio::test]
    async fn disabled_owner_fails_the_bearer_verification() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let management = crate::management::AdminAuth::Platform(std::sync::Arc::new(
            crate::test_support::MockVerifier::disabling(&["tagma-bound"]).with_tagmas(&[
                (TEST_TAGMA_BEARER, "tagma-under-test"),
                (crate::test_support::TEST_TAGMA_BEARER_BOUND, "tagma-bound"),
            ]),
        ));
        let state = AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            management,
            identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
            metrics: crate::metrics::Metrics::private(),
        };
        let app = router_for_test(state);

        let response = app
            .clone()
            .oneshot(get(
                "/sets",
                Some(crate::test_support::TEST_TAGMA_BEARER_BOUND),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_of(response).await;
        assert_eq!(body["error"]["message"], "unknown tagma token", "{body}");

        let response = app
            .oneshot(get("/sets", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A tagma the authority cannot address resolves to the bare 401 --
    /// the same answer an unknown token gets, so the enrollment state
    /// leaks no oracle.
    #[tokio::test]
    async fn unaddressable_tagma_answers_the_bare_401() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let management = crate::management::AdminAuth::Platform(std::sync::Arc::new(
            crate::test_support::MockVerifier::unenrolled(),
        ));
        let state = AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            management,
            identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
            metrics: crate::metrics::Metrics::private(),
        };
        let app = router_for_test(state);

        let response = app
            .oneshot(get("/sets", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_of(response).await;
        assert_eq!(body["error"]["message"], "unknown tagma token", "{body}");
    }

    /// An unreachable authority fails the resolution closed: the
    /// shared 503, never a fallthrough to serving.
    #[tokio::test]
    async fn unreachable_authority_fails_resolution_closed() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let management = crate::management::AdminAuth::Platform(std::sync::Arc::new(
            crate::test_support::MockVerifier::unreachable(),
        ));
        let state = AppState {
            db,
            public_base_url: "http://gw.test:7501".to_string(),
            management,
            identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
            metrics: crate::metrics::Metrics::private(),
        };
        let app = router_for_test(state);

        let response = app
            .oneshot(get("/sets", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = json_of(response).await;
        assert_eq!(body["error"]["code"], "auth_backend_unavailable", "{body}");
    }

    /// A PUT/DELETE request helper beside the GET one.
    fn req_with_body(
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: serde_json::Value,
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(bearer) = bearer {
            builder = builder.header("authorization", format!("Bearer {bearer}"));
        }
        builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    /// The selection face: the write validates the domain, the read
    /// serves the member sets in one shot, and the delete is
    /// idempotent.
    #[tokio::test]
    async fn selection_round_trip_scopes_and_clears() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let management = crate::management::AdminAuth::Platform(std::sync::Arc::new(
            crate::test_support::MockVerifier::enrolled(&[
                ("tagma-under-test", "user-plain"),
                ("tagma-bound", "acc-bound"),
            ])
            .with_tagmas(&[
                (TEST_TAGMA_BEARER, "tagma-under-test"),
                (crate::test_support::TEST_TAGMA_BEARER_BOUND, "tagma-bound"),
            ]),
        ));
        let state = AppState {
            db: db.clone(),
            public_base_url: "http://gw.test:7501".to_string(),
            management,
            identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
            metrics: crate::metrics::Metrics::private(),
        };
        let app = router_for_test(state);
        let bound = crate::test_support::TEST_TAGMA_BEARER_BOUND;

        // No selection yet: the read names the state.
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(TEST_TAGMA_BEARER)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = json_of(response).await;
        assert_eq!(body["error"]["message"], "no collection selected");

        // Another space's unpublished collection is a 404 either way
        // (no existence oracle across spaces).
        let response = app
            .clone()
            .oneshot(req_with_body(
                "PUT",
                "/selection",
                Some(TEST_TAGMA_BEARER),
                serde_json::json!({"owner": "acc-bound", "collection": "own-col"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // The bound account's own unpublished collection selects fine.
        let response = app
            .clone()
            .oneshot(req_with_body(
                "PUT",
                "/selection",
                Some(bound),
                serde_json::json!({"owner": "acc-bound", "collection": "own-col"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(body["owner"], "acc-bound");
        assert_eq!(body["collection"], "own-col");

        // An absent owner means the account's own space.
        let response = app
            .clone()
            .oneshot(req_with_body(
                "PUT",
                "/selection",
                Some(bound),
                serde_json::json!({"collection": "own-col"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(body["owner"], "acc-bound");
        assert_eq!(body["collection"], "own-col");
        // The one-read shape: the collection's member sets, sanitized.
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(bound)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(body["collection"], "own-col");
        assert_eq!(body["sets"].as_array().unwrap().len(), 1);
        assert_eq!(body["sets"][0]["name"], "own-set");
        assert_eq!(body["sets"][0]["profiles"][0]["model"], "bound-model");
        assert_eq!(body["default_set"], "own-set");

        // A re-PUT replaces the pointer whole.
        let response = app
            .clone()
            .oneshot(req_with_body(
                "PUT",
                "/selection",
                Some(bound),
                serde_json::json!({"owner": "system", "collection": "baseline"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(bound)))
            .await
            .unwrap();
        let body = json_of(response).await;
        let names: Vec<&str> = body["sets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(body["default_set"].is_null());

        // The effective default: an anchor that names a visible member
        // serves as-is, a missing anchor over one visible member derives
        // that member, and a dangling anchor never survives the answer.
        reanchor(&db, "system", "baseline", Some("alpha")).await;
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(bound)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(body["default_set"], "alpha");

        reanchor(&db, "system", "baseline", Some("ghost")).await;
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(bound)))
            .await
            .unwrap();
        let body = json_of(response).await;
        assert!(body["default_set"].is_null());

        // A dangling anchor over a single visible member derives that
        // member instead.
        let response = app
            .clone()
            .oneshot(req_with_body(
                "PUT",
                "/selection",
                Some(bound),
                serde_json::json!({"owner": "acc-bound", "collection": "own-col"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        reanchor(&db, "acc-bound", "own-col", Some("ghost")).await;
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(bound)))
            .await
            .unwrap();
        let body = json_of(response).await;
        assert_eq!(body["default_set"], "own-set");

        // The delete clears, and clearing an absent selection is
        // still a success.
        let response = app
            .clone()
            .oneshot(req_with_body(
                "DELETE",
                "/selection",
                Some(bound),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let response = app
            .clone()
            .oneshot(req_with_body(
                "DELETE",
                "/selection",
                Some(bound),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(bound)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Point a seeded collection's default-set anchor straight at
    /// the row (the management PATCH path is exercised elsewhere;
    /// the distribution face only reads the column).
    async fn reanchor(
        db: &sea_orm::DatabaseConnection,
        owner: &str,
        collection: &str,
        set: Option<&str>,
    ) {
        use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel};
        let mut row =
            registry::collection::Entity::find_by_id((owner.to_string(), collection.to_string()))
                .one(db)
                .await
                .expect("find collection")
                .expect("collection row")
                .into_active_model();
        row.default_set_name = sea_orm::Set(set.map(str::to_string));
        row.update(db).await.expect("reanchor");
    }

    /// Two tagmas enrolled to one account keep independent selections:
    /// the store keys by tagma, so one tagma's switch never moves
    /// the other's pull (the account-keyed store shared one row
    /// across every sibling).
    #[tokio::test]
    async fn two_tagmas_on_one_account_select_independently() {
        let db = migrated_test_db().await;
        crate::test_support::seed_registry(&db, "https://api.upstream.test").await;
        let management = crate::management::AdminAuth::Platform(std::sync::Arc::new(
            crate::test_support::MockVerifier::enrolled(&[
                ("tagma-a", "acc-bound"),
                ("tagma-b", "acc-bound"),
            ])
            .with_tagmas(&[
                ("bearer-a-tagma-pick-0123456789ab", "tagma-a"),
                ("bearer-b-tagma-pick-0123456789ab", "tagma-b"),
            ]),
        ));
        let state = AppState {
            db: db.clone(),
            public_base_url: "http://gw.test:7501".to_string(),
            management,
            identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
            metrics: crate::metrics::Metrics::private(),
        };
        let app = router_for_test(state);
        let a = "bearer-a-tagma-pick-0123456789ab";
        let b = "bearer-b-tagma-pick-0123456789ab";

        // Tagma a picks the account's own collection; tagma b, on
        // the same account, still has no selection of its own.
        let response = app
            .clone()
            .oneshot(req_with_body(
                "PUT",
                "/selection",
                Some(a),
                serde_json::json!({"collection": "own-col"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(b)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Tagma b picks the platform catalog; a's pull is untouched.
        let response = app
            .clone()
            .oneshot(req_with_body(
                "PUT",
                "/selection",
                Some(b),
                serde_json::json!({"owner": "system", "collection": "baseline"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(a)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(body["collection"], "own-col");
        let response = app
            .clone()
            .oneshot(get("/selected-collection", Some(b)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(body["collection"], "baseline");
    }
}
