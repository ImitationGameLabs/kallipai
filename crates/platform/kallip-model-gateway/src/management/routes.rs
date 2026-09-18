//! The management route table: registry CRUD (sets, parking), the upstream
//! credential store, the default marker, and the fail-closed audit that
//! wraps every mutation.
//!
//! Invalidation contract: the reads here hit the store, and every write
//! clears the distribution face's process-local key cache once its
//! transaction has committed (`state.key_cache.clear()`), so a change is
//! visible to proxy-key resolution on the next request -- no restart, no
//! TTL to wait out. The store stays the source of truth; the cache is
//! only the warm read path. In-flight resolves are covered by the
//! cache's generation guard: a store snapshot taken before a write
//! committed cannot backfill after the clear.
//!
//! Scope assumption (single-writer): every management write goes through
//! this process's admin face. A write landed by any other path (another
//! process writing the store directly) clears no cache here and leaves
//! entry staleness with no upper bound. If the face ever scales to
//! multiple instances, this contract needs a shared invalidation or a
//! TTL.
//!
//! Mutation shape (every POST/PUT/DELETE here): open a transaction, read
//! the current state, apply the change, land the `management_events` row,
//! commit. An audit failure therefore rolls the change back (fail-closed)
//! -- the deliberate opposite of the forwarding path's best-effort request
//! audit, which runs after the upstream cost is spent and has a hedge this
//! face does not (a management change is the only trace of itself). Never
//! weaken this by analogy with the forwarding path.
//!
//! Secret contract: upstream API keys cross this face inbound only. Every
//! response -- writes included -- carries the mask, never the plaintext,
//! and the audit rows carry the mask too.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{delete, get, post, put};
use sea_orm::prelude::*;
use sea_orm::{ActiveModelTrait, QueryOrder, Set, TransactionTrait};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::audit::management_event::{self, ManagementEventRow};
use crate::audit::record_lifecycle_event;
use crate::db;
use crate::management::AdminToken;
use crate::registry::{self, DEFAULT_SET_KEY, profile, profile_set, registry_meta, set_member};
use crate::secret::{self, proxy_key, proxy_key_set, tagma, upstream_credential};
use crate::state::AppState;
use kallip_common::protocol::ApiError;

/// The response wrapper for successful deletions. `warning` carries the
/// static risk hint on set deletion (the proxy does
/// not track per-agent binds, so it cannot enumerate what dangles).
#[derive(Serialize)]
pub struct Deleted {
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The admin view of a profile: the full registry row, no credential
/// material (that lives in the secret store, keyed by profile id).
#[derive(Clone, Debug, Serialize)]
pub struct AdminProfile {
    pub profile_id: String,
    pub family: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Vec<String>,
    pub parked: bool,
    pub max_budget: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
}

impl From<profile::Model> for AdminProfile {
    fn from(p: profile::Model) -> Self {
        let modalities = p
            .modalities
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .unwrap_or_default();
        Self {
            profile_id: p.profile_id,
            family: p.family,
            model: p.model,
            max_context_window: p.max_context_window,
            effort: p.effort,
            modalities,
            parked: p.parked,
            max_budget: p.max_budget,
            tpm_limit: p.tpm_limit,
            rpm_limit: p.rpm_limit,
        }
    }
}

/// The admin view of one set: identity plus the ordered members (the
/// failover order, verbatim -- every read path must preserve it).
#[derive(Clone, Debug, Serialize)]
pub struct AdminSet {
    pub name: String,
    pub description: String,
    pub profiles: Vec<AdminProfile>,
    /// Set only on mutation responses that degrade the default-set path
    /// (the face warns instead of guarding; the degradation itself is
    /// defined on the forwarding face).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SetSummary {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SetSummaries {
    pub sets: Vec<SetSummary>,
}

#[derive(Deserialize)]
pub struct SetCreate {
    pub name: String,
    pub description: String,
}

/// PUT /admin/sets/{name}: `description` and `members` are independent --
/// absent means unchanged. `members`, when present, replaces the whole
/// membership (the list order is the failover order).
#[derive(Deserialize)]
pub struct SetUpdate {
    pub description: Option<String>,
    pub members: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct ProfileCreate {
    pub profile_id: String,
    pub family: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
    pub max_budget: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
}

/// PUT /admin/parking/{id}: full-field replacement (plain PUT semantics --
/// the body is the complete desired state, `parked` included, so a promote
/// to served and a demote back to draft are the same endpoint).
#[derive(Deserialize)]
pub struct ProfilePut {
    pub family: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    pub modalities: Option<Vec<String>>,
    pub parked: bool,
    pub max_budget: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
}

#[derive(Deserialize)]
pub struct SecretPut {
    pub upstream_base_url: String,
    pub upstream_api_key: String,
}

/// The credential view: metadata and the mask. The plaintext has no path
/// out of this face -- not in writes, not in lists, not in the audit.
#[derive(Clone, Debug, Serialize)]
pub struct SecretView {
    pub profile_id: String,
    pub upstream_base_url: String,
    pub api_key_masked: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SecretViews {
    pub secrets: Vec<SecretView>,
}

#[derive(Deserialize)]
pub struct DefaultPut {
    pub default_set: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DefaultView {
    pub default_set: String,
}

/// The openai-wire families the forwarding path can drive (the
/// BackendFactory dispatch set). Registration rejects anything else: a
/// profile no backend can serve must not enter the registry (the registry
/// module's "admin-face registration concern").
const WIRE_FAMILIES: [&str; 2] = ["deepseek", "openai-compatible"];

fn validate_family(family: &str) -> Result<(), ApiError> {
    if WIRE_FAMILIES.contains(&family) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "unsupported family {family:?}: the openai-wire families are {WIRE_FAMILIES:?}"
        )))
    }
}

/// Set names ride in URL path segments on the detail routes, so a name
/// containing the segment separator would create an unaddressable set.
fn validate_set_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty() {
        return Err(ApiError::bad_request("set name must not be empty"));
    }
    if name.contains('/') {
        return Err(ApiError::bad_request(
            "set name must not contain '/' (names are path-addressed)",
        ));
    }
    Ok(())
}

/// Profile ids ride in URL path segments on the parking detail routes, so
/// an id containing the segment separator would create an unaddressable
/// draft (same rule as [`validate_set_name`]).
fn validate_profile_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty() {
        return Err(ApiError::bad_request("profile_id must not be empty"));
    }
    if id.contains('/') {
        return Err(ApiError::bad_request(
            "profile_id must not contain '/' (ids are path-addressed)",
        ));
    }
    Ok(())
}

fn validate_modalities(modalities: &Option<Vec<String>>) -> Result<(), ApiError> {
    match modalities {
        None => Ok(()),
        Some(v) if v.iter().all(|m| !m.is_empty()) => Ok(()),
        Some(_) => Err(ApiError::bad_request(
            "modality entries must be non-empty strings",
        )),
    }
}

fn encode_modalities(modalities: &Option<Vec<String>>) -> Option<String> {
    modalities
        .as_ref()
        .map(|v| serde_json::to_string(v).expect("Vec<String> always serializes"))
}

/// The display mask for an upstream key (the shared mask convention: head,
/// `***`, tail -- identification without exposure).
fn mask_key(key: &str) -> String {
    kallip_common::authtoken::mask_token(key, kallip_common::authtoken::TokenKind(""))
}

fn masked_credential_json(view: &SecretView) -> serde_json::Value {
    serde_json::json!({
        "profile_id": view.profile_id,
        "upstream_base_url": view.upstream_base_url,
        "api_key_masked": view.api_key_masked,
    })
}

