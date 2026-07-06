//! Token budget API: query and adjust the daemon-wide token consumption limit.

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use kallip_common::protocol::{ApiError, TokenBudgetResponse, TokenBudgetUpdateRequest};
use kallip_common::tokens::format_tokens_m;

use crate::state::SharedState;

/// GET /budget — return daemon-wide token budget status.
///
/// Any authenticated identity may read the budget.
pub async fn get_budget(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
) -> impl IntoResponse {
    let snap = state.token_budget.snapshot();
    Json(TokenBudgetResponse {
        budget: snap.budget,
        consumed: snap.consumed,
        remaining: snap.remaining(),
    })
}

/// POST /budget — adjust or set the daemon-wide token budget.
///
/// Operator only — affects all agents.
///
/// Accepts either `set_remaining` (remaining budget — daemon computes total = consumed + value)
/// or `delta` (signed adjustment to total budget), but not both.
pub async fn update_budget(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<TokenBudgetUpdateRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Operator only.
    crate::auth::require_operator(auth.identity())?;

    // Validate: exactly one of set_remaining/delta must be provided.
    match (req.set_remaining, req.delta) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "cannot specify both 'set_remaining' and 'delta'",
            ));
        }
        (None, None) => {
            return Err(ApiError::bad_request(
                "must specify either 'set_remaining' or 'delta'",
            ));
        }
        _ => {}
    }

    // Compute and apply the new budget value.
    if let Some(value) = req.set_remaining {
        // Set remaining budget: new total = consumed + value.
        // Intentionally allows remaining == 0, which is the pause-all-agents
        // mechanism.
        state.token_budget.set_remaining(value);
    } else {
        // Delta adjustment — CAS loop ensures delta applies to the actual current value.
        let delta = req.delta.unwrap();
        if delta == 0 {
            return Err(ApiError::bad_request("delta must be non-zero"));
        }

        state
            .token_budget
            .adjust_delta(delta)
            .map_err(|attempted| {
                ApiError::conflict(format!(
                    "new budget ({}) would be at or below tokens already consumed ({})",
                    format_tokens_m(attempted),
                    format_tokens_m(state.token_budget.consumed()),
                ))
            })?;
    }

    // Use a single snapshot so budget/consumed/remaining are consistent.
    let snap = state.token_budget.snapshot();

    Ok(Json(TokenBudgetResponse {
        budget: snap.budget,
        consumed: snap.consumed,
        remaining: snap.remaining(),
    }))
}
