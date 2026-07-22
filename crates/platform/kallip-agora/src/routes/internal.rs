//! The service-to-service `/internal/*` ControlPlane HTTP surface.
//!
//! Each handler wraps the DB-backed `DbControlPlane`. `None` outcomes (unknown
//! session / token / tagma) map to HTTP `404` so the lesche's
//! `HttpControlPlane` can turn status straight into `Option::None` without
//! parsing a sentinel body. The whole nest is guarded by
//! [`crate::middleware::internal_guard`] (shared-secret bearer); handlers never
//! re-check it.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;

use kallip_agora_common::control_plane::ControlPlane;
use kallip_agora_common::internal_api::{
    TagmaIdentityRequest, TagmaIdentityResponse, TagmaResolvableRequest, TagmaResolvableResponse,
    TunnelProofTsRequest, TunnelProofTsResponse, VerifyBearerRequest, VerifyBearerResponse,
    VerifySessionRequest, VerifySessionResponse, WirePrincipal,
};
use kallip_agora_common::principal::Principal;

use crate::control_plane::DbControlPlane;
use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/verify-session", post(verify_session))
        .route("/verify-bearer", post(verify_bearer))
        .route("/tagma-resolvable", post(tagma_resolvable))
        .route("/tagma-identity", post(tagma_identity))
        .route("/tunnel-proof-ts", post(tunnel_proof_ts))
}

/// A handler error: a status + an optional diagnostic body. The lesche only
/// inspects the status (`404` -> `None`, anything non-2xx other than 404 ->
/// `Backend`); the body is for logs.
type HandlerError = (StatusCode, String);

/// Map a `ControlPlane` backend failure to a 500.
fn backend<E: std::fmt::Display>(e: E) -> HandlerError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// The "not found" outcome: `404` with an empty body.
const NOT_FOUND: HandlerError = (StatusCode::NOT_FOUND, String::new());

/// Build a per-request `DbControlPlane` (a cloned `Db` handle + the admin hash).
fn control(state: &SharedState) -> DbControlPlane {
    DbControlPlane::new(state.db.clone(), state.admin_token_hash.clone())
}

async fn verify_session(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<VerifySessionRequest>,
) -> Result<axum::Json<VerifySessionResponse>, HandlerError> {
    match control(&state).verify_session(&req.cookie).await {
        Ok(Some(user_id)) => Ok(axum::Json(VerifySessionResponse { user_id })),
        Ok(None) => Err(NOT_FOUND),
        Err(e) => Err(backend(e)),
    }
}

async fn verify_bearer(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<VerifyBearerRequest>,
) -> Result<axum::Json<VerifyBearerResponse>, HandlerError> {
    let principal = control(&state)
        .verify_bearer(&req.token)
        .await
        .map_err(backend)?;
    let Some(principal) = principal else {
        return Err(NOT_FOUND);
    };
    // `verify_bearer` can only resolve Admin or Tagma by construction (the
    // deputy guard). A `User` here is unreachable; surface it as a loud 500
    // rather than a silent 404 so a regression is not mistaken for a miss.
    let principal = match principal {
        Principal::Admin => WirePrincipal::Admin,
        Principal::Tagma(tagma_id) => WirePrincipal::Tagma { tagma_id },
        Principal::User(_) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "unexpected User principal from verify_bearer".to_string(),
            ));
        }
    };
    Ok(axum::Json(VerifyBearerResponse { principal }))
}

async fn tagma_resolvable(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<TagmaResolvableRequest>,
) -> Result<axum::Json<TagmaResolvableResponse>, HandlerError> {
    let resolvable = control(&state)
        .tagma_resolvable_by(&req.tagma_id, &req.user_id)
        .await
        .map_err(backend)?;
    Ok(axum::Json(TagmaResolvableResponse { resolvable }))
}

async fn tagma_identity(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<TagmaIdentityRequest>,
) -> Result<axum::Json<TagmaIdentityResponse>, HandlerError> {
    match control(&state).tagma_identity(&req.tagma_id).await {
        Ok(Some(id)) => Ok(axum::Json(TagmaIdentityResponse {
            pinned_public_key: id.pinned_public_key,
            owner_user_id: id.owner_user_id,
        })),
        Ok(None) => Err(NOT_FOUND),
        Err(e) => Err(backend(e)),
    }
}

async fn tunnel_proof_ts(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<TunnelProofTsRequest>,
) -> Result<axum::Json<TunnelProofTsResponse>, HandlerError> {
    let fresh = control(&state)
        .bump_tunnel_proof_ts(&req.tagma_id, req.ts)
        .await
        .map_err(backend)?;
    Ok(axum::Json(TunnelProofTsResponse { fresh }))
}
