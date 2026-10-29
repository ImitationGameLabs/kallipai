//! Shared fixtures for the management-face domain tests: the app
//! harness, the request builders, the seeded-state helpers, and the
//! audit readers every domain module builds its tests on.

use crate::registry;
use crate::registry::{collection, profile, profile_set, provider};
use crate::secret::provider_credential;
use crate::state::AppState;
pub(crate) use crate::test_support::{
    TEST_ADMIN_BEARER, TEST_ADMIN_COOKIE, TEST_TAGMA_BEARER, TEST_USER_COOKIE,
};
use crate::test_support::{migrated_test_db, seed_registry};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use tower::ServiceExt;

/// The local platform administrator's private owner (the creator
/// constant, one spelling with the audit attribution).
pub(crate) const LOCAL_ADMIN_OWNER: &str = "admin";

/// The catalog collection the plain planting helpers anchor to: the
/// migration-seeded `baseline` bundle, which is where the fixture sets
/// (`alpha`, `beta`) live. Helpers that need a different collection take
/// it as an argument (`*_in`).
pub(crate) const TEST_COLLECTION: &str = "baseline";

pub(crate) fn app(state: AppState) -> axum::Router {
    crate::routes::management_plane_router(state, "")
}

/// A management-face request: bearer + optional JSON body.
pub(crate) fn req(
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let body = match body {
        Some(v) => {
            // axum's Json extractor requires the content-type header.
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_string(&v).expect("test body serializes"))
        }
        None => Body::empty(),
    };
    builder.body(body).expect("request builds")
}

/// The seeded fixtures live in the catalog space -- the one space the
/// admin face manages; tests plant their extra rows beside them.
pub(crate) async fn seeded_state() -> AppState {
    let db = migrated_test_db().await;
    seed_registry(&db, "https://api.upstream.test").await;
    AppState {
        db,
        public_base_url: "http://gw.test:7501".to_owned(),
        management: crate::test_support::test_management(),
        identity_cache: std::sync::Arc::new(crate::secret::IdentityCache::default()),
        metrics: crate::metrics::Metrics::private(),
    }
}

