//! Credential records over HTTP: a user's personal access tokens and a tagma's
//! single credential.
//!
//! Every route here starts with [`require_session`]: a token that could mint,
//! reset or re-scope credentials would mint its own successor, so a personal
//! access token is refused before a handler body runs. Ownership is checked against the row
//! owner (a personal token's `owner_id`, a tagma's `owner_user_id`); an id that
//! is not the caller's is a 404, never a 403, so an id probe learns nothing.
//!
//! The plaintext leaves the process exactly once, from `POST /me/tokens` and
//! from a reset, and is never stored.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use kallipai_archeion_common::ids::{TagmaId, TokenId, UserId};
use kallipai_archeion_common::perm::PermSet;
use kallipai_common::protocol::ApiError;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, TransactionTrait};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::auth::{AuthPrincipal, require_session};
use crate::credentials::{self, Owner};
use crate::db::entity::{access_tokens, credential_events, tagmata};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::state::SharedState;
use crate::token::PAT;

/// Longest accepted credential name. A label, rendered in a list row; long
/// enough for a machine and a purpose, short enough to stay one line.
const MAX_NAME_LEN: usize = 64;

/// The expiries a personal token may be created with, in days. A token with no
/// expiry is the tagma credential's shape, not a person's shortcut.
const PAT_EXPIRY_CHOICES: [i64; 3] = [30, 90, 365];

/// The default personal-token life.
const PAT_EXPIRY_DEFAULT: i64 = 90;

/// The personal-token and tagma-credential surface.
pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/me/tokens", get(list_tokens).post(create_token))
        .route("/me/tokens/{id}", patch(update_token).delete(revoke_token))
        .route(
            "/tagmata/{id}/credential",
            get(get_tagma_credential).patch(update_tagma_credential),
        )
        .route(
            "/tagmata/{id}/credential/reset",
            post(reset_tagma_credential),
        )
}

/// One credential record as a list row: the mask, never the secret.
#[derive(Debug, Serialize)]
struct TokenView {
    id: String,
    name: String,
    masked: Option<String>,
    perms: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    revoked_at: Option<OffsetDateTime>,
}

/// The row's stored permission array, verbatim.
///
/// Written only from a string array, so a shape that does not read back is a
/// corrupt row; it is logged and shown as an empty list rather than dressed up
/// as a permission string.
fn stored_perms(row: &access_tokens::Model) -> Vec<String> {
    match serde_json::from_value(row.perms.clone()) {
        Ok(perms) => perms,
        Err(err) => {
            tracing::warn!(
                token_id = %row.id,
                error = %err,
                "stored permission set is not a string array"
            );
            Vec::new()
        }
    }
}

impl TokenView {
    fn from_record(row: &access_tokens::Model) -> Self {
        // The stored array, verbatim: a string that left the vocabulary is
        // still what the record carries, and verification answers 401 for the
        // whole record. Reading the array through the vocabulary first would
        // print "carries nothing" about a record that carries something it
        // can no longer use -- the opposite of what its calls do.
        let perms = stored_perms(row);
        Self {
            id: row.id.clone(),
            name: row.name.clone(),
            masked: row.masked.clone(),
            perms,
            created_at: row.created_at,
            expires_at: row.expires_at,
            last_used_at: row.last_used_at,
            revoked_at: row.revoked_at,
        }
    }
}

/// One audit row.
#[derive(Debug, Serialize)]
struct EventView {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    kind: String,
    actor_user_id: Option<String>,
    added: Vec<String>,
    removed: Vec<String>,
}

impl EventView {
    fn from_row(row: &credential_events::Model) -> Self {
        Self {
            at: row.at,
            kind: row.kind.clone(),
            actor_user_id: row.actor_user_id.clone(),
            added: serde_json::from_value(row.added.clone()).unwrap_or_default(),
            removed: serde_json::from_value(row.removed.clone()).unwrap_or_default(),
        }
    }
}

