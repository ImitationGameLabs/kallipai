//! The agent lifecycle state machine — the authoritative runtime state.
//!
//! The outer loop keeps a single `select!` whose per-state arm differences are
//! encoded as guards (the design's guard matrix), so this enum exists to own
//! the *state* — replacing the parallel `wait_until`/`parked_reason`/
//! `parked_at`/`retry_at` Option-fields that previously scattered it — and to
//! make illegal transitions loud via [`LifecycleState::transition`].
//!
//! Boundaries (deliberately NOT states here):
//! - mid-round internals (the round loop, the budget gate, heartbeat) run
//!   inside `Running` and never appear as enum variants;
//! - the delivery-layer duty gate stays in delivery — "system refuses to
//!   deliver" is not an agent state;
//! - in-request backoff (`retry.rs` `Retrying`/`StreamReset` events) happens
//!   inside acquisition while truly `Running`; the bridge renders it as a
//!   `RETRYING` display overlay, not a state transition.
//! - `transient_fails` (the chain-transient failure counter) lives on
//!   `AgentContext`, not in `Retrying`'s payload: it survives across states
//!   and is cleared only when a retried round succeeds, so payload-izing it
//!   would lose it on every transition.

use std::time::Instant;

use kallip_common::protocol::ParkedReason;

/// Authoritative lifecycle state of one agent task. `Running` is set just
/// before `run_and_report` is entered; the outer loop never observes it while
/// parked on its `select!`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LifecycleState {
    /// `run_and_report` is in flight (the outer loop does not hold this).
    Running,
    /// The agent parked itself with `break(wait)`; `until` is the armed fuse.
    Waiting { until: Instant },
    Idle,
    /// A failure path parked the agent awaiting operator action (kick or
    /// remove); `at` supports "parked N ago" in kick turns and status.
    Parked { reason: ParkedReason, at: Instant },
    /// Chain-transient backoff: a terminal FCE armed a delayed retry of the
    /// original prompt (timer fire re-runs it, no agent participation).
    Retrying {
        attempt: u32,
        max_attempts: u32,
        retry_at: Instant,
    },
}

impl LifecycleState {
    /// Apply the next state, asserting the transition is one of the legal
    /// edges (debug builds panic on anything else). Rules the table encodes:
    ///
    /// * entries into `Parked`/`Waiting`/`Retrying` happen only from `Running`
    ///   (failures and `break` occur mid-round; the FCE arm site is terminal);
    /// * `Parked` may only leave via kick (→`Running`) or restart degrade
    ///   (→`Idle`);
    /// * `Waiting`↔`Retrying` direct transitions are forbidden (agent-chosen
    ///   waiting and system-driven backoff are different wake families);
    /// * `Running`→`Running` is not a transition (a prompt queued during
    ///   `Running` re-enters `run_and_report` without a state change);
    /// * restart degrade goes only to `Idle` (timers and reasons are in-memory);
    /// * self-loops are legal for `Waiting` (budget-blocked re-arm) and
    ///   `Retrying` (re-armed backoff after another FCE).
    ///
    /// Birth (→`Idle`) is construction, not a transition; cancel/remove
    /// terminates the task without calling this.
    pub(crate) fn transition(&mut self, next: LifecycleState) {
        let legal = match self {
            Self::Idle => matches!(next, Self::Running),
            Self::Running => matches!(
                next,
                Self::Idle | Self::Waiting { .. } | Self::Parked { .. } | Self::Retrying { .. }
            ),
            Self::Waiting { .. } => matches!(next, Self::Running | Self::Waiting { .. } | Self::Idle),
            Self::Parked { .. } => matches!(next, Self::Running | Self::Idle),
            Self::Retrying { .. } => {
                matches!(next, Self::Running | Self::Retrying { .. } | Self::Idle)
            }
        };
        debug_assert!(legal, "illegal lifecycle transition: {self:?} -> {next:?}");
        *self = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting() -> LifecycleState {
        LifecycleState::Waiting {
            until: Instant::now(),
        }
    }

    fn parked() -> LifecycleState {
        LifecycleState::Parked {
            reason: ParkedReason::MaxRoundsExceeded,
            at: Instant::now(),
        }
    }

    fn retrying() -> LifecycleState {
        LifecycleState::Retrying {
            attempt: 1,
            max_attempts: 3,
            retry_at: Instant::now(),
        }
    }

    /// Every legal edge (the design's table minus birth-as-construction) is
    /// walkable: chains that collectively cover all 13 callable edges, with
    /// the landed state asserted, so a regression in the table reds here
    /// before it can silence an illegal path in production code.
    #[test]
    fn walks_every_legal_transition_edge() {
        // 2 Idle->Running, 3 Running->Idle.
        let mut s = LifecycleState::Idle;
        s.transition(LifecycleState::Running);
        s.transition(LifecycleState::Idle);
        assert_eq!(s, LifecycleState::Idle);

        // 4 Running->Waiting, 5 Waiting->Running, 6 Waiting self-loop (re-arm).
        s.transition(LifecycleState::Running);
        s.transition(waiting());
        let rearm = waiting();
        s.transition(rearm.clone());
        assert_eq!(s, rearm);
        s.transition(LifecycleState::Running);

        // 7 Running->Parked, 11 Parked->Running (kick).
        s.transition(parked());
        s.transition(LifecycleState::Running);

        // 8 Running->Retrying, 9 Retrying->Running, 10 Retrying self-loop.
        s.transition(retrying());
        s.transition(retrying());
        s.transition(LifecycleState::Running);

        // 12/13/14 restart degrade -> Idle from Waiting/Parked/Retrying.
        s.transition(waiting());
        s.transition(LifecycleState::Idle);
        s.transition(LifecycleState::Running);
        s.transition(parked());
        s.transition(LifecycleState::Idle);
        s.transition(LifecycleState::Running);
        s.transition(retrying());
        s.transition(LifecycleState::Idle);
    }

    #[test]
    #[should_panic(expected = "illegal lifecycle transition")]
    fn rejects_parked_entry_from_non_running_states() {
        waiting().transition(parked());
    }

    #[test]
    #[should_panic(expected = "illegal lifecycle transition")]
    fn rejects_waiting_entry_from_non_running_states() {
        LifecycleState::Idle.transition(waiting());
    }

    #[test]
    #[should_panic(expected = "illegal lifecycle transition")]
    fn rejects_retrying_entry_from_non_running_states() {
        waiting().transition(retrying());
    }

    #[test]
    #[should_panic(expected = "illegal lifecycle transition")]
    fn rejects_parked_escape_to_waiting() {
        parked().transition(waiting());
    }

    #[test]
    #[should_panic(expected = "illegal lifecycle transition")]
    fn rejects_waiting_to_retrying_direct_transition() {
        waiting().transition(retrying());
    }

    #[test]
    #[should_panic(expected = "illegal lifecycle transition")]
    fn rejects_retrying_to_waiting_direct_transition() {
        retrying().transition(waiting());
    }

    #[test]
    #[should_panic(expected = "illegal lifecycle transition")]
    fn rejects_running_self_transition() {
        LifecycleState::Running.transition(LifecycleState::Running);
    }
}
