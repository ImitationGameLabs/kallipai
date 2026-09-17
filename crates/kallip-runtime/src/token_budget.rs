//! Instance-wide token budget shared by all agents.
//!
//! Provides [`TokenBudget`] — a handle to an `Arc<Mutex<BudgetState>>` holding
//! an explicit `BudgetState` enum (`Limited` or `Unlimited`), cloned from
//! `AppState` so every agent on the tagma shares the same budget. Unlimited
//! means enforcement is off; consumption is still tracked — it is observation
//! data, not enforcement data.

use std::sync::{Arc, Mutex, MutexGuard};

/// The instance-wide budget state: a finite limit with cumulative consumption,
/// or unlimited (enforcement off, consumption still tracked).
#[derive(Debug, Clone, PartialEq, Eq)]
enum BudgetState {
    Limited { budget: u64, consumed: u64 },
    Unlimited { consumed: u64 },
}

/// A point-in-time read of the budget state. Built under the state lock,
/// so `budget`/`consumed` are mutually consistent (no torn reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudgetSnapshot {
    /// Total token budget (limit); 0 when unlimited (the `unlimited` flag
    /// is authoritative).
    pub budget: u64,
    /// Cumulative tokens consumed.
    pub consumed: u64,
    /// Whether the budget is unlimited (no enforcement).
    pub unlimited: bool,
}

impl TokenBudgetSnapshot {
    /// Remaining tokens before budget exhaustion; 0 when unlimited.
    pub fn remaining(&self) -> u64 {
        if self.unlimited {
            return 0;
        }
        self.budget.saturating_sub(self.consumed)
    }

    /// Whether the budget has been fully consumed. Always false when
    /// unlimited — enforcement is off regardless of consumption.
    ///
    /// The `unlimited` short-circuit is load-bearing: a naive
    /// `consumed >= budget` on an unlimited snapshot (whose budget field is
    /// 0) would read `0 >= 0` and wrongly report exhaustion.
    pub fn is_exceeded(&self) -> bool {
        !self.unlimited && self.consumed >= self.budget
    }

    /// Budget usage as a percentage (0–100); 0 when unlimited. The
    /// multiplication saturates — a `Limited` state reached by migrating
    /// from a long unlimited run can carry a consumed value whose `*100`
    /// would overflow plain u64 arithmetic.
    pub fn usage_pct(&self) -> u8 {
        if self.unlimited || self.budget == 0 {
            return 0;
        }
        // Premise: production paths keep budget >= consumed —
        // set_remaining builds the new total from consumed and
        // adjust_delta rejects any result at or below it — so the
        // ratio stays <= 100 and the `as u8` never truncates.
        // `set_limit` has no production call site today; if one
        // appears and can push budget below consumed, clamp with
        // `.min(100)` here first.
        (self.consumed.saturating_mul(100) / self.budget) as u8
    }
}

/// Instance-wide token budget shared by all agents.
///
/// Wraps an `Arc<Mutex<BudgetState>>` — an explicit `Limited`/`Unlimited`
/// state machine — cloned from `AppState` so every agent on the tagma shares
/// the same budget.
///
/// `Clone` is cheap: it clones the inner `Arc` (reference-count increment),
/// producing a handle to the **same** underlying budget, not a copy. All
/// methods take the lock, finish, and return an owned value — nothing holds
/// the guard across an await point.
#[derive(Clone)]
pub struct TokenBudget {
    state: Arc<Mutex<BudgetState>>,
}
impl std::fmt::Debug for TokenBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snap = self.snapshot();
        f.debug_struct("TokenBudget")
            .field("budget", &snap.budget)
            .field("consumed", &snap.consumed)
            .field("unlimited", &snap.unlimited)
            .finish()
    }
}