fn secret_view(profile_id: &str, base_url: &str, key: &str) -> SecretView {
    SecretView {
        profile_id: profile_id.to_owned(),
        upstream_base_url: base_url.to_owned(),
        api_key_masked: mask_key(key),
    }
}

/// The audit JSON for a set: identity plus ordered member ids. `None` =
/// no such set (the handler turns that into the face's 404).
async fn set_state_json<C: sea_orm::ConnectionTrait>(
    txn: &C,
    name: &str,
) -> Result<Option<serde_json::Value>, DbErr> {
    let Some(set) = profile_set::Entity::find_by_id(name).one(txn).await? else {
        return Ok(None);
    };
    let members = set_member::Entity::find()
        .filter(set_member::Column::SetName.eq(name))
        .order_by_asc(set_member::Column::Position)
        .all(txn)
        .await?;
    Ok(Some(serde_json::json!({
        "name": set.name,
        "description": set.description,
        "members": members.into_iter().map(|m| m.profile_id).collect::<Vec<_>>(),
    })))
}

pub fn router() -> Router<AppState> {
    Router::new()
        // -- sets -----------------------------------------------------------
        .route("/sets", post(create_set).get(list_sets))
        .route(
            "/sets/{name}",
            get(get_set).put(update_set).delete(delete_set),
        )
        // -- parking --------------------------------------------------------
        .route("/parking", post(create_parked).get(list_parked))
        .route(
            "/parking/{profile_id}",
            get(get_parked).put(update_parked).delete(delete_parked),
        )
        // -- upstream credentials (the secret store) -------------------------
        .route("/secrets", get(list_secrets))
        .route(
            "/secrets/{profile_id}",
            put(put_secret).delete(delete_secret),
        )
        // -- the default marker ----------------------------------------------
        .route("/default", put(put_default).get(get_default))
        // -- proxy keys -------------------------------------------------------
        .route("/keys", post(create_key).get(list_keys))
        .route("/keys/{key_id}", delete(revoke_key))
}

// -- sets ----------------------------------------------------------------

pub async fn create_set(
    State(state): State<AppState>,
    _admin: AdminToken,
    Json(body): Json<SetCreate>,
) -> Result<Json<AdminSet>, ApiError> {
    validate_set_name(&body.name)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    if registry::set_by_name(&txn, &body.name)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict("set already exists"));
    }
    let description = body.description;
    profile_set::ActiveModel {
        name: Set(body.name.clone()),
        description: Set(description.clone()),
    }
    .insert(&txn)
    .await
    .map_err(db::map_db_err)?;
    let after = set_state_json(&txn, &body.name)
        .await
        .map_err(db::map_db_err)?
        .expect("the set was just inserted");
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "profile_set",
            entity_id: body.name.clone(),
            before: None,
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(AdminSet {
        name: body.name,
        description,
        profiles: vec![],
        warning: None,
    }))
}

pub async fn list_sets(
    State(state): State<AppState>,
    _admin: AdminToken,
) -> Result<Json<SetSummaries>, ApiError> {
    let sets = profile_set::Entity::find()
        .order_by_asc(profile_set::Column::Name)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(SetSummaries {
        sets: sets
            .into_iter()
            .map(|s| SetSummary {
                name: s.name,
                description: s.description,
            })
            .collect(),
    }))
}

pub async fn get_set(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(name): Path<String>,
) -> Result<Json<AdminSet>, ApiError> {
    let set = registry::set_by_name(&state.db, &name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such set"))?;
    let profiles = registry::set_members_ordered(&state.db, &name)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(AdminSet {
        name: set.name,
        description: set.description,
        profiles: profiles.into_iter().map(AdminProfile::from).collect(),
        warning: None,
    }))
}

pub async fn update_set(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(name): Path<String>,
    Json(body): Json<SetUpdate>,
) -> Result<Json<AdminSet>, ApiError> {
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let before = set_state_json(&txn, &name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such set"))?;
    if let Some(description) = &body.description {
        profile_set::ActiveModel {
            name: Set(name.clone()),
            description: Set(description.clone()),
        }
        .update(&txn)
        .await
        .map_err(db::map_db_err)?;
    }
    if let Some(members) = &body.members {
        // Duplicate ids cannot carry two positions; unknown ids would make
        // the set unresolvable for the forwarding face. Both checks run
        // before any write, and the transaction rolls back on either.
        let mut seen = std::collections::HashSet::new();
        for id in members {
            if !seen.insert(id) {
                return Err(ApiError::bad_request(format!(
                    "duplicate member {id:?} in the members list"
                )));
            }
        }
        for id in members {
            if registry::profile_by_id(&txn, id)
                .await
                .map_err(db::map_db_err)?
                .is_none()
            {
                return Err(ApiError::not_found(format!("no such profile: {id}")));
            }
        }
        set_member::Entity::delete_many()
            .filter(set_member::Column::SetName.eq(&name))
            .exec(&txn)
            .await
            .map_err(db::map_db_err)?;
        for (position, id) in members.iter().enumerate() {
            set_member::ActiveModel {
                set_name: Set(name.clone()),
                profile_id: Set(id.clone()),
                position: Set(position as i32),
            }
            .insert(&txn)
            .await
            .map_err(db::map_db_err)?;
        }
    }
    // Draining the default set is allowed -- the forwarding face's
    // default path degrades to 404, which is defined behavior -- but the
    // response carries an explicit warning. The before/after audit row
    // is the record; this is the attention getter.
    let warning = match &body.members {
        Some(members) if members.is_empty() => {
            let drained_default = registry::default_set_name(&txn)
                .await
                .map_err(db::map_db_err)?
                .is_some_and(|default| default == name);
            drained_default.then(|| {
                "the default set now has no members; requests without a ".to_owned()
                    + "profile header fail until a member is added"
            })
        }
        _ => None,
    };
    let after = set_state_json(&txn, &name)
        .await
        .map_err(db::map_db_err)?
        .expect("the set existed at transaction start");
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "profile_set",
            entity_id: name.clone(),
            before: Some(before),
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    get_set_inner(&state, &name, warning).await
}

/// The post-commit read shared by the mutating set handlers (the response
/// is served from the committed state, not from in-transaction guesses).
/// `warning` was computed in-transaction (the rule: the response is
/// the attention getter; the audit row is the record).
async fn get_set_inner(
    state: &AppState,
    name: &str,
    warning: Option<String>,
) -> Result<Json<AdminSet>, ApiError> {
    let set = registry::set_by_name(&state.db, name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such set"))?;
    let profiles = registry::set_members_ordered(&state.db, name)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(AdminSet {
        name: set.name,
        description: set.description,
        profiles: profiles.into_iter().map(AdminProfile::from).collect(),
        warning,
    }))
}

pub async fn delete_set(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(name): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let before = set_state_json(&txn, &name)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such set"))?;
    if registry::default_set_name(&txn)
        .await
        .map_err(db::map_db_err)?
        .as_deref()
        == Some(name.as_str())
    {
        // The default marker is registry-level truth: leaving it pointing
        // at a deleted set would break every default-fallback resolution.
        // Fail the delete; pointing the marker elsewhere first is the
        // explicit path.
        return Err(ApiError::conflict(
            "set is the default set; PUT /admin/default to another set first",
        ));
    }
    profile_set::Entity::delete_by_id(name.clone())
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "profile_set",
            entity_id: name.clone(),
            before: Some(before),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(Deleted {
        deleted: true,
        warning: Some(
            "members and allowed-set grants were removed with the set; \
             tagma-side agents still bound to it dangle until rebound \
             (the proxy does not track per-agent binds)"
                .to_owned(),
        ),
    }))
}