/// A created or reset credential: the one time the plaintext is readable.
#[derive(Serialize)]
struct IssuedToken {
    token: TokenView,
    plaintext: String,
}

impl std::fmt::Debug for IssuedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the plaintext: this type reaches logs and assertion messages.
        f.debug_struct("IssuedToken")
            .field("token", &self.token)
            .field("plaintext", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct CreateTokenRequest {
    name: String,
    perms: Vec<String>,
    /// One of [`PAT_EXPIRY_CHOICES`]; absent means the default life.
    expires_in_days: Option<i64>,
}

#[derive(Deserialize)]
struct UpdateTokenRequest {
    name: Option<String>,
    perms: Option<Vec<String>>,
}

/// A tagma's credential plus its history, for the owner's card.
#[derive(Debug, Serialize)]
struct TagmaCredential {
    token: TokenView,
    events: Vec<EventView>,
}

#[derive(Deserialize)]
struct TagmaCredentialUpdate {
    name: Option<String>,
    perms: Option<Vec<String>>,
}

fn resolve_name(name: &str) -> Result<String, ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("a credential name is required"));
    }
    if trimmed.chars().count() > MAX_NAME_LEN {
        return Err(ApiError::bad_request(format!(
            "a credential name is at most {MAX_NAME_LEN} characters"
        )));
    }
    Ok(trimmed.to_string())
}

fn resolve_perms(perms: &[String]) -> Result<PermSet, ApiError> {
    PermSet::from_known(perms.iter().map(String::as_str))
        .map_err(|e| ApiError::bad_request(e.to_string()))
}

/// The caller's personal tokens, oldest first, revoked ones included: a list
/// that hid them could not show what was revoked or when.
async fn list_tokens(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
) -> Result<Json<Vec<TokenView>>, ApiError> {
    let user_id = require_session(&auth)?;
    let rows = credentials::list(&state.db, Owner::User, user_id.as_ref())
        .await?
        .into_iter();
    Ok(Json(rows.map(|row| TokenView::from_record(&row)).collect()))
}

async fn create_token(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
    Json(req): Json<CreateTokenRequest>,
) -> Result<(StatusCode, Json<IssuedToken>), ApiError> {
    let user_id = require_session(&auth)?;
    let name = resolve_name(&req.name)?;
    let perms = resolve_perms(&req.perms)?;
    // An empty set is the tagma credential's "stopped, key kept" state; a
    // personal token has no such state (revoking is how one is stopped), so a
    // person minting one picks at least one string.
    if perms.is_empty() {
        return Err(ApiError::bad_request(
            "a personal access token carries at least one permission",
        ));
    }
    let days = req.expires_in_days.unwrap_or(PAT_EXPIRY_DEFAULT);
    if !PAT_EXPIRY_CHOICES.contains(&days) {
        return Err(ApiError::bad_request(format!(
            "a personal token expires in one of {PAT_EXPIRY_CHOICES:?} days"
        )));
    }
    let expires_at = OffsetDateTime::now_utc() + time::Duration::days(days);
    // The record and its `create` event land together or not at all.
    let user_id = user_id.clone();
    let outcome = state
        .db
        .transaction::<_, credentials::Minted, TxnError>(|txn| {
            Box::pin(async move {
                credentials::create(
                    txn,
                    PAT,
                    credentials::NewCredential {
                        owner: Owner::User,
                        owner_id: user_id.as_ref(),
                        name: &name,
                        perms: &perms,
                        created_by: user_id.as_ref(),
                        actor: Some(user_id.as_ref()),
                        expires_at: Some(expires_at),
                    },
                )
                .await
                .map_err(TxnError::Api)
            })
        })
        .await;
    let minted = flatten_txn(outcome)?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedToken {
            token: TokenView::from_record(&minted.record),
            plaintext: minted.plaintext,
        }),
    ))
}

