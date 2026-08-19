//! Token-budget methods for [`TagmaClient`].
//!
//! Get, adjust, and set the tagma-wide token budget. Split from
//! `client.rs` verbatim; the client core stays in the parent module.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallip_common::protocol::{TokenBudgetResponse, TokenBudgetUpdateRequest};

impl TagmaClient {
    /// Get the tagma-wide token budget status.
    pub async fn get_token_budget(&self) -> Result<TokenBudgetResponse> {
        self.handle_response(
            self.with_auth(self.inner.http.get(self.url("/budget")))
                .send()
                .await
                .context("failed to get token budget")?,
            "failed to parse budget response",
        )
        .await
    }

    /// Adjust the tagma-wide token budget by a signed delta.
    ///
    /// Positive delta increases, negative delta decreases.
    pub async fn adjust_token_budget(&self, delta: i64) -> Result<TokenBudgetResponse> {
        self.handle_response(
            self.with_auth(self.inner.http.post(self.url("/budget")).json(
                &TokenBudgetUpdateRequest {
                    set_remaining: None,
                    delta: Some(delta),
                },
            ))
            .send()
            .await
            .context("failed to adjust token budget")?,
            "failed to parse budget response",
        )
        .await
    }

    /// Set the remaining tagma-wide token budget.
    ///
    /// The tagma computes `new_total = consumed + value`. Use `value == 0`
    /// to pause all agents (remaining = 0 triggers immediate budget exceeded).
    pub async fn set_token_budget(&self, value: u64) -> Result<TokenBudgetResponse> {
        self.handle_response(
            self.with_auth(self.inner.http.post(self.url("/budget")).json(
                &TokenBudgetUpdateRequest {
                    set_remaining: Some(value),
                    delta: None,
                },
            ))
            .send()
            .await
            .context("failed to set token budget")?,
            "failed to parse budget response",
        )
        .await
    }
}
