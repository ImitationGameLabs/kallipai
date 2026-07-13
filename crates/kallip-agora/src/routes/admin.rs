//! Admin (provisioning) endpoints, authenticated by the admin token. These mint
//! user accounts and enrollment codes; they are the invite-only entry point.

use axum::Json;
use axum::extract::State;
use axum::routing::post;
use axum::{Router, routing::get};
use kallip_agora_common::ids::UserId;
use kallip_common::authtoken::MintedToken;
use kallip_common::protocol::ApiError;
use serde::{Deserialize, Serialize};

use crate::auth::{AuthPrincipal, require_admin};
use crate::state::SharedState;
use crate::token::{ENROLLMENT, USER};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/users", post(create_user))
        .route("/enrollment-codes", post(create_enrollment_code))
        // A trivial GET on the admin nest so `axum::serve` wiring is exercised
        // by the health/E2E harness without a body-bearing call.
        .route("/", get(|| async { "kallip-agora admin" }))
}

#[derive(Serialize)]
struct CreateUserResponse {
    user_id: String,
    /// `sk-user-...` — returned once; the agora retains only its hash.
    access_token: String,
}

async fn create_user(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<CreateUserResponse>, ApiError> {
    require_admin(&principal)?;
    let user_id = UserId::random();
    let token = MintedToken::generate(USER);
    let access_token = token.secret().to_string();
    let mut reg = state.write()?;
    reg.users.insert(user_id.clone());
    reg.access_tokens
        .insert(token.hash().clone(), user_id.clone());
    Ok(Json(CreateUserResponse {
        user_id: user_id.to_string(),
        access_token,
    }))
}

#[derive(Deserialize)]
struct CreateEnrollmentCodeRequest {
    user_id: String,
}

#[derive(Serialize)]
struct CreateEnrollmentCodeResponse {
    /// `sk-enroll-...` — single-use, short-TTL; returned once.
    code: String,
}

async fn create_enrollment_code(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(req): Json<CreateEnrollmentCodeRequest>,
) -> Result<Json<CreateEnrollmentCodeResponse>, ApiError> {
    require_admin(&principal)?;
    let user_id = UserId::from(req.user_id);
    let code = MintedToken::generate(ENROLLMENT);
    let plaintext = code.secret().to_string();
    let mut reg = state.write()?;
    reg.gc_enrollment_codes(std::time::Instant::now());
    if !reg.users.contains(&user_id) {
        return Err(ApiError::not_found("unknown user_id"));
    }
    reg.enrollment_codes.insert(
        code.hash().clone(),
        crate::state::EnrollmentCode {
            user: user_id,
            expires_at: std::time::Instant::now() + state.limits.enrollment_code_ttl,
            consumed: false,
        },
    );
    Ok(Json(CreateEnrollmentCodeResponse { code: plaintext }))
}