/// The caller's own personal token, or a 404 when the id belongs to someone
/// else (or to a tagma).
async fn owned_personal_token<C: ConnectionTrait>(
    conn: &C,
    user_id: &UserId,
    id: &str,
) -> Result<access_tokens::Model, ApiError> {
    let row = access_tokens::Entity::find_by_id(id.to_string())
        .one(conn)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown token"))?;
    if row.owner_kind != "user" || row.owner_id != user_id.as_ref() {
        return Err(ApiError::not_found("unknown token"));
    }
    Ok(row)
}

async fn update_token(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
    Path(id): Path<TokenId>,
    Json(req): Json<UpdateTokenRequest>,
) -> Result<Json<TokenView>, ApiError> {
    let user_id = require_session(&auth)?;
    // A rename and a rescope are one edit: the row and its `perms` event commit
    // together, so a rename cannot land without the set it was sent with.
    let user_id = user_id.clone();
    let outcome = state
        .db
        .transaction::<_, access_tokens::Model, TxnError>(|txn| {
            Box::pin(async move {
                let row = owned_personal_token(txn, &user_id, id.as_ref())
                    .await
                    .map_err(TxnError::Api)?;
                if row.revoked_at.is_some() {
                    return Err(TxnError::Api(ApiError::conflict("this token is revoked")));
                }
                if req.name.is_none() && req.perms.is_none() {
                    return Err(TxnError::Api(ApiError::bad_request("nothing to change")));
                }
                let mut row = row;
                if let Some(name) = &req.name {
                    let name = resolve_name(name).map_err(TxnError::Api)?;
                    row = credentials::rename(txn, row, &name)
                        .await
                        .map_err(TxnError::Api)?;
                }
                if let Some(perms) = &req.perms {
                    let set = resolve_perms(perms).map_err(TxnError::Api)?;
                    row = credentials::set_perms(txn, row, &set, user_id.as_ref())
                        .await
                        .map_err(TxnError::Api)?;
                }
                Ok(row)
            })
        })
        .await;
    Ok(Json(TokenView::from_record(&flatten_txn(outcome)?)))
}