// -- parking ---------------------------------------------------------------

/// New profiles enter the registry as parking drafts (`parked = true`, not
/// client-visible on the distribution face until promoted). Promotion is a
/// plain PUT with `parked: false`.
pub async fn create_parked(
    State(state): State<AppState>,
    _admin: AdminToken,
    Json(body): Json<ProfileCreate>,
) -> Result<Json<AdminProfile>, ApiError> {
    validate_profile_id(&body.profile_id)?;
    validate_family(&body.family)?;
    if body.model.is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    validate_modalities(&body.modalities)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    if registry::profile_by_id(&txn, &body.profile_id)
        .await
        .map_err(db::map_db_err)?
        .is_some()
    {
        return Err(ApiError::conflict("profile already exists"));
    }
    let active = profile::ActiveModel {
        profile_id: Set(body.profile_id.clone()),
        family: Set(body.family.clone()),
        model: Set(body.model.clone()),
        max_context_window: Set(body.max_context_window),
        effort: Set(body.effort.clone()),
        modalities: Set(encode_modalities(&body.modalities)),
        // New profiles park: serving is an explicit promotion (the PUT).
        parked: Set(true),
        max_budget: Set(body.max_budget),
        tpm_limit: Set(body.tpm_limit),
        rpm_limit: Set(body.rpm_limit),
    };
    let inserted = active.insert(&txn).await.map_err(db::map_db_err)?;
    let after = serde_json::to_value(AdminProfile::from(inserted.clone()))
        .expect("AdminProfile always serializes");
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "profile",
            entity_id: body.profile_id.clone(),
            before: None,
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(AdminProfile::from(inserted)))
}

pub async fn list_parked(
    State(state): State<AppState>,
    _admin: AdminToken,
) -> Result<Json<Vec<AdminProfile>>, ApiError> {
    let parked = registry::parked_profiles(&state.db)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(parked.into_iter().map(AdminProfile::from).collect()))
}

/// The parking detail route serves parked drafts only: an unparked profile
/// is not in this resource (404). PUT and DELETE still operate on it --
/// they are how a served profile gets edited, re-parked, or removed.
pub async fn get_parked(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(profile_id): Path<String>,
) -> Result<Json<AdminProfile>, ApiError> {
    let p = registry::profile_by_id(&state.db, &profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such profile"))?;
    if !p.parked {
        return Err(ApiError::not_found("profile is not parked"));
    }
    Ok(Json(AdminProfile::from(p)))
}

pub async fn update_parked(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(profile_id): Path<String>,
    Json(body): Json<ProfilePut>,
) -> Result<Json<AdminProfile>, ApiError> {
    validate_family(&body.family)?;
    if body.model.is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    validate_modalities(&body.modalities)?;
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let current = registry::profile_by_id(&txn, &profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such profile"))?;
    let before =
        serde_json::to_value(AdminProfile::from(current)).expect("AdminProfile always serializes");
    let updated = profile::ActiveModel {
        profile_id: Set(profile_id.clone()),
        family: Set(body.family.clone()),
        model: Set(body.model.clone()),
        max_context_window: Set(body.max_context_window),
        effort: Set(body.effort.clone()),
        modalities: Set(encode_modalities(&body.modalities)),
        parked: Set(body.parked),
        max_budget: Set(body.max_budget),
        tpm_limit: Set(body.tpm_limit),
        rpm_limit: Set(body.rpm_limit),
    }
    .update(&txn)
    .await
    .map_err(db::map_db_err)?;
    let after = serde_json::to_value(AdminProfile::from(updated.clone()))
        .expect("AdminProfile always serializes");
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "update",
            entity: "profile",
            entity_id: profile_id.clone(),
            before: Some(before),
            after: Some(after),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(AdminProfile::from(updated)))
}

/// Deleting a profile cascades three facts: the profile row, its upstream
/// credential (via the FK cascade), and its set memberships (also via the
/// cascade). All three are audited: the profile as the primary entry, the
/// credential as its own masked row when one existed, and one row per
/// affected set with the set's before/after member order (entity
/// `set_member`) -- the membership rows vanish, so this is their trace.
pub async fn delete_parked(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(profile_id): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let current = registry::profile_by_id(&txn, &profile_id)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no such profile"))?;
    let before =
        serde_json::to_value(AdminProfile::from(current)).expect("AdminProfile always serializes");
    let credential = upstream_credential::Entity::find_by_id(&profile_id)
        .one(&txn)
        .await
        .map_err(db::map_db_err)?;

    // The membership rows die with the profile (FK cascade): capture each
    // affected set's before-state first, while the members are still there.
    let affected_sets = set_member::Entity::find()
        .filter(set_member::Column::ProfileId.eq(profile_id.clone()))
        .order_by_asc(set_member::Column::SetName)
        .all(&txn)
        .await
        .map_err(db::map_db_err)?;
    let mut member_befores = Vec::with_capacity(affected_sets.len());
    for m in &affected_sets {
        if let Some(state) = set_state_json(&txn, &m.set_name)
            .await
            .map_err(db::map_db_err)?
        {
            member_befores.push((m.set_name.clone(), state));
        }
    }
    // Removing a default-set member shifts (or empties) the
    // default-path head; warn instead of guarding (the degradation is
    // defined behavior on the forwarding face).
    let in_default = registry::default_set_name(&txn)
        .await
        .map_err(db::map_db_err)?
        .is_some_and(|default| affected_sets.iter().any(|m| m.set_name == default));
    profile::Entity::delete_by_id(profile_id.clone())
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "profile",
            entity_id: profile_id.clone(),
            before: Some(before),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    if let Some(cred) = credential {
        let view = secret_view(&profile_id, &cred.upstream_base_url, &cred.upstream_api_key);
        management_event::record(
            &txn,
            ManagementEventRow {
                action: "delete",
                entity: "upstream_credential",
                entity_id: profile_id.clone(),
                before: Some(masked_credential_json(&view)),
                after: None,
            },
        )
        .await
        .map_err(db::map_db_err)?;
    }

    // One row per affected set: the member order the profile held before
    // the cascade, and the order left behind. Failover position is exactly
    // the fact nothing else records.
    for (set_name, before) in member_befores {
        let after = set_state_json(&txn, &set_name)
            .await
            .map_err(db::map_db_err)?
            .expect("the set outlives its members within this transaction");
        management_event::record(
            &txn,
            ManagementEventRow {
                action: "update",
                entity: "set_member",
                entity_id: set_name,
                before: Some(before),
                after: Some(after),
            },
        )
        .await
        .map_err(db::map_db_err)?;
    }
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(Deleted {
        deleted: true,
        warning: in_default.then(|| {
            "profile was a member of the default set; the default-path ".to_owned()
                + "head changed (404 if the set is now empty)"
        }),
    }))
}

// -- upstream credentials (the secret store) ------------------------------

