//! Team enrollment + key distribution.
//!
//! `POST /v1/teams` redeems a single-use enrollment code (bound to a user) for
//! a long-lived team token, pinning the herald's device public key. The herald
//! must sign the enrollment transcript with the matching private key (proof of
//! possession), so a stolen code alone cannot pin an attacker-chosen key. The
//! code is consumed atomically under the registry write lock, so a concurrent
//! redeem race is rejected (first wins).
//!
//! `GET /v1/teams/{id}` serves the pinned key to the owning user (TOFU with
//! change-detection on the app side).

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control::{EnrollRequest, EnrollResponse};
use kallip_agora_common::ids::TeamId;
use kallip_agora_common::proof::{ProofError, verify_enroll_proof};
use kallip_common::authtoken::{MintedToken, TokenHash};
use kallip_common::protocol::ApiError;
use serde::Serialize;
use tracing::warn;

use crate::auth::{AuthPrincipal, require_user};
use crate::state::{EnrollmentCode, SharedState, TeamRecord};
use crate::token::TEAM;

/// Expected length of an Ed25519 public key, enforced at the enrollment
/// boundary (the wire newtype carries bytes without a length check).
const ED25519_PUBLIC_KEY_LEN: usize = 32;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/teams", post(enroll))
        .route("/teams/{id}", get(get_team))
}

async fn enroll(
    State(state): State<SharedState>,
    Json(req): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, ApiError> {
    if req.device_public_key.0.len() != ED25519_PUBLIC_KEY_LEN {
        return Err(ApiError::bad_request(
            "device public key must be 32 bytes (Ed25519)",
        ));
    }

    let code_hash = TokenHash::of(&req.code);

    // Cheap gate: reject unknown/dead codes (one HashMap get under a read lock)
    // before burning a signature verify. Keeps the verify step lock-free below.
    {
        let reg = state.read()?;
        require_live_code(
            reg.enrollment_codes.get(&code_hash),
            std::time::Instant::now(),
        )?;
    }

    // Proof of possession: the signature must verify against the key being
    // pinned. Done before consuming the code so a bad-proof attempt does not
    // burn it. Pure CPU, outside the registry lock.
    verify_enroll_proof(&req.device_public_key.0, &req.code, &req.signature.0)
        .map_err(proof_to_bad_request)?;

    let team_id = TeamId::random();
    let team_token = MintedToken::generate(TEAM);
    let team_token_plaintext = team_token.secret().to_string();

    let mut reg = state.write()?;
    // Re-check under the write lock: the code may have been consumed/expired in
    // the window between the read gate and here. Consume race-free (any
    // concurrent enroll blocks on this write lock).
    let now = std::time::Instant::now();
    reg.gc_enrollment_codes(now);
    let owner = match reg.enrollment_codes.get(&code_hash) {
        Some(c) if !c.is_dead(now) => c.user.clone(),
        Some(c) => {
            warn!("enrollment code redeemed while dead (expired or already used)");
            return Err(dead_code_error(c));
        }
        None => return Err(ApiError::unauthorized("invalid enrollment code")),
    };
    if let Some(c) = reg.enrollment_codes.get_mut(&code_hash) {
        c.consumed = true;
    }

    reg.teams.insert(
        team_id.clone(),
        TeamRecord {
            owner,
            pinned_public_key: req.device_public_key,
        },
    );
    reg.team_tokens
        .insert(team_token.hash().clone(), team_id.clone());

    Ok(Json(EnrollResponse {
        team_id,
        team_token: team_token_plaintext,
    }))
}

/// A rejected enroll proof is a client error (malformed or invalid signature).
fn proof_to_bad_request(e: ProofError) -> ApiError {
    ApiError::bad_request(format!("invalid enrollment proof: {e}"))
}

/// Reject an unknown/dead enrollment code with the right status. Shared by the
/// cheap read-lock gate and the consume path. An unknown code is an invalid
/// credential (401); a recognized-but-dead code is a state conflict (409).
fn require_live_code(
    code: Option<&EnrollmentCode>,
    now: std::time::Instant,
) -> Result<(), ApiError> {
    match code {
        Some(c) if !c.is_dead(now) => Ok(()),
        Some(c) => Err(dead_code_error(c)),
        None => Err(ApiError::unauthorized("invalid enrollment code")),
    }
}

/// 409 for a recognized-but-dead code, with a message distinguishing consumed
/// from expired. `consumed` is checked first so a code that is both (redeemed,
/// then also past its TTL) reports "already used" - redemption is the
/// actionable cause for the caller, not the clock.
fn dead_code_error(c: &EnrollmentCode) -> ApiError {
    let msg = if c.consumed {
        "enrollment code already used"
    } else {
        "enrollment code expired"
    };
    ApiError::conflict(msg)
}

#[derive(Serialize)]
struct TeamInfo {
    team_id: String,
    pinned_public_key: Ed25519PublicKey,
}

async fn get_team(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
) -> Result<Json<TeamInfo>, ApiError> {
    let user = require_user(&principal)?;
    let team_id = TeamId::from(id);
    let reg = state.read()?;
    let team = reg
        .teams
        .get(&team_id)
        .ok_or_else(|| ApiError::not_found("unknown team"))?;
    // Existence-oracle hardening: a non-owner gets the same 404 as for an
    // unknown team, so they cannot confirm whether a guessed team id exists.
    if team.owner != *user {
        return Err(ApiError::not_found("unknown team"));
    }
    Ok(Json(TeamInfo {
        team_id: team_id.to_string(),
        pinned_public_key: team.pinned_public_key.clone(),
    }))
}