async fn revoke_token(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
    Path(id): Path<TokenId>,
) -> Result<StatusCode, ApiError> {
    let user_id = require_session(&auth)?;
    let row = owned_personal_token(&state.db, user_id, id.as_ref()).await?;
    if row.revoked_at.is_none() {
        credentials::revoke(&state.db, row).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The caller's tagma, or a 404 when the id is unknown or another user's.
async fn owned_tagma<C: ConnectionTrait>(
    conn: &C,
    user_id: &UserId,
    id: &str,
) -> Result<tagmata::Model, ApiError> {
    let row = tagmata::Entity::find_by_id(id.to_string())
        .filter(tagmata::Column::OwnerUserId.eq(user_id.to_string()))
        .one(conn)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown tagma"))?;
    Ok(row)
}

async fn tagma_credential<C: ConnectionTrait>(
    conn: &C,
    tagma_id: &TagmaId,
) -> Result<access_tokens::Model, ApiError> {
    credentials::live_tagma_record(conn, tagma_id.as_ref())
        .await?
        .ok_or_else(|| ApiError::not_found("this tagma has no credential"))
}

async fn get_tagma_credential(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
    Path(id): Path<TagmaId>,
) -> Result<Json<TagmaCredential>, ApiError> {
    let user_id = require_session(&auth)?;
    owned_tagma(&state.db, user_id, id.as_ref()).await?;
    let row = tagma_credential(&state.db, &id).await?;
    let events = credentials::events(&state.db, &row.id)
        .await?
        .iter()
        .map(EventView::from_row)
        .collect();
    Ok(Json(TagmaCredential {
        token: TokenView::from_record(&row),
        events,
    }))
}

async fn update_tagma_credential(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
    Path(id): Path<TagmaId>,
    Json(req): Json<TagmaCredentialUpdate>,
) -> Result<Json<TagmaCredential>, ApiError> {
    let user_id = require_session(&auth)?;
    // Same shape as the personal-token edit: one transaction for the record
    // change and its audit row, then the trail is read back for the response.
    let user_id = user_id.clone();
    let outcome = state
        .db
        .transaction::<_, access_tokens::Model, TxnError>(|txn| {
            Box::pin(async move {
                let tagma = owned_tagma(txn, &user_id, id.as_ref())
                    .await
                    .map_err(TxnError::Api)?;
                if tagma.revoked_at.is_some() {
                    return Err(TxnError::Api(ApiError::conflict("this tagma is revoked")));
                }
                if req.name.is_none() && req.perms.is_none() {
                    return Err(TxnError::Api(ApiError::bad_request("nothing to change")));
                }
                let mut row = tagma_credential(txn, &id).await.map_err(TxnError::Api)?;
                if let Some(name) = &req.name {
                    let name = resolve_name(name).map_err(TxnError::Api)?;
                    row = credentials::rename(txn, row, &name)
                        .await
                        .map_err(TxnError::Api)?;
                }
                if let Some(perms) = &req.perms {
                    let set = resolve_perms(perms).map_err(TxnError::Api)?;
                    row = credentials::set_perms(txn, row, &set, user_id.as_ref())
                        .await
                        .map_err(TxnError::Api)?;
                }
                Ok(row)
            })
        })
        .await;
    let row = flatten_txn(outcome)?;
    let events = credentials::events(&state.db, &row.id)
        .await?
        .iter()
        .map(EventView::from_row)
        .collect();
    Ok(Json(TagmaCredential {
        token: TokenView::from_record(&row),
        events,
    }))
}

async fn reset_tagma_credential(
    State(state): State<SharedState>,
    auth: AuthPrincipal,
    Path(id): Path<TagmaId>,
) -> Result<Json<IssuedToken>, ApiError> {
    let user_id = require_session(&auth)?;
    // The new hash and its `reset` event commit together: a hash swapped
    // without the audit row would hand out a secret nothing recorded.
    let user_id = user_id.clone();
    let outcome = state
        .db
        .transaction::<_, credentials::Minted, TxnError>(|txn| {
            Box::pin(async move {
                let tagma = owned_tagma(txn, &user_id, id.as_ref())
                    .await
                    .map_err(TxnError::Api)?;
                if tagma.revoked_at.is_some() {
                    return Err(TxnError::Api(ApiError::conflict("this tagma is revoked")));
                }
                let row = tagma_credential(txn, &id).await.map_err(TxnError::Api)?;
                credentials::reset(txn, crate::token::TAGMA, row, user_id.as_ref())
                    .await
                    .map_err(TxnError::Api)
            })
        })
        .await;
    let minted = flatten_txn(outcome)?;
    Ok(Json(IssuedToken {
        token: TokenView::from_record(&minted.record),
        plaintext: minted.plaintext,
    }))
}

#[cfg(test)]
mod tests {
    //! The token routes: creation replaces the secret with a record, an edit
    //! leaves an audit row, a revoke keeps the row, and no id answers across
    //! owners.

    use axum::extract::{Path, State};

    use super::{
        CreateTokenRequest, PAT_EXPIRY_CHOICES, TagmaCredentialUpdate, UpdateTokenRequest,
        create_token, get_tagma_credential, list_tokens, reset_tagma_credential, revoke_token,
        update_tagma_credential, update_token,
    };
    use crate::auth::{AuthPrincipal, Principal};
    use crate::credentials;
    use crate::db::entity::{access_tokens, tagmata};
    use crate::test_helpers::{make_state, seed_tagma, seed_user};
    use crate::token::PAT;
    use kallipai_archeion_common::bytes::Ed25519PublicKey;
    use kallipai_archeion_common::ids::{TokenId, UserId};
    use kallipai_archeion_common::perm::{self, FILES_RECORD_READ, FILES_RECORD_WRITE};
    use kallipai_common::authtoken::mask_token;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    fn session(user: &UserId) -> AuthPrincipal {
        AuthPrincipal::unscoped(Principal::User(user.clone()))
    }

    async fn seed_pat(
        state: &crate::state::SharedState,
        user: &UserId,
        name: &str,
        perms: &[&str],
    ) -> TokenId {
        let set = perm::PermSet::from_known(perms.iter().copied()).expect("known strings");
        let minted = credentials::create(
            &state.db,
            PAT,
            credentials::NewCredential {
                owner: credentials::Owner::User,
                owner_id: user.as_ref(),
                name,
                perms: &set,
                created_by: user.as_ref(),
                actor: Some(user.as_ref()),
                expires_at: None,
            },
        )
        .await
        .expect("seed a personal token");
        TokenId::from(minted.record.id)
    }

    fn create_req(perms: &[&str], days: Option<i64>) -> CreateTokenRequest {
        CreateTokenRequest {
            name: "cli-laptop".to_string(),
            perms: perms.iter().map(|p| p.to_string()).collect(),
            expires_in_days: days,
        }
    }

    #[tokio::test]
    async fn create_then_list_shows_the_record_without_the_secret() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;

        let (status, axum::Json(issued)) = create_token(
            State(state.clone()),
            session(&user),
            axum::Json(create_req(&[FILES_RECORD_READ], None)),
        )
        .await
        .expect("create");
        assert_eq!(status, axum::http::StatusCode::CREATED);
        assert!(issued.plaintext.starts_with("sk-pat-"));
        assert_eq!(
            issued.token.masked.as_deref(),
            Some(mask_token(&issued.plaintext, PAT).as_str())
        );
        assert_eq!(issued.token.perms, vec![FILES_RECORD_READ.to_string()]);
        assert!(issued.token.expires_at.is_some());
        assert!(issued.token.revoked_at.is_none());

        // The plaintext works the moment it is returned: asking the registry
        // to resolve it yields its owner and exactly the record's set.
        let control = crate::control_plane::DbControlPlane::new(
            state.db.clone(),
            state
                .admin_token_hash
                .read()
                .expect("admin token lock")
                .clone(),
        );
        let verified = kallipai_archeion_common::control_plane::ControlPlane::verify_bearer(
            &control,
            &issued.plaintext,
        )
        .await
        .expect("verify")
        .expect("a freshly minted token is a key");
        assert!(matches!(&verified.principal, Principal::User(id) if *id == user));
        let scope = verified.credential.expect("a personal token is a record");
        assert_eq!(scope.token.as_ref(), issued.token.id.as_str());
        assert!(scope.perms.contains(FILES_RECORD_READ));
        assert!(!scope.perms.contains(FILES_RECORD_WRITE));

        let axum::Json(listed) = list_tokens(State(state), session(&user))
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, issued.token.id);
        assert_ne!(
            listed[0].masked.as_deref(),
            Some(issued.plaintext.as_str()),
            "the list carries the mask, never the secret"
        );
    }

    /// A stored string this build no longer knows stays in the row and is
    /// shown as stored: the record is refused on use, so listing it as empty
    /// would describe a credential that carries something as one that carries
    /// nothing.
    #[tokio::test]
    async fn the_list_shows_a_stored_set_verbatim_after_the_vocabulary_moves() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let id = seed_pat(&state, &user, "older release", &[FILES_RECORD_READ]).await;

        access_tokens::ActiveModel {
            id: Set(id.as_ref().to_string()),
            perms: Set(serde_json::json!([
                FILES_RECORD_READ,
                "files:records:archive"
            ])),
            ..Default::default()
        }
        .update(&state.db)
        .await
        .expect("rewrite the stored set");

        let axum::Json(listed) = list_tokens(State(state), session(&user))
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].perms,
            vec![
                FILES_RECORD_READ.to_string(),
                "files:records:archive".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn create_refuses_an_empty_permission_set() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;

        let err = create_token(
            State(state.clone()),
            session(&user),
            axum::Json(create_req(&[], None)),
        )
        .await
        .expect_err("a personal token needs at least one permission");
        assert_eq!(err.status, 400);

        // Nothing was written: the refusal happens before the record exists.
        let axum::Json(listed) = list_tokens(State(state), session(&user))
            .await
            .expect("list");
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn create_rejects_an_unknown_perm_and_an_off_menu_expiry() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;

        let err = create_token(
            State(state.clone()),
            session(&user),
            axum::Json(create_req(&["files:records:read"], None)),
        )
        .await
        .expect_err("unknown string");
        assert_eq!(err.status, 400);

        let err = create_token(
            State(state.clone()),
            session(&user),
            axum::Json(create_req(&[FILES_RECORD_READ], Some(7))),
        )
        .await
        .expect_err("off-menu expiry");
        assert_eq!(err.status, 400);

        let err = create_token(
            State(state.clone()),
            session(&user),
            axum::Json(CreateTokenRequest {
                name: "  ".to_string(),
                perms: vec![FILES_RECORD_READ.to_string()],
                expires_in_days: None,
            }),
        )
        .await
        .expect_err("blank name");
        assert_eq!(err.status, 400);

        // Every accepted choice still creates: the guard rejects the menu's
        // outside, not the form.
        for days in PAT_EXPIRY_CHOICES {
            let _ = create_token(
                State(state.clone()),
                session(&user),
                axum::Json(create_req(&[FILES_RECORD_READ], Some(days))),
            )
            .await
            .expect("an accepted expiry");
        }
    }

    #[tokio::test]
    async fn update_token_renames_rescopes_and_records_the_edit() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let id = seed_pat(&state, &user, "cli-laptop", &[FILES_RECORD_READ]).await;

        let axum::Json(view) = update_token(
            State(state.clone()),
            session(&user),
            Path(id.clone()),
            axum::Json(UpdateTokenRequest {
                name: Some("ci".to_string()),
                perms: Some(vec![FILES_RECORD_WRITE.to_string()]),
            }),
        )
        .await
        .expect("update");

        assert_eq!(view.name, "ci");
        assert_eq!(view.perms, vec![FILES_RECORD_WRITE.to_string()]);
        let trail = credentials::events(&state.db, id.as_ref())
            .await
            .expect("events");
        let last = trail.last().expect("a perms event");
        assert_eq!(last.kind, credentials::EVENT_PERMS);
        assert_eq!(last.removed, serde_json::json!([FILES_RECORD_READ]));
    }

    #[tokio::test]
    async fn revoke_is_idempotent_and_keeps_the_row() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let id = seed_pat(&state, &user, "cli-laptop", &[FILES_RECORD_READ]).await;

        for _ in 0..2 {
            revoke_token(State(state.clone()), session(&user), Path(id.clone()))
                .await
                .expect("revoke");
        }

        let axum::Json(listed) = list_tokens(State(state), session(&user))
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].revoked_at.is_some());
    }

    #[tokio::test]
    async fn another_users_token_is_a_404() {
        let state = make_state().await;
        let alice = seed_user(&state, "alice").await;
        let bob = seed_user(&state, "bob").await;
        let id = seed_pat(&state, &alice, "cli-laptop", &[FILES_RECORD_READ]).await;

        let err = update_token(
            State(state.clone()),
            session(&bob),
            Path(id.clone()),
            axum::Json(UpdateTokenRequest {
                name: Some("mine now".to_string()),
                perms: None,
            }),
        )
        .await
        .expect_err("not bob's token");
        assert_eq!(err.status, 404);

        let err = revoke_token(State(state), session(&bob), Path(id))
            .await
            .expect_err("not bob's token");
        assert_eq!(err.status, 404);
    }

    /// A personal token may not manage credentials even when it carries every
    /// string: the record, not the missing permission, bars the door -- and the
    /// refusal is a 403, which reads differently from an unrecognized token.
    #[tokio::test]
    async fn a_personal_token_cannot_manage_credentials() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let every = perm::VOCABULARY.to_vec();
        let id = seed_pat(&state, &user, "everything", &every).await;
        let scoped = AuthPrincipal::scoped(
            Principal::User(user.clone()),
            kallipai_archeion_common::control_plane::CredentialScope {
                token: id,
                perms: perm::PermSet::from_known(every).expect("known"),
            },
        );

        let err = create_token(
            State(state.clone()),
            scoped.clone(),
            axum::Json(create_req(&[FILES_RECORD_READ], None)),
        )
        .await
        .expect_err("a token cannot mint a token");
        assert_eq!(err.status, 403);
        assert!(
            err.message.contains("personal access token"),
            "{}",
            err.message
        );

        let (tagma_id, _) = seed_tagma(&state, &user, Ed25519PublicKey(vec![0u8; 32])).await;
        let err = reset_tagma_credential(State(state), scoped, Path(tagma_id))
            .await
            .expect_err("a token cannot reset a credential");
        assert_eq!(err.status, 403);
    }

    #[tokio::test]
    async fn tagma_credential_read_update_and_reset() {
        let state = make_state().await;
        let user = seed_user(&state, "alice").await;
        let (tagma_id, original) = seed_tagma(&state, &user, Ed25519PublicKey(vec![0u8; 32])).await;

        let axum::Json(view) =
            get_tagma_credential(State(state.clone()), session(&user), Path(tagma_id.clone()))
                .await
                .expect("read");
        assert_eq!(view.token.name, "enrollment");
        assert_eq!(
            view.token.perms,
            perm::tagma_defaults().to_vec(),
            "a tagma credential starts with the files defaults"
        );
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.events[0].kind, credentials::EVENT_CREATE);

        let axum::Json(view) = update_tagma_credential(
            State(state.clone()),
            session(&user),
            Path(tagma_id.clone()),
            axum::Json(TagmaCredentialUpdate {
                name: None,
                perms: Some(vec![FILES_RECORD_READ.to_string()]),
            }),
        )
        .await
        .expect("narrow");
        assert_eq!(view.token.perms, vec![FILES_RECORD_READ.to_string()]);
        assert_eq!(
            view.events.last().expect("a perms event").kind,
            credentials::EVENT_PERMS
        );

        let axum::Json(issued) =
            reset_tagma_credential(State(state.clone()), session(&user), Path(tagma_id.clone()))
                .await
                .expect("reset");
        assert_ne!(issued.plaintext, original);
        assert_eq!(
            issued.token.masked.as_deref(),
            Some(mask_token(&issued.plaintext, crate::token::TAGMA).as_str())
        );

        // Revoking the tagma closes its credential surface.
        let tagma = tagmata::Entity::find_by_id(tagma_id.to_string())
            .one(&state.db)
            .await
            .expect("read")
            .expect("the tagma exists");
        let mut am: tagmata::ActiveModel = tagma.into();
        am.revoked_at = Set(Some(time::OffsetDateTime::now_utc()));
        am.update(&state.db).await.expect("revoke the tagma");

        let err = reset_tagma_credential(State(state), session(&user), Path(tagma_id))
            .await
            .expect_err("a revoked tagma has no live credential to reset");
        assert_eq!(err.status, 409);
    }

    #[tokio::test]
    async fn another_users_tagma_credential_is_a_404() {
        let state = make_state().await;
        let alice = seed_user(&state, "alice").await;
        let bob = seed_user(&state, "bob").await;
        let (tagma_id, _) = seed_tagma(&state, &alice, Ed25519PublicKey(vec![0u8; 32])).await;

        let err = get_tagma_credential(State(state.clone()), session(&bob), Path(tagma_id.clone()))
            .await
            .map(|_| ())
            .expect_err("not bob's tagma");
        assert_eq!(err.status, 404);

        let err = reset_tagma_credential(State(state), session(&bob), Path(tagma_id))
            .await
            .map(|_| ())
            .expect_err("not bob's tagma");
        assert_eq!(err.status, 404);
    }
}