/// Upsert the credential for a profile. The plaintext is accepted here and
/// never comes back: the response and the audit row carry the mask only.
pub async fn put_secret(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(profile_id): Path<String>,
    Json(body): Json<SecretPut>,
) -> Result<Json<SecretView>, ApiError> {
    if body.upstream_api_key.is_empty() {
        return Err(ApiError::bad_request("upstream_api_key must not be empty"));
    }
    if reqwest::Url::parse(&body.upstream_base_url).is_err() {
        return Err(ApiError::bad_request(
            "upstream_base_url must be an absolute URL",
        ));
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    if registry::profile_by_id(&txn, &profile_id)
        .await
        .map_err(db::map_db_err)?
        .is_none()
    {
        return Err(ApiError::not_found("no such profile"));
    }
    let existing = upstream_credential::Entity::find_by_id(&profile_id)
        .one(&txn)
        .await
        .map_err(db::map_db_err)?;
    let before = existing
        .as_ref()
        .map(|c| secret_view(&profile_id, &c.upstream_base_url, &c.upstream_api_key))
        .map(|v| masked_credential_json(&v));
    let active = upstream_credential::ActiveModel {
        profile_id: Set(profile_id.clone()),
        upstream_base_url: Set(body.upstream_base_url.clone()),
        upstream_api_key: Set(body.upstream_api_key.clone()),
    };
    match existing {
        Some(_) => {
            active.update(&txn).await.map_err(db::map_db_err)?;
        }
        None => {
            active.insert(&txn).await.map_err(db::map_db_err)?;
        }
    }
    let view = secret_view(&profile_id, &body.upstream_base_url, &body.upstream_api_key);
    management_event::record(
        &txn,
        ManagementEventRow {
            action: if before.is_some() { "update" } else { "create" },
            entity: "upstream_credential",
            entity_id: profile_id.clone(),
            before,
            after: Some(masked_credential_json(&view)),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(view))
}

pub async fn delete_secret(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(profile_id): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let existing = upstream_credential::Entity::find_by_id(&profile_id)
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no credential for this profile"))?;
    let view = secret_view(
        &profile_id,
        &existing.upstream_base_url,
        &existing.upstream_api_key,
    );
    upstream_credential::Entity::delete_by_id(profile_id.clone())
        .exec(&txn)
        .await
        .map_err(db::map_db_err)?;
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "upstream_credential",
            entity_id: profile_id.clone(),
            before: Some(masked_credential_json(&view)),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(Deleted {
        deleted: true,
        warning: None,
    }))
}

pub async fn list_secrets(
    State(state): State<AppState>,
    _admin: AdminToken,
) -> Result<Json<SecretViews>, ApiError> {
    let rows = upstream_credential::Entity::find()
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    Ok(Json(SecretViews {
        secrets: rows
            .into_iter()
            .map(|r| secret_view(&r.profile_id, &r.upstream_base_url, &r.upstream_api_key))
            .collect(),
    }))
}

// -- the default marker ---------------------------------------------------

/// PUT /admin/default: point the registry-level default marker at an
/// existing set. The marker is shared by every tagma (decision 8), so this
/// write changes global forwarding fallback behavior immediately.
pub async fn put_default(
    State(state): State<AppState>,
    _admin: AdminToken,
    Json(body): Json<DefaultPut>,
) -> Result<Json<DefaultView>, ApiError> {
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    if registry::set_by_name(&txn, &body.default_set)
        .await
        .map_err(db::map_db_err)?
        .is_none()
    {
        return Err(ApiError::not_found("no such set"));
    }
    let existing = registry::default_set_name(&txn)
        .await
        .map_err(db::map_db_err)?;
    let before = existing
        .as_ref()
        .map(|v| serde_json::json!({"key": DEFAULT_SET_KEY, "value": v}));
    let active = registry_meta::ActiveModel {
        key: Set(DEFAULT_SET_KEY.to_owned()),
        value: Set(body.default_set.clone()),
    };
    match existing {
        Some(_) => {
            active.update(&txn).await.map_err(db::map_db_err)?;
        }
        None => {
            active.insert(&txn).await.map_err(db::map_db_err)?;
        }
    }
    management_event::record(
        &txn,
        ManagementEventRow {
            action: if before.is_some() { "update" } else { "create" },
            entity: "registry_meta",
            entity_id: DEFAULT_SET_KEY.to_owned(),
            before,
            after: Some(serde_json::json!({
                "key": DEFAULT_SET_KEY,
                "value": body.default_set,
            })),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(DefaultView {
        default_set: body.default_set,
    }))
}

/// The management face's symmetric read (the distribution face's
/// GET /default behind the admin credential).
pub async fn get_default(
    State(state): State<AppState>,
    _admin: AdminToken,
) -> Result<Json<DefaultView>, ApiError> {
    let name = registry::default_set_name(&state.db)
        .await
        .map_err(db::map_db_err)?
        .ok_or_else(|| ApiError::not_found("no default set configured"))?;
    Ok(Json(DefaultView { default_set: name }))
}

// -- proxy keys ------------------------------------------------------------

/// POST /admin/keys body: the owning tagma, the allowed sets, and an
/// optional expiry (RFC 3339).
#[derive(Deserialize)]
pub struct KeyCreate {
    pub tagma_id: String,
    pub allowed_sets: Vec<String>,
    /// `None` = never expires.
    /// Parsed as RFC 3339; missing or null means never.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

/// The admin view of one proxy key: metadata only. `key_id` is the hex
/// of the stored hash -- not presentable material (a hash never
/// authenticates anything) and the only identifier the store keeps.
/// There is no masked preview by design: the plaintext exists only in
/// the mint response, so a mask would have to be stored, which is
/// exactly the key-derived material the hash-only contract forbids.
#[derive(Clone, Debug, Serialize)]
pub struct AdminKey {
    pub key_id: String,
    pub tagma_id: String,
    pub allowed_sets: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

/// The mint response: the key view plus the plaintext bearer, shown once.
#[derive(Serialize)]
pub struct KeyIssued {
    #[serde(flatten)]
    pub key: AdminKey,
    pub token: String,
}

#[derive(Serialize)]
pub struct KeyList {
    pub keys: Vec<AdminKey>,
}

/// Mint a proxy key. The plaintext comes back exactly once, in this
/// response; only the hash is stored. Every write -- the key row, its
/// grants, the `issued` lifecycle row, and the management audit row --
/// lands in one transaction: an audit failure rolls the mint back
/// (fail-closed; a key without its trail is not a key).
pub async fn create_key(
    State(state): State<AppState>,
    _admin: AdminToken,
    Json(body): Json<KeyCreate>,
) -> Result<Json<KeyIssued>, ApiError> {
    if body.allowed_sets.is_empty() {
        return Err(ApiError::bad_request("allowed_sets must not be empty"));
    }
    let mut seen = std::collections::HashSet::new();
    for name in &body.allowed_sets {
        if !seen.insert(name) {
            return Err(ApiError::bad_request(format!(
                "duplicate set {name:?} in allowed_sets"
            )));
        }
    }
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    if tagma::Entity::find_by_id(&body.tagma_id)
        .one(&txn)
        .await
        .map_err(db::map_db_err)?
        .is_none()
    {
        return Err(ApiError::not_found(format!(
            "no such tagma: {}",
            body.tagma_id
        )));
    }
    for name in &body.allowed_sets {
        if registry::set_by_name(&txn, name)
            .await
            .map_err(db::map_db_err)?
            .is_none()
        {
            return Err(ApiError::not_found(format!("no such set: {name}")));
        }
    }
    if let Some(expires_at) = body.expires_at
        && expires_at <= OffsetDateTime::now_utc()
    {
        return Err(ApiError::bad_request("expires_at must be in the future"));
    }
    let minted = secret::mint_key(&txn, &body.tagma_id, &body.allowed_sets, body.expires_at)
        .await
        .map_err(db::map_db_err)?;
    let key_id = hex::encode(&minted.key_hash);
    record_lifecycle_event(&txn, &minted.key_hash, &body.tagma_id, "issued", None)
        .await
        .map_err(db::map_db_err)?;
    let view = AdminKey {
        key_id: key_id.clone(),
        tagma_id: body.tagma_id.clone(),
        allowed_sets: body.allowed_sets.clone(),
        created_at: minted.created_at,
        expires_at: body.expires_at,
    };
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "create",
            entity: "proxy_key",
            entity_id: key_id,
            before: None,
            after: Some(serde_json::to_value(&view).expect("AdminKey always serializes")),
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(KeyIssued {
        key: view,
        token: minted.token.secret().to_owned(),
    }))
}

/// GET /admin/keys: every key, metadata only (see [`AdminKey`] for why
/// no key material appears here). Issuance order.
pub async fn list_keys(
    State(state): State<AppState>,
    _admin: AdminToken,
) -> Result<Json<KeyList>, ApiError> {
    let keys = proxy_key::Entity::find()
        .order_by_asc(proxy_key::Column::CreatedAt)
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let grants = proxy_key_set::Entity::find()
        .all(&state.db)
        .await
        .map_err(db::map_db_err)?;
    let mut by_key: std::collections::BTreeMap<Vec<u8>, Vec<String>> =
        std::collections::BTreeMap::new();
    for g in grants {
        by_key.entry(g.key_hash).or_default().push(g.set_name);
    }
    Ok(Json(KeyList {
        keys: keys
            .into_iter()
            .map(|k| AdminKey {
                allowed_sets: by_key.remove(&k.key_hash).unwrap_or_default(),
                key_id: hex::encode(&k.key_hash),
                tagma_id: k.tagma_id,
                created_at: k.created_at,
                expires_at: k.expires_at,
            })
            .collect(),
    }))
}

/// DELETE /admin/keys/{key_id}: revoke. `key_id` is the hex hash from
/// the list; anything malformed is "no such key" (the distinction never
/// matters to a caller). The delete, the `revoked` lifecycle row, and
/// the management audit row commit together; the grants cascade away
/// with the row, and the bearer gets the dedicated `key_revoked` code
/// from the next request on.
pub async fn revoke_key(
    State(state): State<AppState>,
    _admin: AdminToken,
    Path(key_id): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let Ok(hash) = hex::decode(&key_id) else {
        return Err(ApiError::not_found("no such key"));
    };
    let txn = state.db.begin().await.map_err(db::map_db_err)?;
    let Some(revoked) = secret::revoke_key(&txn, &hash)
        .await
        .map_err(db::map_db_err)?
    else {
        return Err(ApiError::not_found("no such key"));
    };
    record_lifecycle_event(&txn, &hash, &revoked.tagma_id, "revoked", None)
        .await
        .map_err(db::map_db_err)?;
    let before = serde_json::to_value(AdminKey {
        key_id: hex::encode(&hash),
        tagma_id: revoked.tagma_id.clone(),
        allowed_sets: revoked.allowed_sets.clone(),
        created_at: revoked.created_at,
        expires_at: revoked.expires_at,
    })
    .expect("AdminKey always serializes");
    management_event::record(
        &txn,
        ManagementEventRow {
            action: "delete",
            entity: "proxy_key",
            entity_id: key_id,
            before: Some(before),
            after: None,
        },
    )
    .await
    .map_err(db::map_db_err)?;
    txn.commit().await.map_err(db::map_db_err)?;
    // The write landed: drop every cached resolution so the next
    // distribution request sees the new authorization matrix.
    state.key_cache.clear();
    Ok(Json(Deleted {
        deleted: true,
        warning: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::QuotaLedger;
    use crate::test_support::{
        TEST_BEARER, TEST_MANAGEMENT_TOKEN, UPSTREAM_KEY, migrated_test_db, seed_registry,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sea_orm::Statement;
    use tower::ServiceExt;

    fn app(state: AppState) -> axum::Router {
        crate::routes::management_plane_router(state, "")
    }

    /// The data-plane router over the same state: for the assertions that
    /// prove a management write is visible to the distribution face --
    /// the cross-face visibility the physical split must not break.
    fn data_app(state: &AppState) -> axum::Router {
        crate::routes::data_plane_router(state.clone(), "")
    }

    /// A management-face request: bearer + optional JSON body.
    fn req(
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

    async fn seeded_state() -> AppState {
        let db = migrated_test_db().await;
        seed_registry(&db, "https://api.upstream.test").await;
        AppState {
            db,
            public_base_url: "http://gw.test:7501".to_owned(),
            quota: std::sync::Arc::new(QuotaLedger::new()),
            management: crate::test_support::test_management(),
            key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
        }
    }

    async fn send(app: axum::Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
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

    async fn audit_rows(
        db: &sea_orm::DatabaseConnection,
    ) -> Vec<crate::audit::management_event::Model> {
        crate::audit::management_event::Entity::find()
            .all(db)
            .await
            .expect("audit rows read")
    }

    // -- pure validation (local legs, no container) -----------------------

    #[test]
    fn validate_family_rejects_non_wire_families() {
        assert!(validate_family("deepseek").is_ok());
        assert!(validate_family("openai-compatible").is_ok());
        assert!(validate_family("anthropic").is_err());
        assert!(validate_family("").is_err());
    }

    #[test]
    fn set_name_validation_blocks_empty_and_separators() {
        assert!(validate_set_name("gamma").is_ok());
        assert!(validate_set_name("").is_err());
        assert!(validate_set_name("a/b").is_err());
    }

    #[test]
    fn profile_id_validation_blocks_empty_and_separators() {
        assert!(validate_profile_id("gamma").is_ok());
        assert!(validate_profile_id("").is_err());
        assert!(validate_profile_id("a/b").is_err());
    }

    #[test]
    fn mask_key_shows_head_and_tail_only() {
        let masked = mask_key("sk-plaintext-value-123");
        assert!(masked.contains("***"));
        assert!(!masked.contains("plaintext"));
    }

    // -- authentication: the family separation pin -------------------------

    #[tokio::test]
    async fn management_face_rejects_every_wrong_credential() {
        let state = seeded_state().await;
        // No credential at all.
        let (status, _) = send(app(state.clone()), req("GET", "/admin/sets", None, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // The wrong management token.
        let (status, _) = send(
            app(state.clone()),
            req("GET", "/admin/sets", Some("wrong-management-token"), None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // A valid proxy key is still the wrong family: the two credential
        // families share nothing (no compound judgment can cross them).
        let (status, body) = send(
            app(state),
            req("GET", "/admin/sets", Some(TEST_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["message"], "invalid management token");
    }

    #[tokio::test]
    async fn unconfigured_management_face_is_closed_not_absent() {
        let mut state = seeded_state().await;
        state.management = std::sync::Arc::new(crate::management::Disabled);
        let (status, body) = send(
            app(state),
            req("GET", "/admin/sets", Some(TEST_MANAGEMENT_TOKEN), None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("not configured")
        );
    }

    // -- sets -------------------------------------------------------------

    #[tokio::test]
    async fn set_crud_round_trip_preserves_order_and_audits() {
        let state = seeded_state().await;
        // Create.
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/sets",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"name": "gamma", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["name"], "gamma");
        // Duplicate name conflicts.
        let (status, _) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/sets",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"name": "gamma", "description": "x"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // Members: two profiles in a chosen order.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/sets/gamma",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"members": ["p4", "p1"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["profile_id"], "p4");
        assert_eq!(body["profiles"][1]["profile_id"], "p1");
        // Reorder: the failover order is editable and read back verbatim.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/sets/gamma",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"members": ["p1", "p4"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["profile_id"], "p1");
        // Description-only update leaves members alone.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/sets/gamma",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"description": "new text"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["description"], "new text");
        assert_eq!(body["profiles"][0]["profile_id"], "p1");
        // Audit trail: one create + three updates, entity profile_set.
        let rows = audit_rows(&state.db).await;
        let gamma: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "profile_set" && r.entity_id == "gamma")
            .collect();
        assert_eq!(gamma.len(), 4);
        assert_eq!(gamma[0].action, "create");
        assert!(gamma[0].before.is_none());
        assert_eq!(gamma.iter().filter(|r| r.action == "update").count(), 3);
        assert!(rows.iter().all(|r| r.actor == "management-token"));
    }

    #[tokio::test]
    async fn set_member_edits_reject_unknown_and_duplicate_ids() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/sets/alpha",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"members": ["p1", "ghost"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/sets/alpha",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"members": ["p1", "p1"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // The failed writes rolled back: alpha still has exactly p1, p2.
        let (status, body) = send(
            app(state),
            req(
                "GET",
                "/admin/sets/alpha",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["profiles"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn set_delete_guards_default_and_warns_then_audits() {
        let state = seeded_state().await;
        // alpha is the seeded default: the delete must refuse.
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/sets/alpha",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        // Point the default elsewhere, then delete.
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/default",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"default_set": "beta"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/sets/alpha",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], true);
        // The dangling-bind risk hint rides the response.
        assert!(body["warning"].as_str().expect("warning").contains("bind"));
        // The set is gone from both faces.
        let (status, _) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/sets/alpha",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = send(
            data_app(&state),
            req("GET", "/sets", Some(TEST_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sets"], serde_json::json!([]));
        // The delete landed an audit row with the full before-state.
        let rows = audit_rows(&state.db).await;
        let del = rows
            .iter()
            .find(|r| r.action == "delete" && r.entity_id == "alpha")
            .expect("delete audited");
        assert_eq!(del.entity, "profile_set");
        let before = del.before.as_ref().expect("before captured");
        assert_eq!(before["members"], serde_json::json!(["p1", "p2"]));
        assert!(del.after.is_none());
    }

    // -- parking -----------------------------------------------------------

    #[tokio::test]
    async fn parking_crud_promotes_and_audits() {
        let state = seeded_state().await;
        let draft = serde_json::json!({
            "profile_id": "p9",
            "family": "openai-compatible",
            "model": "m9",
            "modalities": ["text"],
            "max_context_window": 8192
        });
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/parking",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(draft),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["parked"], true);
        // Duplicate id conflicts; path-unsafe ids, unknown family, and
        // bad modality entries all reject.
        for (payload, expected) in [
            (
                serde_json::json!({"profile_id": "p9", "family": "openai-compatible", "model": "x"}),
                StatusCode::CONFLICT,
            ),
            (
                serde_json::json!({"profile_id": "p10", "family": "anthropic", "model": "x"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!({"profile_id": "p10", "family": "deepseek", "model": "x", "modalities": [""]}),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!({"profile_id": "a/b", "family": "openai-compatible", "model": "x"}),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let (status, _) = send(
                app(state.clone()),
                req(
                    "POST",
                    "/admin/parking",
                    Some(TEST_MANAGEMENT_TOKEN),
                    Some(payload),
                ),
            )
            .await;
            assert_eq!(status, expected);
        }
        // Promote: parked=false through the same PUT.
        let promote = serde_json::json!({
            "profile_id": "p9",
            "family": "openai-compatible",
            "model": "m9",
            "modalities": ["text"],
            "max_context_window": 8192,
            "parked": false,
            "max_budget": null,
            "tpm_limit": null,
            "rpm_limit": null
        });
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/parking/p9",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(promote),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["parked"], false);
        // The promoted profile left the parking resource on both faces.
        let (status, _) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/parking/p9",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = send(
            data_app(&state),
            req("GET", "/parking", Some(TEST_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let models = body["profiles"].as_array().expect("profiles array");
        assert_eq!(models.len(), 1); // only the seeded p4 draft remains
        // The distribution payload is the sanitized shape (no profile_id;
        // the model name identifies the draft).
        assert_eq!(models[0]["model"], "draft-x");
        // The audit trail now matches the name: one create (before=None,
        // after parked) and one promote (parked true -> false), per field.
        let rows = audit_rows(&state.db).await;
        let p9_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "profile" && r.entity_id == "p9")
            .collect();
        assert_eq!(p9_rows.len(), 2);
        assert_eq!(p9_rows[0].action, "create");
        assert!(p9_rows[0].before.is_none());
        assert_eq!(
            p9_rows[0].after.as_ref().expect("create after")["parked"],
            true
        );
        assert_eq!(p9_rows[1].action, "update");
        assert_eq!(
            p9_rows[1].before.as_ref().expect("promote before")["parked"],
            true
        );
        assert_eq!(
            p9_rows[1].after.as_ref().expect("promote after")["parked"],
            false
        );
    }

    // -- secrets ------------------------------------------------------------

    #[tokio::test]
    async fn secret_crud_never_returns_the_plaintext() {
        let state = seeded_state().await;
        let secret_body = serde_json::json!({
            "upstream_base_url": "https://api.upstream.test",
            "upstream_api_key": "sk-plaintext-abcdef123456"
        });
        // Write (create path).
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/secrets/p4",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(secret_body.clone()),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profile_id"], "p4");
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
                "/admin/secrets/p4",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(secret_body),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The list is masked: the plaintext appears nowhere in the body.
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/admin/secrets", Some(TEST_MANAGEMENT_TOKEN), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = body.to_string();
        assert!(!text.contains("sk-plaintext-abcdef123456"));
        assert!(text.contains("p4"));
        // The store itself holds the plaintext (injection depends on it).
        let stored = upstream_credential::Entity::find_by_id("p4")
            .one(&state.db)
            .await
            .expect("credential read")
            .expect("credential stored");
        assert_eq!(stored.upstream_api_key, "sk-plaintext-abcdef123456");
        // The audit rows carry the mask, never the plaintext.
        let rows = audit_rows(&state.db).await;
        let secret_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.entity == "upstream_credential" && r.entity_id == "p4")
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
                "/admin/secrets/p4",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            app(state),
            req(
                "DELETE",
                "/admin/secrets/p4",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn secret_write_requires_an_existing_profile() {
        let state = seeded_state().await;
        let (status, _) = send(
            app(state),
            req(
                "PUT",
                "/admin/secrets/ghost",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({
                    "upstream_base_url": "https://api.upstream.test",
                    "upstream_api_key": "sk-orphan"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A multi-byte upstream key rides every masked exit without panicking:
    /// store, list, overwrite, plain delete, and the delete-parked cascade.
    /// Byte 3 and byte len-3 of the key both fall inside a character -- the
    /// exact shape that panicked the old byte-slice mask.
    #[tokio::test]
    async fn multibyte_key_masks_across_every_exit() {
        let state = seeded_state().await;
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
                "/admin/secrets/p4",
                Some(TEST_MANAGEMENT_TOKEN),
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
            req("GET", "/admin/secrets", Some(TEST_MANAGEMENT_TOKEN), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!listed.to_string().contains(key));
        // Overwrite: the update path masks the old value for its before row.
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/secrets/p4",
                Some(TEST_MANAGEMENT_TOKEN),
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
                "/admin/secrets/p2",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(p2_body),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/secrets/p2",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The cascade exit: p4 joins beta, then the parked delete masks the
        // credential snapshot inside the cascade; the delete succeeds
        // with the stored value in place.
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/sets/beta",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"members": ["p4"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, deleted) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/parking/p4",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
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
        assert_eq!(member_rows[0].entity_id, "beta");
        assert_eq!(
            member_rows[0].before.as_ref().expect("before")["members"],
            serde_json::json!(["p4"])
        );
        assert_eq!(
            member_rows[0].after.as_ref().expect("after")["members"],
            serde_json::json!([])
        );
    }

    // -- the default marker --------------------------------------------------

    #[tokio::test]
    async fn default_marker_is_hot_and_audited() {
        let state = seeded_state().await;
        // The seeded marker reads symmetrically on the admin face.
        let (status, body) = send(
            app(state.clone()),
            req("GET", "/admin/default", Some(TEST_MANAGEMENT_TOKEN), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["default_set"], "alpha");
        // Rewrite; a missing set refuses.
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/default",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"default_set": "beta"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/default",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"default_set": "ghost"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Hot-reload: the distribution face serves the new marker on the
        // next request, with no restart.
        let (status, body) = send(
            data_app(&state),
            req("GET", "/default", Some(TEST_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["default_set"], "beta");
        // Audit: one update row with the old and new values.
        let rows = audit_rows(&state.db).await;
        let meta = rows
            .iter()
            .find(|r| r.entity == "registry_meta")
            .expect("marker audited");
        assert_eq!(meta.action, "update");
        assert_eq!(meta.before.as_ref().expect("before")["value"], "alpha");
        assert_eq!(meta.after.as_ref().expect("after")["value"], "beta");
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
                "/admin/sets",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"name": "gamma", "description": "d"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // No set row survived the failed audit.
        let (status, _) = send(
            app(state),
            req(
                "GET",
                "/admin/sets/gamma",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    // -- parking delete: the cascade and its audit -----------------------

    /// Deleting a parked draft removes it from both faces and lands one
    /// profile delete row; deleting a served profile additionally lands
    /// the cascading credential's own masked delete row and one row per
    /// affected set with its before/after member order (the membership
    /// edge leaves via the FK cascade, but the audit trail keeps it).
    #[tokio::test]
    async fn delete_parked_audits_the_cascade() {
        let state = seeded_state().await;
        // The seeded draft p4 has no credential: one audit row only.
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/parking/p4",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/parking/p4",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // p1 is served, member of alpha, and holds a credential: the
        // delete cascades all three facts.
        let (status, _) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/parking/p1",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The set lost the member...
        let (status, body) = send(
            app(state.clone()),
            req(
                "GET",
                "/admin/sets/alpha",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let members = body["profiles"].as_array().expect("profiles array");
        assert_eq!(members.len(), 1); // only p2 remains
        assert_eq!(members[0]["profile_id"], "p2");
        // ...and the distribution face answers 404 for the deleted id.
        let (status, _) = send(
            data_app(&state),
            req("GET", "/profiles/p1", Some(TEST_BEARER), None),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Audit: exactly two delete rows for p1 -- the profile and its
        // cascading credential (masked); the plaintext is in neither.
        let rows = audit_rows(&state.db).await;
        let p1_deletes: Vec<_> = rows
            .iter()
            .filter(|r| r.entity_id == "p1" && r.action == "delete")
            .collect();
        assert_eq!(p1_deletes.len(), 2);
        let profile_row = p1_deletes
            .iter()
            .find(|r| r.entity == "profile")
            .expect("profile delete audited");
        assert!(profile_row.before.is_some());
        assert!(profile_row.after.is_none());
        let cred_row = p1_deletes
            .iter()
            .find(|r| r.entity == "upstream_credential")
            .expect("credential delete audited");
        let before = cred_row.before.as_ref().expect("before captured");
        assert!(
            before["api_key_masked"]
                .as_str()
                .expect("mask")
                .contains("***")
        );
        let raw = format!("{:?}", p1_deletes);
        assert!(!raw.contains(UPSTREAM_KEY));

        // The membership edge is audited per set: alpha's order with p1
        // at the head before, p2 alone after.
        let member_rows: Vec<_> = rows.iter().filter(|r| r.entity == "set_member").collect();
        assert_eq!(member_rows.len(), 1);
        assert_eq!(member_rows[0].entity_id, "alpha");
        assert_eq!(member_rows[0].action, "update");
        assert_eq!(
            member_rows[0].before.as_ref().expect("before")["members"],
            serde_json::json!(["p1", "p2"])
        );
        assert_eq!(
            member_rows[0].after.as_ref().expect("after")["members"],
            serde_json::json!(["p2"])
        );
    }

    // -- proxy keys: mint, list, revoke ---------------------------------

    /// The full mint loop: POST returns the plaintext exactly once (the
    /// `sk-proxy-` family), the stored identifier is the hex hash of that
    /// plaintext, the key resolves on the forwarding face immediately,
    /// and both audit rows -- lifecycle `issued` and the management
    /// `proxy_key` row -- landed in the same commit.
    #[tokio::test]
    async fn mint_round_trip_resolves_and_audits() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/keys",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({
                    "tagma_id": "tagma-under-test",
                    "allowed_sets": ["alpha"],
                    "expires_at": "2030-01-01T00:00:00Z"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let token = body["token"].as_str().expect("token in mint response");
        assert!(token.starts_with("sk-proxy-"));
        let key_id = body["key_id"].as_str().expect("key_id");
        assert_eq!(
            key_id,
            hex::encode(kallip_common::authtoken::TokenHash::of(token).as_bytes())
        );
        let resolved = crate::secret::resolve(&state.db, &state.key_cache, token)
            .await
            .expect("resolve");
        let crate::secret::Resolution::Resolved(resolved) = resolved else {
            panic!("minted key resolves");
        };
        assert_eq!(resolved.allowed_sets, vec!["alpha".to_owned()]);
        let events = crate::audit::key_lifecycle_event::Entity::find()
            .all(&state.db)
            .await
            .expect("lifecycle rows");
        assert_eq!(events.len(), 1, "seeded keys never write lifecycle rows");
        assert_eq!(events[0].event, "issued");
        assert_eq!(
            events[0].key_hash,
            kallip_common::authtoken::TokenHash::of(token)
                .as_bytes()
                .to_vec()
        );
        let rows = audit_rows(&state.db).await;
        let row = rows
            .iter()
            .find(|r| r.entity == "proxy_key")
            .expect("proxy_key audit row");
        assert_eq!(row.action, "create");
        assert_eq!(row.entity_id, key_id);
        assert_eq!(row.actor, "management-token");
    }

    /// The list is metadata only: no token field anywhere (the plaintext
    /// existed only in the mint response), and the seeded key's identifier
    /// is the hex of its bearer's hash.
    #[tokio::test]
    async fn list_keys_is_metadata_only() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state),
            req("GET", "/admin/keys", Some(TEST_MANAGEMENT_TOKEN), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.to_string().contains("token"), "{body}");
        let keys = body["keys"].as_array().expect("keys array");
        assert_eq!(keys.len(), 2, "the two seeded keys");
        let main_id = hex::encode(kallip_common::authtoken::TokenHash::of(TEST_BEARER).as_bytes());
        let main = keys
            .iter()
            .find(|k| k["key_id"] == main_id.as_str())
            .expect("seeded key listed");
        assert_eq!(main["tagma_id"], "tagma-under-test");
        assert_eq!(main["allowed_sets"], serde_json::json!(["alpha"]));
        assert!(main["expires_at"].is_null());
        assert!(!main["created_at"].is_null());
    }

    /// Input validation on POST: the whole matrix fails with the right
    /// status and no key row is written.
    #[tokio::test]
    async fn mint_rejects_bad_input() {
        let state = seeded_state().await;
        let cases = [
            (
                serde_json::json!({"tagma_id": "tagma-under-test", "allowed_sets": []}),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!({"tagma_id": "tagma-under-test", "allowed_sets": ["alpha", "alpha"]}),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!({"tagma_id": "no-such-tagma", "allowed_sets": ["alpha"]}),
                StatusCode::NOT_FOUND,
            ),
            (
                serde_json::json!({"tagma_id": "tagma-under-test", "allowed_sets": ["no-such-set"]}),
                StatusCode::NOT_FOUND,
            ),
            (
                serde_json::json!({"tagma_id": "tagma-under-test", "allowed_sets": ["alpha"], "expires_at": "2020-01-01T00:00:00Z"}),
                StatusCode::BAD_REQUEST,
            ),
        ];
        for (payload, expected) in cases {
            let (status, body) = send(
                app(state.clone()),
                req(
                    "POST",
                    "/admin/keys",
                    Some(TEST_MANAGEMENT_TOKEN),
                    Some(payload),
                ),
            )
            .await;
            assert_eq!(status, expected, "{body}");
        }
        let keys = proxy_key::Entity::find()
            .all(&state.db)
            .await
            .expect("keys");
        assert_eq!(keys.len(), 2, "no key survived a rejected mint");
    }

    /// Revoke: the bearer dies at the resolution face (the forwarding
    /// 401), the `revoked` lifecycle row and the management delete row
    /// land, the grants cascade, and unknown or malformed ids are 404.
    #[tokio::test]
    async fn revoke_kills_resolution_and_audits() {
        let state = seeded_state().await;
        let key_id = hex::encode(kallip_common::authtoken::TokenHash::of(TEST_BEARER).as_bytes());
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                &format!("/admin/keys/{key_id}"),
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], true);
        let resolved = crate::secret::resolve(&state.db, &state.key_cache, TEST_BEARER)
            .await
            .expect("resolve");
        assert!(matches!(resolved, crate::secret::Resolution::Revoked));
        let events = crate::audit::key_lifecycle_event::Entity::find()
            .all(&state.db)
            .await
            .expect("lifecycle rows");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "revoked");
        let rows = audit_rows(&state.db).await;
        let row = rows
            .iter()
            .find(|r| r.entity == "proxy_key")
            .expect("proxy_key audit row");
        assert_eq!(row.action, "delete");
        assert_eq!(row.entity_id, key_id);
        assert_eq!(
            row.before.as_ref().expect("before")["allowed_sets"],
            serde_json::json!(["alpha"])
        );
        let grants = proxy_key_set::Entity::find()
            .all(&state.db)
            .await
            .expect("grants");
        assert!(grants.is_empty(), "the seeded grant died with the key");
        for bad in [key_id.as_str(), "not-hex-at-all", &"ab".repeat(31)] {
            let (status, _) = send(
                app(state.clone()),
                req(
                    "DELETE",
                    &format!("/admin/keys/{bad}"),
                    Some(TEST_MANAGEMENT_TOKEN),
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{bad}");
        }
    }

    /// Rotation is the delete+mint pair: the trail holds revoked then
    /// issued for the replacement, and only the new bearer resolves.
    /// A revoke whose row is already gone (the concurrent-revoke
    /// interleaving: the other request's delete won) answers 404 and
    /// writes no second `revoked` lifecycle row -- deleting zero rows
    /// is never reported as a second success.
    #[tokio::test]
    async fn double_revoke_of_a_gone_row_is_a_clean_404() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/keys",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({
                    "tagma_id": "tagma-under-test",
                    "allowed_sets": ["alpha"]
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let key_id = body["key_id"].as_str().expect("key_id").to_owned();

        let (status, _) = send(
            app(state.clone()),
            req(
                "DELETE",
                &format!("/admin/keys/{key_id}"),
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // The interleaving's loser: the row is gone under this request.
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                &format!("/admin/keys/{key_id}"),
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // Exactly one `revoked` audit row, not two.
        let events = crate::audit::key_lifecycle_event::Entity::find()
            .filter(crate::audit::key_lifecycle_event::Column::Event.eq("revoked"))
            .all(&state.db)
            .await
            .expect("lifecycle rows");
        assert_eq!(events.len(), 1, "no duplicate revoke audit");
    }

    #[tokio::test]
    async fn rotation_is_delete_then_mint() {
        let state = seeded_state().await;
        let (status, issued) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/keys",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({
                    "tagma_id": "tagma-under-test",
                    "allowed_sets": ["beta"]
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        let old_token = issued["token"].as_str().expect("token").to_owned();
        let old_id = issued["key_id"].as_str().expect("key_id").to_owned();
        let (status, _) = send(
            app(state.clone()),
            req(
                "DELETE",
                &format!("/admin/keys/{old_id}"),
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, fresh) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/keys",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({
                    "tagma_id": "tagma-under-test",
                    "allowed_sets": ["beta"]
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{fresh}");
        let new_token = fresh["token"].as_str().expect("token").to_owned();
        assert!(matches!(
            crate::secret::resolve(&state.db, &state.key_cache, &old_token)
                .await
                .expect("resolve"),
            crate::secret::Resolution::Revoked
        ));
        assert!(matches!(
            crate::secret::resolve(&state.db, &state.key_cache, &new_token)
                .await
                .expect("resolve"),
            crate::secret::Resolution::Resolved(_)
        ));
        let events = crate::audit::key_lifecycle_event::Entity::find()
            .order_by_asc(crate::audit::key_lifecycle_event::Column::Id)
            .all(&state.db)
            .await
            .expect("rows");
        let summary: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(summary, ["issued", "revoked", "issued"]);
    }

    /// Fail-closed: the lifecycle row and the key row commit together.
    /// If the lifecycle write cannot land, the whole mint rolls back.
    #[tokio::test]
    async fn mint_is_fail_closed_on_lifecycle_write() {
        let state = seeded_state().await;
        state
            .db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "DROP TABLE key_lifecycle_events".to_owned(),
            ))
            .await
            .expect("table dropped");
        let (status, _) = send(
            app(state.clone()),
            req(
                "POST",
                "/admin/keys",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({
                    "tagma_id": "tagma-under-test",
                    "allowed_sets": ["alpha"]
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let keys = proxy_key::Entity::find()
            .all(&state.db)
            .await
            .expect("keys");
        assert_eq!(keys.len(), 2, "no key row survived the failed audit");
    }

    /// Draining the default set is allowed (the forwarding face's
    /// default path degrades to 404, which is defined) but the response
    /// warns; any other set drains silently.
    #[tokio::test]
    async fn emptying_the_default_set_warns() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/sets/alpha",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"members": []})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["warning"]
                .as_str()
                .expect("warning")
                .contains("default set")
        );
        let (status, body) = send(
            app(state.clone()),
            req(
                "PUT",
                "/admin/sets/beta",
                Some(TEST_MANAGEMENT_TOKEN),
                Some(serde_json::json!({"members": []})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["warning"].is_null());
    }

    /// Removing a default-set member shifts (or empties) the
    /// default-path head; the delete response warns.
    #[tokio::test]
    async fn deleting_a_default_set_member_warns() {
        let state = seeded_state().await;
        let (status, body) = send(
            app(state.clone()),
            req(
                "DELETE",
                "/admin/parking/p1",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["warning"]
                .as_str()
                .expect("warning")
                .contains("default set")
        );
        let (status, body) = send(
            app(state),
            req(
                "DELETE",
                "/admin/parking/p3",
                Some(TEST_MANAGEMENT_TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["warning"].is_null());
    }
}
