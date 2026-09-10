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
                    set_unlimited: false,
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
                    set_unlimited: false,
                },
            ))
            .send()
            .await
            .context("failed to set token budget")?,
            "failed to parse budget response",
        )
        .await
    }

    /// Switch the tagma to an unlimited token budget: enforcement off,
    /// consumption still tracked. On servers predating the unlimited wire
    /// field this gets the 400 must-specify error back; the CLI maps that
    /// to an unsupported-server message instead of a usage error.
    pub async fn set_token_budget_unlimited(&self) -> Result<TokenBudgetResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url("/budget"))
                    .json(&unlimited_request()),
            )
            .send()
            .await
            .context("failed to set token budget unlimited")?,
            "failed to parse budget response",
        )
        .await
    }
}

/// The request body `set_token_budget_unlimited` sends. Extracted from
/// the method so the wire shape has a direct test.
fn unlimited_request() -> TokenBudgetUpdateRequest {
    TokenBudgetUpdateRequest {
        set_remaining: None,
        delta: None,
        set_unlimited: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unlimited request body carries `set_unlimited` and neither of
    /// the other two write shapes — the exactly-one contract the server
    /// 400s on when violated.
    #[test]
    fn unlimited_request_body_carries_only_set_unlimited() {
        let body = serde_json::to_value(unlimited_request()).unwrap();
        assert_eq!(body, serde_json::json!({ "set_unlimited": true }));
    }
}
