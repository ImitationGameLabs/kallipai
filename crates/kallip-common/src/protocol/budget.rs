//! Token budget wire types.

use serde::{Deserialize, Serialize};

/// Request body for POST /budget.
///
/// Exactly one of `set_remaining`, `delta`, or `set_unlimited` must be provided.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudgetUpdateRequest {
    /// Set remaining budget to this value. The tagma computes the new total
    /// as `consumed + value`. Mutually exclusive with `delta` and `set_unlimited`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "set_remaining"
    )]
    pub set_remaining: Option<u64>,
    /// Adjust total budget by this signed delta. Mutually exclusive with `set_remaining`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<i64>,
    /// Switch the tagma to an unlimited budget (consumption stays tracked,
    /// enforcement stops). Mutually exclusive with `set_remaining` and `delta`.
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        rename = "set_unlimited"
    )]
    pub set_unlimited: bool,
}

/// Response body for GET/POST /budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudgetResponse {
    /// Current budget (total token limit); 0 when unlimited (the `unlimited`
    /// flag is authoritative).
    pub budget: u64,
    /// Cumulative tokens consumed so far.
    pub consumed: u64,
    /// Remaining tokens before budget exhaustion; 0 when unlimited.
    pub remaining: u64,
    /// Whether the budget is unlimited: no enforcement, consumption still
    /// tracked. Absent (defaults to false) from older servers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unlimited: bool,
}

impl TokenBudgetResponse {
    /// Format this response as a human-readable status string.
    ///
    /// Output examples: `"budget: 100.0M  consumed: 23.5M  remaining: 76.5M"`,
    /// or `"budget: unlimited  consumed: 23.5M"` when unlimited.
    pub fn format_display(&self) -> String {
        if self.unlimited {
            return format!(
                "budget: unlimited  consumed: {}",
                crate::tokens::format_tokens_m(self.consumed),
            );
        }
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
            unlimited: false,
        };
        assert_eq!(
            resp.format_display(),
            "budget: 100.0M  consumed: 23.5M  remaining: 76.5M",
        );
    }

    #[test]
    fn unlimited_response_format_display() {
        let resp = TokenBudgetResponse {
            budget: 0,
            consumed: 23_500_000,
            remaining: 0,
            unlimited: true,
        };
        assert_eq!(resp.format_display(), "budget: unlimited  consumed: 23.5M",);
    }
}
