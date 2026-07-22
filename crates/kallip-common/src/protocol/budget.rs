//! Token budget wire types.

use serde::{Deserialize, Serialize};

/// Default token budget when none is specified (100M tokens).
pub const DEFAULT_TOKEN_BUDGET: u64 = 100_000_000;

/// Request body for POST /budget.
///
/// Exactly one of `set_remaining` or `delta` must be provided.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudgetUpdateRequest {
    /// Set remaining budget to this value. The tagma computes the new total
    /// as `consumed + value`. Mutually exclusive with `delta`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "set_remaining"
    )]
    pub set_remaining: Option<u64>,
    /// Adjust total budget by this signed delta. Mutually exclusive with `set_remaining`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<i64>,
}

/// Response body for GET/POST /budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudgetResponse {
    /// Current budget (total token limit).
    pub budget: u64,
    /// Cumulative tokens consumed so far.
    pub consumed: u64,
    /// Remaining tokens before budget exhaustion.
    pub remaining: u64,
}

impl TokenBudgetResponse {
    /// Format this response as a human-readable status string.
    ///
    /// Output example: `"budget: 100.0M  consumed: 23.5M  remaining: 76.5M"`
    pub fn format_display(&self) -> String {
        format!(
            "budget: {}  consumed: {}  remaining: {}",
            crate::tokens::format_tokens_m(self.budget),
            crate::tokens::format_tokens_m(self.consumed),
            crate::tokens::format_tokens_m(self.remaining),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_budget_response_format_display() {
        let resp = TokenBudgetResponse {
            budget: 100_000_000,
            consumed: 23_500_000,
            remaining: 76_500_000,
        };
        assert_eq!(
            resp.format_display(),
            "budget: 100.0M  consumed: 23.5M  remaining: 76.5M",
        );
    }
}
