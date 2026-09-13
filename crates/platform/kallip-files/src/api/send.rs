//! POST /{id}/send: server-side delivery (the send semantics of
//! delivery, not access). The caller needs read
//! right on the source; the target is validated against the enrollment
//! facts; the landing write happens as the service, never through a path
//! grant, so the ACL stays a zero-exception-channel model.
//!
//! The four principal x target combinations (all pinned in tests):
//! - U -> U: lands in `/users/{U2}/inbox/` (matrix row 9's delivery face)
//! - U -> T: lands in `/users/{U}/tagmas/{T2}/inbox/` (eighth default)
//! - T -> T: same space only; lands in the target's private `inbox/`
//! - T -> U: refused -- the shared region is the channel (ninth default)
//!
//! The write is one transaction: new record (same blob, refcount + 1,
//! zero data copy), provenance stamped, and the delivery event appended
//! together, so a delivery is never visible without its audit trail.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::acl::{self, Action};
use crate::auth::AuthPrincipal;
use crate::state::AppState;
use kallip_archeion_common::principal::Principal;
use kallip_common::protocol::ApiError;

#[derive(Debug, Deserialize)]
pub struct SendRequest {
    /// Deliver to this user's inbox (mutually exclusive with `to_tagma`).
    pub to_user: Option<String>,
    /// Deliver to this tagma's inbox (mutually exclusive with `to_user`).
    pub to_tagma: Option<String>,
}

/// The response body of a successful delivery.
#[derive(Debug, Serialize)]
pub struct SendResponse {
    /// The newly created record (the recipient's own copy).
    pub record_id: Uuid,
    /// Same blob as the source: zero data copied.
    pub blob_id: String,
    /// Where the copy landed in the recipient's space.
    pub path: String,
}

/// POST /{id}/send
pub async fn send_file(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    AuthPrincipal(principal): AuthPrincipal,
    axum::Json(request): axum::Json<SendRequest>,
) -> Result<Response, ApiError> {
    let target = match (request.to_user, request.to_tagma) {
        (Some(user), None) => Target::User(user),
        (None, Some(tagma)) => Target::Tagma(tagma),
        _ => {
            return Err(ApiError::bad_request(
                "specify exactly one of to_user, to_tagma",
            ));
        }
    };

    // The source read is an ordinary ACL read. Admin has no content face
    // (row 8) and therefore cannot send either.
    let record = super::load_record(&state, id).await?;
    let source_path = super::parse_record_path(&record.space_path)?;
    let facts = match &principal {
        Principal::User(user) => {
            if !acl::user_can(user.as_ref(), &source_path, Action::Read) {
                return Err(ApiError::forbidden("no read right on the source record"));
            }
            None
        }
        Principal::Tagma(tagma) => {
            let facts = super::tagma_facts(&state, tagma).await?;
            if !acl::tagma_can(tagma.as_ref(), &source_path, Action::Read, &facts) {
                return Err(ApiError::forbidden("no read right on the source record"));
            }
            Some(facts)
        }
        Principal::Admin => return Err(ApiError::forbidden("admin cannot send content")),
    };

    let filename = record
        .space_path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ApiError::internal("record path has no filename"))?
        .to_owned();

    let landing = resolve_landing(&state, &principal, facts.as_ref(), &target, &filename).await?;

    let blob_id = kallip_blob_store::BlobId::parse(&record.blob_id).map_err(ApiError::internal)?;
    let size = stat_or_drift(&state, &blob_id).await?;
    let (record_id, _event_id) = crate::metadata::repo::register_delivery(
        &state.db,
        id,
        &blob_id,
        size,
        &landing.path,
        &landing.owner,
        &landing.provenance,
        &landing.from,
        &landing.to,
    )
    .await
    .map_err(ApiError::internal)?;

    // Best-effort event push (fire-and-forget): the delivery itself is
    // committed; a failed push only logs, because the file is safely in
    // the recipient's space and discoverable via the listing surface.
    if let Some(notify) = &state.notify {
        let notify = notify.clone();
        let to_user = landing.owner.clone();
        let path = landing.path.clone();
        let from = landing.from.clone();
        let name = filename.clone();
        tokio::spawn(async move {
            notify
                .file_delivered(&to_user, record_id, &path, &from, &name, size as u64)
                .await;
        });
    }
    Ok((
        StatusCode::CREATED,
        axum::Json(SendResponse {
            record_id,
            blob_id: record.blob_id,
            path: landing.path,
        }),
    )
        .into_response())
}

enum Target {
    User(String),
    Tagma(String),
}

/// The resolved destination of a delivery: where the record lands, who owns
/// the landing space, and the provenance/event principal strings.
struct Landing {
    path: String,
    owner: String,
    provenance: String,
    from: String,
    to: String,
}

