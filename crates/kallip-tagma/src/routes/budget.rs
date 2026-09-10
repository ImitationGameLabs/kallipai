//! Token budget API: query and adjust the tagma-wide token consumption limit.

use axum::Json;
use axum::extract::State;
use kallip_common::protocol::{ApiError, TokenBudgetResponse, TokenBudgetUpdateRequest};
use kallip_common::tokens::format_tokens_m;

use crate::state::SharedState;

/// GET /budget — return tagma-wide token budget status.
///
/// Any authenticated identity may read the budget.
pub async fn get_budget(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
) -> Json<TokenBudgetResponse> {
    let snap = state.token_budget.snapshot();
    Json(TokenBudgetResponse {
        budget: snap.budget,
        consumed: snap.consumed,
        remaining: snap.remaining(),
        unlimited: snap.unlimited,
    })
}

/// POST /budget — adjust or set the tagma-wide token budget.
///
/// Operator only — affects all agents.
///
/// Accepts exactly one of `set_remaining` (remaining budget — tagma computes
/// total = consumed + value), `delta` (signed adjustment to total budget), or
/// `set_unlimited` (enforcement off, consumption still tracked).
pub async fn update_budget(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<TokenBudgetUpdateRequest>,
) -> Result<Json<TokenBudgetResponse>, ApiError> {
    // Operator only.
    crate::auth::require_operator(auth.identity())?;

    // Validate: exactly one of the three write shapes must be provided.
    match (req.set_remaining, req.delta, req.set_unlimited) {
        (Some(_), Some(_), _) | (_, Some(_), true) | (Some(_), _, true) => {
            return Err(ApiError::bad_request(
                "cannot specify more than one of 'set_remaining', 'delta', 'set_unlimited'",
            ));
        }
        (None, None, false) => {
            return Err(ApiError::bad_request(
                "must specify one of 'set_remaining', 'delta', or 'set_unlimited'",
            ));
        }
        _ => {}
    }

    if req.set_unlimited {
        state.token_budget.set_unlimited();
    } else if let Some(value) = req.set_remaining {
        // Set remaining budget: new total = consumed + value.
        // Intentionally allows remaining == 0, which is the pause-all-agents
        // mechanism. From unlimited mode this is the documented exit with
        // an explicit value.
        state.token_budget.set_remaining(value);
    } else {
        // Delta adjustment. Enforcement is off in unlimited mode, so a
        // delta has nothing to adjust: the pre-check below is the
        // operator-facing message, and adjust_delta's in-lock Unlimited arm
        // is the TOCTOU backstop (a concurrent set_unlimited between here
        // and the call).
        let delta = req.delta.unwrap_or(0);
        if delta == 0 {
            return Err(ApiError::bad_request("delta must be non-zero"));
        }
        if state.token_budget.is_unlimited() {
            return Err(ApiError::conflict(
                "budget is unlimited; set a finite budget first",
            ));
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
    // A budget-limit write is a discrete mutation class: wake the snapshot pumps
    // (the ticker would cover it within 2 s, but the operator expects the
    // header to reflect the change on the next frame, not three later).
    state.invalidate();

    // Use a single snapshot so budget/consumed/remaining are consistent.
    let snap = state.token_budget.snapshot();
    Ok(Json(TokenBudgetResponse {
        budget: snap.budget,
        consumed: snap.consumed,
        remaining: snap.remaining(),
        unlimited: snap.unlimited,
    }))
}

#[cfg(test)]
mod tests {
    use axum::Json;
    use axum::extract::State;
    use kallip_common::agentid::AgentId;
    use kallip_common::protocol::TokenBudgetUpdateRequest;

    use super::{get_budget, update_budget};
    use crate::auth::{AuthIdentity, Identity};
    use crate::test_helpers::make_state;

    /// A wire body with the three write shapes addressed positionally.
    fn body(
        set_remaining: Option<u64>,
        delta: Option<i64>,
        set_unlimited: bool,
    ) -> Json<TokenBudgetUpdateRequest> {
        Json(TokenBudgetUpdateRequest {
            set_remaining,
            delta,
            set_unlimited,
        })
    }

    /// Unlimited budgets report wire zeroes for budget/remaining while
    /// consumption keeps its true accumulated value.
    #[tokio::test]
    async fn get_reports_wire_zeroes_while_unlimited() {
        let state = make_state();
        state.token_budget.set_unlimited();
        state.token_budget.record_usage(700, 300);
        let axum::Json(resp) =
            get_budget(State(state), AuthIdentity::test_new(Identity::Operator)).await;
        assert!(resp.unlimited);
        assert_eq!(resp.budget, 0);
        assert_eq!(resp.remaining, 0);
        assert_eq!(resp.consumed, 1_000);
    }

    #[tokio::test]
    async fn set_unlimited_switches_from_limited() {
        let state = make_state();
        state.token_budget.record_usage(600_000, 0);
        let axum::Json(resp) = update_budget(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            body(None, None, true),
        )
        .await
        .expect("set_unlimited succeeds");
        assert!(resp.unlimited);
        assert_eq!(resp.budget, 0);
        assert_eq!(resp.consumed, 600_000);
        assert!(state.token_budget.is_unlimited());
    }

    /// The documented exit from unlimited: an explicit set_remaining value
    /// migrates to a limited budget of consumed + value.
    #[tokio::test]
    async fn set_remaining_migrates_unlimited_to_limited() {
        let state = make_state();
        state.token_budget.set_unlimited();
        state.token_budget.record_usage(700, 0);
        let axum::Json(resp) = update_budget(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            body(Some(500), None, false),
        )
        .await
        .expect("set_remaining migrates");
        assert!(!resp.unlimited);
        assert_eq!(resp.budget, 1_200);
        assert_eq!(resp.consumed, 700);
        assert_eq!(resp.remaining, 500);
    }

    #[tokio::test]
    async fn delta_on_unlimited_conflicts() {
        let state = make_state();
        state.token_budget.set_unlimited();
        let err = update_budget(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            body(None, Some(500), false),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 409);
        assert!(err.message.contains("unlimited"));
        assert!(state.token_budget.is_unlimited());
    }

    /// Exactly one of the three write shapes: every multi-shape combination
    /// and the empty body are 400s.
    #[tokio::test]
    async fn exactly_one_shape_is_enforced() {
        let state = make_state();
        for body in [
            body(Some(500), Some(100), false),
            body(Some(500), None, true),
            body(None, Some(100), true),
            body(None, None, false),
        ] {
            let err = update_budget(
                State(state.clone()),
                AuthIdentity::test_new(Identity::Operator),
                body,
            )
            .await
            .unwrap_err();
            assert_eq!(err.status, 400);
        }
    }

    #[tokio::test]
    async fn zero_delta_is_rejected() {
        let state = make_state();
        let err = update_budget(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            body(None, Some(0), false),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[tokio::test]
    async fn delta_at_or_below_consumed_conflicts() {
        let state = make_state();
        state.token_budget.record_usage(600_000, 0);
        let err = update_budget(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            body(None, Some(-500_000), false),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 409);
        assert!(err.message.contains("consumed"));
    }

    #[tokio::test]
    async fn non_operator_is_forbidden() {
        let state = make_state();
        let err = update_budget(
            State(state),
            AuthIdentity::test_new(Identity::Agent {
                id: AgentId::random(),
            }),
            body(Some(500), None, false),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 403);
    }
}
