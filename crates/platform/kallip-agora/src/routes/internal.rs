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
    TagmaProfileResponse, TagmaProfilesRequest, TagmaProfilesResponse, TunnelProofTsRequest,
    TunnelProofTsResponse, UserIdentitiesRequest, UserIdentitiesResponse,
    UserIdentityByUsernameRequest, UserIdentityResponse, VerifyBearerRequest, VerifyBearerResponse,
    VerifySessionRequest, VerifySessionResponse, WirePrincipal,
};
use kallip_agora_common::principal::Principal;

use crate::control_plane::DbControlPlane;
use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/verify-session", post(verify_session))
        .route("/verify-bearer", post(verify_bearer))
        .route("/tagma-profiles", post(tagma_profiles))
        .route("/user-identities", post(user_identities))
        .route(
            "/user-identity-by-username",
            post(user_identity_by_username),
        )
        .route("/tunnel-proof-ts", post(tunnel_proof_ts))
}

/// A handler error: a status code plus a body the lesche never reads (it maps
/// `404` -> `None`, any other non-2xx -> `Backend` from the status alone). All
/// bodies here are empty; diagnostics go to `tracing` (see `backend`).
type HandlerError = (StatusCode, String);

/// Map a `ControlPlane` backend failure to a 500 with an EMPTY body. The body
/// could otherwise echo a raw `DbErr` (query text, constraint names) to a
/// trusted-but-remote caller; the lesche inspects only the status, so log the
/// detail server-side and return the same empty body `NOT_FOUND` uses.
fn backend<E: std::fmt::Display>(e: E) -> HandlerError {
    tracing::error!(error = %e, "internal route failure");
    (StatusCode::INTERNAL_SERVER_ERROR, String::new())
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
        Ok(Some(session)) => Ok(axum::Json(VerifySessionResponse {
            user_id: session.user_id,
            username: session.username,
            display_name: session.display_name,
        })),
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
            return Err(backend("unexpected User principal from verify_bearer"));
        }
    };
    Ok(axum::Json(VerifyBearerResponse { principal }))
}

async fn tagma_profiles(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<TagmaProfilesRequest>,
) -> Result<axum::Json<TagmaProfilesResponse>, HandlerError> {
    let profiles = control(&state)
        .tagma_profiles(&req.tagma_ids)
        .await
        .map_err(backend)?;
    Ok(axum::Json(TagmaProfilesResponse {
        profiles: profiles
            .into_iter()
            .map(|p| TagmaProfileResponse {
                tagma_id: p.tagma_id,
                pinned_public_key: p.pinned_public_key,
                owner_user_id: p.owner_user_id,
                label: p.label,
                owner_username: p.owner_username,
                owner_display_name: p.owner_display_name,
                enrolled: p.enrolled,
                revoked: p.revoked,
                owner_disabled: p.owner_disabled,
            })
            .collect(),
    }))
}

async fn user_identities(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<UserIdentitiesRequest>,
) -> Result<axum::Json<UserIdentitiesResponse>, HandlerError> {
    let users = control(&state)
        .user_identities(&req.user_ids)
        .await
        .map_err(backend)?;
    Ok(axum::Json(UserIdentitiesResponse {
        users: users
            .into_iter()
            .map(|u| UserIdentityResponse {
                user_id: u.user_id,
                username: u.username,
                display_name: u.display_name,
                disabled: u.disabled,
            })
            .collect(),
    }))
}

async fn user_identity_by_username(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<UserIdentityByUsernameRequest>,
) -> Result<axum::Json<UserIdentityResponse>, HandlerError> {
    // Singular handle resolve: `None` (unknown / disabled / malformed) maps to
    // 404, matching verify-session / verify-bearer's None-as-404 convention --
    // not the omission convention of the bulk reader, because this is one id.
    match control(&state)
        .user_identity_by_username(&req.username)
        .await
    {
        Ok(Some(u)) => Ok(axum::Json(UserIdentityResponse {
            user_id: u.user_id,
            username: u.username,
            display_name: u.display_name,
            disabled: u.disabled,
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