impl TokenBudget {
    /// Create a fresh finite budget from a config limit and an initial
    /// consumption value. Used at tagma startup when KALLIP_TOKEN_BUDGET is
    /// set (`initial_consumed = 0`).
    pub fn new(budget: u64, initial_consumed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(BudgetState::Limited {
                budget,
                consumed: initial_consumed,
            })),
        }
    }

    /// Create an unlimited budget: enforcement off, consumption tracked
    /// from zero. The startup default when KALLIP_TOKEN_BUDGET is unset.
    pub fn unlimited() -> Self {
        Self {
            state: Arc::new(Mutex::new(BudgetState::Unlimited { consumed: 0 })),
        }
    }

    /// Take the state lock, recovering from a poisoned mutex.
    ///
    /// Poisoning cannot mean broken invariants here: every critical section
    /// is pure arithmetic on plain u64s, so the guarded value is always
    /// well-formed and `into_inner` is safe.
    fn lock(&self) -> MutexGuard<'_, BudgetState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the budget is currently unlimited (enforcement off).
    pub fn is_unlimited(&self) -> bool {
        matches!(&*self.lock(), BudgetState::Unlimited { .. })
    }

    // -----------------------------------------------------------------------
    // Reads
    // -----------------------------------------------------------------------
    /// Read the current budget limit; 0 when unlimited.
    pub fn budget(&self) -> u64 {
        match &*self.lock() {
            BudgetState::Limited { budget, .. } => *budget,
            BudgetState::Unlimited { .. } => 0,
        }
    }

    /// Read the current cumulative consumption.
    pub fn consumed(&self) -> u64 {
        match &*self.lock() {
            BudgetState::Limited { consumed, .. } | BudgetState::Unlimited { consumed } => {
                *consumed
            }
        }
    }

    /// Remaining tokens before budget exhaustion; 0 when unlimited.
    pub fn remaining(&self) -> u64 {
        self.snapshot().remaining()
    }

    /// Take a point-in-time snapshot of the budget state. Built under the
    /// state lock, so `budget`/`consumed`/`unlimited` are mutually consistent.
    pub fn snapshot(&self) -> TokenBudgetSnapshot {
        match &*self.lock() {
            BudgetState::Limited { budget, consumed } => TokenBudgetSnapshot {
                budget: *budget,
                consumed: *consumed,
                unlimited: false,
            },
            BudgetState::Unlimited { consumed } => TokenBudgetSnapshot {
                budget: 0,
                consumed: *consumed,
                unlimited: true,
            },
        }
    }

    /// Whether the budget has been fully consumed. Always false when
    /// unlimited (the Snapshot short-circuit carries the semantics).
    pub fn is_exceeded(&self) -> bool {
        self.snapshot().is_exceeded()
    }

    /// Budget usage as a percentage (0–100).
    ///
    /// Returns 0 when budget is 0 (division-by-zero guard).
    pub fn usage_pct(&self) -> u8 {
        self.snapshot().usage_pct()
    }

    // -----------------------------------------------------------------------
    /// Record token usage from an LLM call.
    ///
    /// Adds `prompt_tokens + completion_tokens` to the cumulative
    /// consumption — tracked in both variants (observation data, per the
    /// design decision). This is the only method that mutates consumption.
    pub fn record_usage(&self, prompt_tokens: u64, completion_tokens: u64) {
        let total = prompt_tokens.saturating_add(completion_tokens);
        let mut state = self.lock();
        match &mut *state {
            BudgetState::Limited { consumed, .. } | BudgetState::Unlimited { consumed } => {
                *consumed = consumed.saturating_add(total);
            }
        }
    }

    /// Set the total budget limit to an explicit value, migrating to the
    /// `Limited` variant under the lock (an explicit limit leaves unlimited
    /// mode). Consumption is unchanged.
    pub fn set_limit(&self, limit: u64) {
        let mut state = self.lock();
        let consumed = match &*state {
            BudgetState::Limited { consumed, .. } | BudgetState::Unlimited { consumed } => {
                *consumed
            }
        };
        *state = BudgetState::Limited {
            budget: limit,
            consumed,
        };
    }

    /// Set remaining budget: the new total is `consumed + value`, applied by
    /// migrating to the `Limited` variant under the lock. Intentionally
    /// allows `remaining == 0` (pause mechanism: agents stop on the next
    /// budget check). This is the documented way to leave unlimited mode
    /// with an explicit value. Returns the new total budget.
    pub fn set_remaining(&self, value: u64) -> u64 {
        let mut state = self.lock();
        let consumed = match &*state {
            BudgetState::Limited { consumed, .. } | BudgetState::Unlimited { consumed } => {
                *consumed
            }
        };
        let new_budget = consumed.saturating_add(value);
        *state = BudgetState::Limited {
            budget: new_budget,
            consumed,
        };
        new_budget
    }

    /// Adjust the total budget by a signed delta. Returns `Ok(new_budget)`
    /// on success; `Err(attempted)` if the result would land at or below the
    /// current consumption, or if the budget is currently unlimited
    /// (enforcement is off — set a finite budget first; the routes layer
    /// maps both to 409). The whole adjustment runs under the state lock, so
    /// a concurrent `set_unlimited` between the route's pre-check and this
    /// call deterministically lands in the `Unlimited` arm instead of
    /// racing.
    pub fn adjust_delta(&self, delta: i64) -> Result<u64, u64> {
        let mut state = self.lock();
        match &mut *state {
            BudgetState::Unlimited { consumed } => Err(*consumed),
            BudgetState::Limited { budget, consumed } => {
                let abs = delta.unsigned_abs();
                let new = if delta > 0 {
                    budget.saturating_add(abs)
                } else {
                    budget.saturating_sub(abs)
                };
                if new <= *consumed {
                    return Err(new);
                }
                *budget = new;
                Ok(new)
            }
        }
    }

    /// Switch to an unlimited budget: enforcement off, consumption kept.
    /// The documented counterpart to `set_remaining` for leaving either
    /// variant.
    pub fn set_unlimited(&self) {
        let mut state = self.lock();
        let consumed = match &*state {
            BudgetState::Limited { consumed, .. } | BudgetState::Unlimited { consumed } => {
                *consumed
            }
        };
        *state = BudgetState::Unlimited { consumed };
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
            unlimited: false,
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
            unlimited: false,
        };
        assert!(snap.is_exceeded());
        assert_eq!(snap.remaining(), 0);
    }

    #[test]
    fn snapshot_zero_budget() {
        let snap = TokenBudgetSnapshot {
            budget: 0,
            consumed: 0,
            unlimited: false,
        };
        assert_eq!(snap.usage_pct(), 0);
        assert!(snap.is_exceeded()); // 0 >= 0
    }

    #[test]
    fn snapshot_unlimited_short_circuits_enforcement() {
        // An unlimited snapshot reports budget = 0; a naive consumed >=
        // budget would read 0 >= 0 and wrongly claim exhaustion.
        let snap = TokenBudgetSnapshot {
            budget: 0,
            consumed: 5,
            unlimited: true,
        };
        assert!(!snap.is_exceeded());
        assert_eq!(snap.remaining(), 0);
        assert_eq!(snap.usage_pct(), 0);
    }

    #[test]
    fn budget_new_and_snapshot() {
        let b = TokenBudget::new(200, 50);
        let snap = b.snapshot();
        assert_eq!(snap.budget, 200);
        assert_eq!(snap.consumed, 50);
        assert!(!snap.unlimited);
        assert_eq!(snap.remaining(), 150);
    }

    #[test]
    fn unlimited_construction_disables_enforcement() {
        let b = TokenBudget::unlimited();
        assert!(b.is_unlimited());
        assert!(!b.is_exceeded());
        assert_eq!(b.budget(), 0);
        let snap = b.snapshot();
        assert!(snap.unlimited);
        assert_eq!(snap.consumed, 0);
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
    fn unlimited_record_usage_still_tracks() {
        let b = TokenBudget::unlimited();
        b.record_usage(100, 50);
        b.record_usage(200, 100);
        assert_eq!(b.consumed(), 450); // observation data, not enforcement
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
    fn unlimited_set_remaining_migrates_to_limited() {
        let b = TokenBudget::unlimited();
        b.record_usage(300, 0);
        assert_eq!(b.set_remaining(500), 800);
        assert!(!b.is_unlimited());
        assert_eq!(b.budget(), 800);
        assert!(!b.is_exceeded());
        // set_remaining(0) from unlimited = an explicit finite pause.
        b.set_remaining(0);
        assert!(b.is_exceeded());
    }

    #[test]
    fn set_unlimited_switches_back_keeping_consumed() {
        let b = TokenBudget::new(1000, 0);
        b.record_usage(400, 0);
        b.set_unlimited();
        assert!(b.is_unlimited());
        assert!(!b.is_exceeded());
        assert_eq!(b.consumed(), 400);
    }

    #[test]
    fn budget_adjust_delta_positive() {
        let b = TokenBudget::new(1000, 0);
        assert_eq!(b.adjust_delta(500).unwrap(), 1500);
        assert_eq!(b.budget(), 1500);
    }

    #[test]
    fn budget_adjust_delta_negative() {
        let b = TokenBudget::new(1000, 0);
        assert_eq!(b.adjust_delta(-300).unwrap(), 700);
        assert_eq!(b.budget(), 700);
    }

    #[test]
    fn budget_adjust_delta_rejects_below_consumed() {
        let b = TokenBudget::new(1000, 0);
        b.record_usage(800, 0);
        // attempted: 1000 - 500 = 500, consumed = 800, 500 <= 800 → reject
        assert_eq!(b.adjust_delta(-500).unwrap_err(), 500);
    }

    #[test]
    fn adjust_delta_on_unlimited_is_err_toctou_backstop() {
        let b = TokenBudget::unlimited();
        b.record_usage(700, 0);
        // The route pre-check makes this unreachable in normal flow; the
        // in-lock arm is the TOCTOU backstop and reports consumed.
        assert_eq!(b.adjust_delta(100).unwrap_err(), 700);
    }

    #[test]
    fn set_limit_leaves_unlimited_mode() {
        let b = TokenBudget::unlimited();
        b.record_usage(100, 0);
        b.set_limit(500);
        assert!(!b.is_unlimited());
        assert_eq!(b.budget(), 500);
        assert_eq!(b.consumed(), 100);
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

    #[test]
    fn usage_pct_saturates_on_huge_consumed() {
        // Migrating from a long unlimited run can leave consumed above
        // u64::MAX / 100, where plain *100 would overflow; saturating_mul
        // keeps usage_pct finite (here: saturated to u64::MAX, then divided
        // back down to 50 instead of panicking).
        let big = u64::MAX / 50;
        let snap = TokenBudgetSnapshot {
            budget: big,
            consumed: big,
            unlimited: false,
        };
        assert_eq!(snap.usage_pct(), 50);
    }

    #[test]
    fn snapshot_reflects_usage_immediately() {
        // Snapshot is built under the same lock that applies the mutation,
        // so a read after record_usage sees the new value (no torn window).
        let b = TokenBudget::new(1000, 0);
        b.record_usage(60, 40);
        assert_eq!(b.snapshot().consumed, 100);
        assert_eq!(b.remaining(), 900);
    }
}
