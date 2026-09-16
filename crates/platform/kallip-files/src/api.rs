//! The HTTP API surface: route handlers and the small shared helpers every
//! handler leans on (record loading, ACL authorization with the per-request
//! enrollment facts, and the degrade posture).

pub mod admin;
pub mod get;
pub mod list;
pub mod put;
pub mod send;

use std::collections::BTreeSet;

use axum::extract::Path;
use axum::http::StatusCode;
use sea_orm::EntityTrait;
use uuid::Uuid;

use crate::acl::{self, Action, EnrollmentFacts, SpacePath};
use crate::auth::AuthPrincipal;
use crate::metadata::models::file_records;
use crate::state::AppState;
use kallip_archeion_common::ids::TagmaId;
use kallip_archeion_common::principal::Principal;
use kallip_common::protocol::ApiError;

/// GET /health: no authentication, on purpose -- compose healthcheck, the
/// Caddy probe, and baseline acceptance all need an unauthenticated liveness
/// answer (the instances' health route is the precedent).
pub async fn health() -> &'static str {
    "ok"
}

/// Load one file record, 404 when absent. The record id is a server-minted
/// UUID; nothing on the wire can address a record before the service made
/// it (the invariant: records come into being only through a real
/// upload or a delivery).
pub(crate) async fn load_record(
    state: &AppState,
    id: Uuid,
) -> Result<file_records::Model, ApiError> {
    file_records::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("no such file record"))
}

/// Parse a stored space path. A record whose path no longer parses is data
/// corruption (the parser accepted it at write time); that is a 500, never
/// a 4xx the client could act on.
pub(crate) fn parse_record_path(space_path: &str) -> Result<SpacePath, ApiError> {
    SpacePath::parse(space_path)
        .ok_or_else(|| ApiError::internal("record has a malformed space path"))
}

/// Resolve a tagma principal's enrollment facts, per request, no cache.
///
/// Outcomes:
/// - lookup resolves -> the facts;
/// - lookup returns `None` (unknown / pending / revoked / owner-disabled --
///   e.g. a revocation that raced the just-passed authentication) ->
///   empty facts, which deny every decision;
/// - lookup fails (registry unreachable): fail-closed (the default) -> 503;
///   fail-soft (`degrade_fail_soft`) -> empty facts, so tagma decisions
///   deny with 403 instead of erroring. Verification is never degraded
///   either way.
///
/// The consumer discipline on the facts: absence from
/// `enrolled` is denial; no code may unwrap or assume the requester's own
/// membership.
pub(crate) async fn tagma_facts(
    state: &AppState,
    tagma: &TagmaId,
) -> Result<EnrollmentFacts, ApiError> {
    fn deny_all() -> EnrollmentFacts {
        EnrollmentFacts {
            space_user: String::new(),
            enrolled: BTreeSet::new(),
        }
    }
    match state.control.enrollment_lookup(tagma).await {
        Ok(Some(lookup)) => Ok(EnrollmentFacts {
            space_user: lookup.user_id.to_string(),
            enrolled: lookup
                .enrolled_tagmas
                .iter()
                .map(|t| t.to_string())
                .collect(),
        }),
        Ok(None) => Ok(deny_all()),
        Err(e) => {
            if state.config.degrade_fail_soft {
                tracing::warn!(
                    error = %e,
                    tagma = %tagma,
                    "enrollment lookup failed; degrading to deny"
                );
                Ok(deny_all())
            } else {
                Err(ApiError::unavailable(format!("registry unavailable: {e}")))
            }
        }
    }
}

/// The path-level authorization used by read/write/delete routes. Rows 1-5
/// of the matrix live here; row 8 denies the admin principal every content
/// operation (its management surface is `api::admin`).
pub(crate) async fn authorize(
    state: &AppState,
    principal: &Principal,
    path: &SpacePath,
    action: Action,
) -> Result<(), ApiError> {
    let allowed = match principal {
        Principal::User(user) => acl::user_can(user.as_ref(), path, action),
        Principal::Tagma(tagma) => {
            let facts = tagma_facts(state, tagma).await?;
            acl::tagma_can(tagma.as_ref(), path, action, &facts)
        }
        Principal::Admin => acl::admin_can_content(),
    };
    if allowed {
        Ok(())
    } else {
        Err(ApiError::forbidden("not allowed on this path"))
    }
}

/// DELETE /{id}: load the record, decide by row 1-5 plus the
/// shared-region owner-field refinement, then release the reference. The
/// blob itself is never unlinked here -- a zeroed refcount only stamps
/// `freed_at`; the GC owns unlinking.
pub async fn delete_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(id): Path<Uuid>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<StatusCode, ApiError> {
    let record = load_record(&state, id).await?;
    let path = parse_record_path(&record.space_path)?;
    let allowed = match &principal {
        Principal::User(user) => acl::user_can(user.as_ref(), &path, Action::Delete),
        Principal::Tagma(tagma) => {
            let facts = tagma_facts(&state, tagma).await?;
            acl::can_delete_record(
                acl::PrincipalRef::Tagma(tagma.as_ref()),
                &path,
                &record.owner,
                &facts,
            )
        }
        Principal::Admin => return Err(ApiError::forbidden("admin cannot delete content")),
    };
    if !allowed {
        return Err(ApiError::forbidden("not allowed to delete this record"));
    }
    crate::metadata::repo::remove_record(&state.db, id)
        .await
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}