/// Resolve the landing for one of the four combinations. Every denial here
/// is a 400/403 by design: the caller could always have asked first.
async fn resolve_landing(
    state: &AppState,
    principal: &Principal,
    facts: Option<&acl::EnrollmentFacts>,
    target: &Target,
    filename: &str,
) -> Result<Landing, ApiError> {
    match (principal, target) {
        (Principal::User(user), Target::User(to_user)) => {
            if user.as_ref() == to_user.as_str() {
                // A user already owns their space; sending to themselves is
                // a caller mistake, not a delivery.
                return Err(ApiError::bad_request("cannot send to yourself"));
            }
            // The recipient must parse and exist: an arbitrary string would
            // mint a record no principal can read or delete, pinning its
            // blob against garbage collection forever.
            let target = parse_user(to_user)?;
            match state
                .control
                .user_identities(std::slice::from_ref(&target))
                .await
            {
                Ok(found) if !found.is_empty() => {}
                Ok(_) => return Err(ApiError::not_found("target user does not exist")),
                // Same posture as the enrollment lookup: soft degrades the
                // decision to a denial, closed surfaces the outage.
                Err(_e) if state.config.degrade_fail_soft => {
                    return Err(ApiError::not_found("target user does not exist"));
                }
                Err(e) => {
                    return Err(ApiError::unavailable(format!("registry unavailable: {e}")));
                }
            }
            Ok(Landing {
                path: format!("/users/{to_user}/inbox/{filename}"),
                owner: to_user.clone(),
                provenance: user.to_string(),
                from: user.to_string(),
                to: to_user.clone(),
            })
        }
        (Principal::User(user), Target::Tagma(to_tagma)) => {
            // The target must be enrolled in the caller's own space: the
            // user delivers into their own space's tagma region (eighth
            // default), so the target's lookup must name that space.
            let target_facts = super::tagma_facts(state, &parse_tagma(to_tagma)?).await?;
            if !acl::can_receive_delivery(&target_facts, user.as_ref(), to_tagma.as_str()) {
                return Err(ApiError::forbidden(
                    "target tagma is not enrolled in your space",
                ));
            }
            Ok(Landing {
                path: format!("/users/{user}/tagmas/{to_tagma}/inbox/{filename}"),
                owner: user.to_string(),
                provenance: user.to_string(),
                from: user.to_string(),
                to: to_tagma.clone(),
            })
        }
        (Principal::Tagma(tagma), Target::Tagma(to_tagma)) => {
            if tagma.as_ref() == to_tagma.as_str() {
                return Err(ApiError::bad_request("cannot send to yourself"));
            }
            // Same space only: the caller's own facts name the space, and
            // the target must be a member of it. Absent facts deny.
            let facts = facts.ok_or_else(|| ApiError::internal("facts missing"))?;
            if !acl::can_receive_delivery(facts, &facts.space_user, to_tagma.as_str()) {
                return Err(ApiError::forbidden(
                    "target tagma is not enrolled in your space",
                ));
            }
            Ok(Landing {
                path: format!(
                    "/users/{}/tagmas/{to_tagma}/inbox/{filename}",
                    facts.space_user
                ),
                owner: facts.space_user.clone(),
                provenance: tagma.to_string(),
                from: tagma.to_string(),
                to: to_tagma.clone(),
            })
        }
        (Principal::Tagma(_), Target::User(_)) => {
            // Ninth default: no T -> U send. The shared region already
            // reaches the user (matrix row 3); a second channel would be a
            // model extension for nothing.
            Err(ApiError::forbidden(
                "tagma-to-user send is not supported; write to the shared region instead",
            ))
        }
        (Principal::Admin, _) => Err(ApiError::forbidden("admin cannot send content")),
    }
}

fn parse_user(raw: &str) -> Result<kallip_archeion_common::ids::UserId, ApiError> {
    use std::str::FromStr as _;
    kallip_archeion_common::ids::UserId::from_str(raw)
        .map_err(|_| ApiError::bad_request("to_user must be a user id"))
}
fn parse_tagma(raw: &str) -> Result<kallip_archeion_common::ids::TagmaId, ApiError> {
    use std::str::FromStr as _;
    kallip_archeion_common::ids::TagmaId::from_str(raw)
        .map_err(|_| ApiError::bad_request("to_tagma must be a tagma id"))
}

/// Stat the source blob. `None` is catalog/store drift (the reconciler's
/// missing_blobs face): a 500, not a 404.
async fn stat_or_drift(
    state: &AppState,
    blob_id: &kallip_blob_store::BlobId,
) -> Result<i64, ApiError> {
    let info = state.blob.stat(blob_id).await.map_err(ApiError::internal)?;
    info.map(|info| info.size as i64)
        .ok_or_else(|| ApiError::internal("catalog row without a blob file"))
}