/// A provider in the catalog space: the create-parked reference
/// target (the profile's family derives from this row).
pub(crate) async fn catalog_provider(state: &AppState, id: &str, family: &str) {
    provider::ActiveModel {
        owner: Set(registry::CATALOG_OWNER.to_owned()),
        provider_id: Set(id.to_owned()),
        family: Set(family.to_owned()),
        base_url: Set(None),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .expect("catalog provider insert");
}

/// A profile row in the catalog space: the provider lands first
/// (the FK needs it), then the profile in the given set, if any.
pub(crate) async fn catalog_profile(
    state: &AppState,
    id: &str,
    model: &str,
    set: Option<(&str, i32)>,
) {
    catalog_profile_in(state, TEST_COLLECTION, id, model, set).await;
}

/// The same profile row anchored to a named collection.
pub(crate) async fn catalog_profile_in(
    state: &AppState,
    collection: &str,
    id: &str,
    model: &str,
    set: Option<(&str, i32)>,
) {
    catalog_provider(state, id, "openai-compatible").await;
    profile::ActiveModel {
        collection_name: Set(collection.to_owned()),
        profile_id: Set(id.to_owned()),
        provider_id: Set(id.to_owned()),
        model: Set(model.to_owned()),
        max_context_window: Set(Some(128_000)),
        effort: Set(Some("high".to_owned())),
        modalities: Set(Some("[\"text\"]".to_owned())),
        set_name: Set(set.map(|(name, _)| name.to_owned())),
        position: Set(set.map_or(0, |(_, position)| position)),
        store: Set(None),
        owner: Set(registry::CATALOG_OWNER.to_owned()),
    }
    .insert(&state.db)
    .await
    .expect("catalog profile insert");
}

/// A provider credential planted directly: the secret half of the
/// pool on the same composite key as the provider row above.
pub(crate) async fn catalog_credential(state: &AppState, id: &str, key: &str) {
    provider_credential::ActiveModel {
        owner: Set(registry::CATALOG_OWNER.to_owned()),
        provider_id: Set(id.to_owned()),
        api_key: Set(key.to_owned()),
    }
    .insert(&state.db)
    .await
    .expect("catalog credential insert");
}

/// A set row planted directly into the baseline bundle (the
/// catalog space's default collection), so tests can pin
/// membership edges without the collection face.
pub(crate) async fn catalog_set(state: &AppState, name: &str) {
    catalog_set_in(state, TEST_COLLECTION, name).await;
}

/// The same set row anchored to a named collection (a same-name twin in
/// another collection is a different row, which is the point).
pub(crate) async fn catalog_set_in(state: &AppState, collection: &str, name: &str) {
    profile_set::ActiveModel {
        name: Set(name.to_owned()),
        description: Set("d".to_owned()),
        owner: Set(registry::CATALOG_OWNER.to_owned()),
        collection_name: Set(collection.to_owned()),
    }
    .insert(&state.db)
    .await
    .expect("catalog set insert");
}

/// Membership is a profile column: point the planted profile at the set
/// (the set row must exist in the same collection first).
pub(crate) async fn catalog_member(state: &AppState, set: &str, id: &str, position: i32) {
    catalog_member_in(state, TEST_COLLECTION, set, id, position).await;
}

/// The same membership update for a set and a profile in a named collection.
pub(crate) async fn catalog_member_in(
    state: &AppState,
    collection: &str,
    set: &str,
    id: &str,
    position: i32,
) {
    let current = profile::Entity::find()
        .filter(profile::Column::CollectionName.eq(collection))
        .filter(profile::Column::ProfileId.eq(id))
        .one(&state.db)
        .await
        .expect("profile read")
        .expect("planted profile");
    profile::ActiveModel {
        set_name: Set(Some(set.to_owned())),
        position: Set(position),
        ..current.into()
    }
    .update(&state.db)
    .await
    .expect("catalog membership update");
}

/// A provider and an unassigned profile in the admin account's user
/// space (a foreign space to this face), planted directly: no
/// admin route reaches that space.
pub(crate) async fn foreign_profile(state: &AppState, id: &str) {
    seed_collection(state, LOCAL_ADMIN_OWNER, "personal").await;
    provider::ActiveModel {
        owner: Set(LOCAL_ADMIN_OWNER.to_owned()),
        provider_id: Set(id.to_owned()),
        family: Set("deepseek".to_owned()),
        base_url: Set(None),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .expect("foreign provider insert");
    profile::ActiveModel {
        collection_name: Set("personal".to_owned()),
        profile_id: Set(id.to_owned()),
        provider_id: Set(id.to_owned()),
        model: Set("foreign-model".to_owned()),
        max_context_window: Set(Some(128_000)),
        effort: Set(None),
        modalities: Set(Some("[\"text\"]".to_owned())),
        set_name: Set(None),
        position: Set(0),
        store: Set(None),
        owner: Set(LOCAL_ADMIN_OWNER.to_owned()),
    }
    .insert(&state.db)
    .await
    .expect("foreign profile insert");
}

pub(crate) async fn send(
    app: axum::Router,
    request: Request<Body>,
) -> (StatusCode, serde_json::Value) {
    let response = app.oneshot(request).await.expect("infallible oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads");
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, body)
}

/// A collection row in a chosen space: planted sets anchor to one
/// (the catalog space seeds `baseline` via the migrations; tests
/// plant everything else directly).
pub(crate) async fn seed_collection(state: &AppState, owner: &str, name: &str) {
    collection::ActiveModel {
        owner: Set(owner.to_owned()),
        name: Set(name.to_owned()),
        description: Set("d".to_owned()),
        default_set_name: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("seed collection");
}

pub(crate) async fn audit_rows(
    db: &sea_orm::DatabaseConnection,
) -> Vec<crate::audit::management_event::Model> {
    crate::audit::management_event::Entity::find()
        .all(db)
        .await
        .expect("audit rows read")
}

pub(crate) async fn set_of(state: &AppState, collection: &str, id: &str) -> Option<String> {
    profile::Entity::find()
        .filter(profile::Column::CollectionName.eq(collection))
        .filter(profile::Column::ProfileId.eq(id))
        .one(&state.db)
        .await
        .expect("profile read")
        .expect("seeded row")
        .set_name
}

// The folded shape: a1b rides a1's provider row, so every handler
// below must address the provider by the profile's link column,
// not by the profile's own id.
pub(crate) async fn insert_co_rider(
    db: &crate::db::Db,
    owner: &str,
    collection: &str,
    id: &str,
    provider: &str,
) {
    profile::ActiveModel {
        collection_name: Set(collection.to_owned()),
        profile_id: Set(id.to_owned()),
        provider_id: Set(provider.to_owned()),
        model: Set("deepseek-chat".to_owned()),
        max_context_window: Set(Some(128_000)),
        effort: Set(Some("high".to_owned())),
        modalities: Set(Some("[\"text\"]".to_owned())),
        set_name: Set(None),
        position: Set(0),
        store: Set(None),
        owner: Set(owner.to_owned()),
    }
    .insert(db)
    .await
    .expect("insert co-rider");
}

/// A cookie-channel request: optional session cookie + CSRF marker.
pub(crate) fn cookie_req(
    method: &str,
    path: &str,
    cookie: Option<&str>,
    csrf_marker: bool,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(c) = cookie {
        builder = builder.header("cookie", format!("kallipai_session={c}"));
    }
    if csrf_marker {
        builder = builder.header("x-requested-with", "kallipai");
    }
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_string(&v).expect("test body serializes"))
        }
        None => Body::empty(),
    };
    builder.body(body).expect("request builds")
}

pub(crate) fn parts_with_cookie(cookie: &str) -> axum::http::request::Parts {
    let (parts, _) = axum::http::Request::builder()
        .header("cookie", format!("kallipai_session={cookie}"))
        .body(())
        .expect("request builds")
        .into_parts();
    parts
}

pub(crate) async fn create_group(
    state: &AppState,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    send(
        app(state.clone()),
        cookie_req(
            "POST",
            "/admin/groups",
            Some(TEST_ADMIN_COOKIE),
            true,
            Some(body),
        ),
    )
    .await
}

/// A provider+profile pair in the signed-in user's space, the profile
/// anchored to the named collection: the cascade tests need real profile
/// rows to delete with their collection (or to spare), and a profile
/// belongs to exactly one collection.
pub(crate) async fn user_provider_profile(
    state: &AppState,
    collection: &str,
    provider: &str,
    profile: &str,
) {
    let profile_path = format!("/user/collections/{collection}/profiles");
    let provider_body = serde_json::json!({
        "provider_id": provider,
        "family": "deepseek",
        "base_url": "https://api.user.test",
        "api_key": "sk-user-x",
    });
    let profile_body = serde_json::json!({
        "profile_id": profile,
        "provider_id": provider,
        "model": "m",
    });
    for (path, body) in [
        ("/user/providers", provider_body),
        (profile_path.as_str(), profile_body),
    ] {
        let (status, body) = send(
            app(state.clone()),
            cookie_req("POST", path, Some(TEST_USER_COOKIE), true, Some(body)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}
