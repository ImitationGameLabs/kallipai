//! Tagma-wide token budget shared by all agents.
//!
//! Provides [`TokenBudget`] — a wrapper around two `Arc<AtomicU64>` counters
//! (budget limit and cumulative consumption) that is cloned from `AppState`
//! so every agent on the tagma shares the same budget.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Centralized memory ordering for all token budget atomic operations.
///
/// `Relaxed` is correct here: these counters are advisory best-effort
/// checks with no synchronization invariants beyond their own values.
/// The worst case is one extra LLM round after budget exhaustion, which
/// is acceptable.
const ORDERING: Ordering = Ordering::Relaxed;

/// A point-in-time read of both token budget counters.
///
/// Because both values are read from separate atomics, there is no
/// atomicity guarantee between them — this is acceptable under the
/// `Relaxed` ordering model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudgetSnapshot {
    /// Total token budget (limit).
    pub budget: u64,
    /// Cumulative tokens consumed.
    pub consumed: u64,
}

impl TokenBudgetSnapshot {
    /// Remaining tokens before budget exhaustion.
    pub fn remaining(&self) -> u64 {
        self.budget.saturating_sub(self.consumed)
    }

    /// Whether the budget has been fully consumed.
    pub fn is_exceeded(&self) -> bool {
        self.consumed >= self.budget
    }

    /// Budget usage as a percentage (0–100).
    ///
    /// Returns 0 when budget is 0 to avoid division by zero.
    pub fn usage_pct(&self) -> u8 {
        if self.budget == 0 {
            return 0;
        }
        ((self.consumed * 100) / self.budget) as u8
    }
}

/// Tagma-wide token budget shared by all agents.
///
/// Wraps two `Arc<AtomicU64>` counters (budget limit and cumulative consumption)
/// that are cloned from `AppState` so every agent on the tagma shares the same
/// budget.
///
/// `Clone` is cheap: it clones the inner `Arc`s (reference-count increment),
/// producing a handle to the **same** underlying budget, not a copy.
#[derive(Clone)]
pub struct TokenBudget {
    budget: Arc<AtomicU64>,
    consumed: Arc<AtomicU64>,
}

impl std::fmt::Debug for TokenBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snap = self.snapshot();
        f.debug_struct("TokenBudget")
            .field("budget", &snap.budget)
            .field("consumed", &snap.consumed)
            .finish()
    }
}

impl TokenBudget {
    /// Create a fresh budget from a config limit and an initial consumption value.
    ///
    /// Used at tagma startup to initialize the tagma-wide budget
    /// (`initial_consumed = 0`).
    pub fn new(budget: u64, initial_consumed: u64) -> Self {
        Self {
            budget: Arc::new(AtomicU64::new(budget)),
            consumed: Arc::new(AtomicU64::new(initial_consumed)),
        }
    }

    // -----------------------------------------------------------------------
    // Reads
    // -----------------------------------------------------------------------

    /// Read the current budget limit.
    pub fn budget(&self) -> u64 {
        self.budget.load(ORDERING)
    }

    /// Read the current cumulative consumption.
    pub fn consumed(&self) -> u64 {
        self.consumed.load(ORDERING)
    }

    /// Remaining tokens before budget exhaustion.
    pub fn remaining(&self) -> u64 {
        self.budget().saturating_sub(self.consumed())
    }

    /// Take a point-in-time snapshot of both counters.
    pub fn snapshot(&self) -> TokenBudgetSnapshot {
        TokenBudgetSnapshot {
            budget: self.budget(),
            consumed: self.consumed(),
        }
    }

    /// Whether the budget has been fully consumed.
    pub fn is_exceeded(&self) -> bool {
        self.consumed() >= self.budget()
    }

    /// Budget usage as a percentage (0–100).
    ///
    /// Returns 0 when budget is 0 (division-by-zero guard).
    pub fn usage_pct(&self) -> u8 {
        self.snapshot().usage_pct()
    }

    // -----------------------------------------------------------------------
    // Mutations (all &self — atomic interior mutability)
    // -----------------------------------------------------------------------

    /// Record token usage from an LLM call.
    ///
    /// Atomically adds `prompt_tokens + completion_tokens` to the consumption
    /// counter. This is the only method that mutates consumption.
    pub fn record_usage(&self, prompt_tokens: u64, completion_tokens: u64) {
        let total = prompt_tokens.saturating_add(completion_tokens);
        self.consumed.fetch_add(total, ORDERING);
    }

