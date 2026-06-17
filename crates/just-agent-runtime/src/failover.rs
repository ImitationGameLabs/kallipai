//! Within-tier failover state and outcome types.
//!
//! [`FailoverState`] bundles the runtime failover fields that were previously scattered on
//! [`crate::agent_task::AgentContext`]: the resolved capability [`Tier`], the
//! [`ProfileRegistry`] (used to rebuild the client on advance), the system prompt applied to
//! every client built for this agent, and a sticky `profile_idx`. `profile_idx` is private and
//! mutated only by [`FailoverState::advance_to`], making the forward-only invariant structural
//! rather than conventional.
//!
//! The advance *transition* itself — advancing on a `Failover` outcome, swapping the client,
//! re-applying the window, compacting — lives in `crate::runner::advance_failover`, not here:
//! it spills across the agent context (client/window/store) and so cannot be a pure self-method.
//! This module owns the state and the accessors that DRY the chain indexing; the runner owns the
//! choreography.

use std::sync::Arc;

use anyhow::Result;
use just_agent_common::protocol::FailoverChainExhaustion;
use just_llm_client::types::chat::ChatMessage;

use crate::profile::{ChatClient, Profile, ProfileRegistry, Tier};

/// Runtime within-tier failover state. Owned by [`crate::agent_task::AgentContext`] as
/// `ctx.failover`.
///
/// The active profile is `tier.profiles[profile_idx]`; the rest of the chain is the failover
/// order, walked forward-only on a terminal endpoint failure. `profile_idx` resets to 0 on
/// spawn/restore (a fresh [`FailoverState::new`]).
///
/// `pub` + a `pub` [`new`](Self::new) so the daemon can construct an `AgentContext`; the
/// accessors are `pub(crate)` (only the runtime reads the state).
pub struct FailoverState {
    tier: Tier,
    registry: Arc<ProfileRegistry>,
    system_prompt: Option<String>,
    /// Index into `tier.profiles` of the currently active profile. Private — advanced only via
    /// [`advance_to`](Self::advance_to).
    profile_idx: usize,
}

impl FailoverState {
    /// Construct at the head of the chain (`profile_idx = 0`).
    pub fn new(tier: Tier, registry: Arc<ProfileRegistry>, system_prompt: Option<String>) -> Self {
        Self {
            tier,
            registry,
            system_prompt,
            profile_idx: 0,
        }
    }

    /// The currently active profile (`tier.profiles[profile_idx]`).
    ///
    /// Named `current_profile` to disambiguate from [`Tier::active_profile`], which is always
    /// `profiles[0]` (the spawn-time active); `current_profile` tracks the runtime position and
    /// differs once failover has advanced.
    pub(crate) fn current_profile(&self) -> &Profile {
        // profile_idx is always in range: it starts at 0 and only advances within the chain
        // (advance_to is forward-only; the skip loop bounds via candidate_profile).
        &self.tier.profiles[self.profile_idx]
    }

    /// A cloned candidate `offset` positions ahead of the active profile (`None` past the chain
    /// end). Cloned — not borrowed — so callers can mutate `FailoverState` (e.g. `advance_to`)
    /// after inspecting the candidate without a borrow conflict. Failover is rare; the clone is
    /// cheap.
    pub(crate) fn candidate_profile(&self, offset: usize) -> Option<Profile> {
        self.tier.profiles.get(self.profile_idx + offset).cloned()
    }

    pub(crate) fn profile_idx(&self) -> usize {
        self.profile_idx
    }

    /// Total profiles in the tier (chain length). Used to distinguish the single-profile
    /// (`NoFailoverConfigured`) from multi-profile-tail (`AllBackupsExhausted`) exhaustion case.
    pub(crate) fn profile_count(&self) -> usize {
        self.tier.profiles.len()
    }

    /// Whether there is at least one profile ahead of the active one to fail over to.
    pub(crate) fn can_advance(&self) -> bool {
        self.profile_idx + 1 < self.tier.profiles.len()
    }

    /// Build a [`ChatClient`] for `profile` via the registry (looks up the endpoint's backend),
    /// applying this agent's system prompt.
    pub(crate) fn build_client(&self, profile: &Profile) -> Result<ChatClient> {
        self.registry
            .build_client(profile, self.system_prompt.clone())
    }

    /// Advance to `idx`. **The only mutator of `profile_idx`.** Forward-only — `debug_assert!`,
    /// not a release panic: this guards an internal invariant of a rare error path, and a release
    /// panic would turn a recoverable misconfiguration into a process crash. The test suite
    /// catches regressions; production degrades to a wrong index, not a crash.
    pub(crate) fn advance_to(&mut self, idx: usize) {
        debug_assert!(
            idx > self.profile_idx,
            "failover advance must move forward: {idx} <= {}",
            self.profile_idx
        );
        self.profile_idx = idx;
    }
}

/// Outcome of one within-tier failover advance attempt (see `crate::runner::advance_failover`).
///
/// `messages` is returned on [`Advanced`](Self::Advanced) — recomputed if compaction ran, else
/// unchanged — so the round loop can rebind its local without `advance_failover` taking it by
/// `&mut`.
///
/// `Debug` is manual because [`Advanced`](Self::Advanced) carries `Vec<ChatMessage>` and
/// [`ChainExhausted`](Self::ChainExhausted) carries `anyhow::Error` (neither critical for the
/// diagnostic line tests need).
pub(crate) enum FailoverOutcome {
    /// Advanced to a new active profile. `from`/`to` are profile ids. Under skip, `from`→`to`
    /// may jump over unbuildable intermediates (those are `warn!`-ed server-side, not surfaced
    /// here); the carried `messages` are recomputed if compaction ran.
    Advanced {
        from: String,
        to: String,
        messages: Vec<ChatMessage>,
    },
    /// No buildable candidate ahead — the chain is exhausted. Carries the **original** trigger
    /// (the endpoint-level failure that started the advance), not the per-candidate build errors
    /// (which are `warn!`-ed as each is skipped). `reason` distinguishes the structurally distinct
    /// exhaustion modes so the runner can surface a distinguishable terminal outcome.
    ChainExhausted {
        reason: FailoverChainExhaustion,
        trigger: anyhow::Error,
    },
    /// The round was cancelled during the advance.
    Cancelled,
    /// Compaction ran and hit the daemon token budget.
    BudgetExceeded { consumed: u64, budget: u64 },
}

impl std::fmt::Debug for FailoverOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Advanced { from, to, messages } => f
                .debug_struct("Advanced")
                .field("from", from)
                .field("to", to)
                .field("messages_len", &messages.len())
                .finish(),
            Self::ChainExhausted { reason, trigger } => {
                write!(f, "ChainExhausted({reason:?}, {trigger:#})")
            }
            Self::Cancelled => write!(f, "Cancelled"),
            Self::BudgetExceeded { consumed, budget } => {
                write!(f, "BudgetExceeded({consumed}/{budget})")
            }
        }
    }
}