    /// Set the total budget limit to an explicit value.
    pub fn set_limit(&self, limit: u64) {
        self.budget.store(limit, ORDERING);
    }

    /// Set remaining budget by computing `consumed + value` and storing it.
    ///
    /// Returns the new total budget value.
    /// Intentionally allows `remaining == 0` (pause mechanism: agent stops
    /// on the next budget check).
    pub fn set_remaining(&self, value: u64) -> u64 {
        let consumed = self.consumed();
        let new_budget = consumed.saturating_add(value);
        self.budget.store(new_budget, ORDERING);
        new_budget
    }

    /// Adjust the total budget by a signed delta using a CAS loop.
    ///
    /// Returns `Ok(new_budget)` on success.
    /// Returns `Err(attempted_new)` if the result would be at or below
    /// the current consumption level (budget cannot be negative).
    pub fn adjust_delta(&self, delta: i64) -> Result<u64, u64> {
        let is_positive = delta > 0;
        let abs = delta.unsigned_abs();
        let consumed = self.consumed();
        let mut attempted = 0u64;

        let result = self.budget.fetch_update(ORDERING, ORDERING, |current| {
            let new = if is_positive {
                current.saturating_add(abs)
            } else {
                current.saturating_sub(abs)
            };
            attempted = new;
            if new <= consumed {
                return None;
            }
            Some(new)
        });

        match result {
            Ok(_) => Ok(attempted),
            Err(_) => Err(attempted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_basics() {
        let snap = TokenBudgetSnapshot {
            budget: 100,
            consumed: 25,
        };
        assert_eq!(snap.remaining(), 75);
        assert!(!snap.is_exceeded());
        assert_eq!(snap.usage_pct(), 25);
    }

    #[test]
    fn snapshot_exceeded() {
        let snap = TokenBudgetSnapshot {
            budget: 100,
            consumed: 100,
        };
        assert!(snap.is_exceeded());
        assert_eq!(snap.remaining(), 0);
    }

    #[test]
    fn snapshot_zero_budget() {
        let snap = TokenBudgetSnapshot {
            budget: 0,
            consumed: 0,
        };
        assert_eq!(snap.usage_pct(), 0);
        assert!(snap.is_exceeded()); // 0 >= 0
    }

    #[test]
    fn budget_new_and_snapshot() {
        let b = TokenBudget::new(200, 50);
        let snap = b.snapshot();
        assert_eq!(snap.budget, 200);
        assert_eq!(snap.consumed, 50);
        assert_eq!(snap.remaining(), 150);
    }

    #[test]
    fn budget_record_usage() {
        let b = TokenBudget::new(1000, 0);
        b.record_usage(100, 50);
        assert_eq!(b.consumed(), 150);
        b.record_usage(200, 100);
        assert_eq!(b.consumed(), 450);
    }

    #[test]
    fn budget_set_remaining() {
        let b = TokenBudget::new(1000, 0);
        b.record_usage(300, 0);
        let new = b.set_remaining(500);
        // new_total = consumed(300) + 500 = 800
        assert_eq!(new, 800);
        assert_eq!(b.budget(), 800);
    }

    #[test]
    fn budget_adjust_delta_positive() {
        let b = TokenBudget::new(1000, 0);
        let new = b.adjust_delta(500).unwrap();
        assert_eq!(new, 1500);
        assert_eq!(b.budget(), 1500);
    }

    #[test]
    fn budget_adjust_delta_negative() {
        let b = TokenBudget::new(1000, 0);
        let new = b.adjust_delta(-300).unwrap();
        assert_eq!(new, 700);
        assert_eq!(b.budget(), 700);
    }

    #[test]
    fn budget_adjust_delta_rejects_below_consumed() {
        let b = TokenBudget::new(1000, 0);
        b.record_usage(800, 0);
        let result = b.adjust_delta(-500);
        // attempted: 1000 - 500 = 500, consumed = 800, 500 <= 800 → reject
        assert_eq!(result.unwrap_err(), 500);
    }

    #[test]
    fn budget_clone_shares_state() {
        let b = TokenBudget::new(1000, 0);
        let b2 = b.clone();
        b.record_usage(100, 0);
        assert_eq!(b2.consumed(), 100);
        b2.set_limit(500);
        assert_eq!(b.budget(), 500);
    }
}
