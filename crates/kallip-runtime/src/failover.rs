//! Within-set failover state and outcome types.
//!
//! [`FailoverState`] bundles the runtime failover fields that were previously scattered on
//! [`crate::agent_task::AgentContext`]: the resolved [`ProfileSet`], the
//! [`ProfileRegistry`] (used to rebuild the client on advance), the system prompt applied to
//! every client built for this agent, and a sticky `profile_idx`. `profile_idx` is private and
//! mutated only by [`FailoverState::advance_to`], making the forward-only invariant structural
//! rather than conventional. The state also mirrors the active profile's identity into a
//! [`ProfileSnapshot`] cell shared with the tagma, so status surfaces can show the profile
//! the client is actually using (which drifts from the spawn-time active after an advance).
//!
//! The advance *transition* itself — advancing on a `Failover` outcome, swapping the client,
//! re-applying the window, compacting — lives in `crate::acquisition::advance_failover`, not here:
//! it spills across the agent context (client/window/store) and so cannot be a pure self-method.
//! This module owns the state and the accessors that DRY the chain indexing; the acquisition
//! module owns the driving.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use just_llm_client::types::generation::Message;
use kallip_common::protocol::FailoverChainExhaustion;

use crate::profile::{GenerationClient, Profile, ProfileRegistry, ProfileSet};

/// Runtime within-set failover state. Owned by [`crate::agent_task::AgentContext`] as
/// `ctx.failover`.
///
/// The active profile is `set.profiles[profile_idx]`; the rest of the chain is the failover
/// order, walked forward-only on a terminal endpoint failure. `profile_idx` resets to 0 on
/// spawn/restore (a fresh [`FailoverState::new`]).
///
/// `pub` + a `pub` [`new`](Self::new) so the tagma can construct an `AgentContext`; the
/// accessors are `pub(crate)` (only the runtime reads the state).
pub struct FailoverState {
    set: ProfileSet,
    registry: Arc<ProfileRegistry>,
    system_prompt: Option<String>,
    /// Index into `set.profiles` of the currently active profile. Private — advanced only via
    /// [`advance_to`](Self::advance_to).
    profile_idx: usize,
    /// Mirror of the active profile's identity, written by the three methods that establish
    /// the active profile ([`new`](Self::new), [`advance_to`](Self::advance_to),
    /// [`reset_and_rebuild`](Self::reset_and_rebuild)) and read by the tagma through its own
    /// `Arc` handle for status surfaces. The runtime never reads it outside tests.
    snapshot: Arc<Mutex<ProfileSnapshot>>,
    /// The set's name — carried so the snapshot can surface it. The identity is
    /// immutable per set (names are map keys), so `new`/`reset_and_rebuild`
    /// establish it and nothing ever changes it.
    set_name: String,
}

/// Pending profile-reset payload: the tagma's apply handler writes this into a
/// shared cell on each live agent; the agent task drains it at the top of
/// [`crate::agent_task::run_and_report`] and rebuilds its [`FailoverState`]
/// against the new registry. Carries the resolved [`ProfileSet`] and the new
/// [`ProfileRegistry`] Arc.
#[derive(Clone)]
pub struct ProfileReset {
    pub set: ProfileSet,
    pub registry: Arc<ProfileRegistry>,
}

impl FailoverState {
    /// Construct at the head of the chain (`profile_idx = 0`). `snapshot` is the cell the
    /// tagma created (it keeps a clone of the `Arc` to read for status surfaces); it is
    /// seeded with the set's active profile here — the same derivation every later write
    /// uses — so the cell never shows a placeholder once the agent is observable.
    pub fn new(
        set: ProfileSet,
        registry: Arc<ProfileRegistry>,
        system_prompt: Option<String>,
        snapshot: Arc<Mutex<ProfileSnapshot>>,
    ) -> Self {
        let state = Self {
            set_name: set.name.clone(),
            set,
            registry,
            system_prompt,
            profile_idx: 0,
            snapshot,
        };
        state.write_snapshot(state.set.active_profile());
        state
    }

    /// The currently active profile (`set.profiles[profile_idx]`).
    ///
    /// Named `current_profile` to disambiguate from [`ProfileSet::active_profile`], which is always
    /// `profiles[0]` (the spawn-time active); `current_profile` tracks the runtime position and
    /// differs once failover has advanced.
    pub(crate) fn current_profile(&self) -> &Profile {
        // profile_idx is always in range: it starts at 0 and only advances within the chain
        // (advance_to is forward-only; the skip loop bounds via candidate_profile).
        &self.set.profiles[self.profile_idx]
    }

    /// A cloned candidate `offset` positions ahead of the active profile (`None` past the chain
    /// end). Cloned — not borrowed — so callers can mutate `FailoverState` (e.g. `advance_to`)
    /// after inspecting the candidate without a borrow conflict. Failover is rare; the clone is
    /// cheap.
    pub(crate) fn candidate_profile(&self, offset: usize) -> Option<Profile> {
        self.set.profiles.get(self.profile_idx + offset).cloned()
    }

    pub(crate) fn profile_idx(&self) -> usize {
        self.profile_idx
    }
    /// Clone the mirrored active-profile identity. Poison-tolerant (`into_inner`), matching
    /// the tagma-side read pattern — a panic elsewhere in a cell holder must not brick reads.
    pub fn profile_snapshot(&self) -> ProfileSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Total profiles in the set (chain length). Used to distinguish the single-profile
    /// (`NoFailoverConfigured`) from multi-profile-tail (`AllBackupsExhausted`) exhaustion case.
    pub(crate) fn profile_count(&self) -> usize {
        self.set.profiles.len()
    }

    /// Whether there is at least one profile ahead of the active one to fail over to.
    pub(crate) fn can_advance(&self) -> bool {
        self.profile_idx + 1 < self.set.profiles.len()
    }

    /// Build a [`GenerationClient`] for `profile` via the registry (looks up the endpoint's backend),
    /// applying this agent's system prompt.
    pub(crate) fn build_client(&self, profile: &Profile) -> Result<GenerationClient> {
        self.registry
            .build_client(profile, self.system_prompt.clone())
    }

    /// Mirror `profile`'s identity into the shared cell. Private — only the active-profile
    /// writers call it. Poison-tolerant (`into_inner`): the store is a single assignment,
    /// so a poisoned lock (a panic while holding the cell) discards nothing.
    fn write_snapshot(&self, profile: &Profile) {
        *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = ProfileSnapshot {
            set_name: self.set_name.clone(),
            profile_id: profile.id.clone(),
            provider: profile.endpoint.clone(),
            model: profile.model.clone(),
        };
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
        self.write_snapshot(&self.set.profiles[idx]);
    }
    /// Rebuild this failover state against a new registry and set (used by the
    /// online profile-apply path). Builds the client for the new set's active
    /// profile first (fail-fast on a misconfigured endpoint), then commits the
    /// set, registry, resets `profile_idx` to 0, and rewrites the shared profile snapshot.
    /// [`GenerationClient`] so the caller can swap `ctx.client`. On error, nothing is
    /// mutated — the agent continues on its prior config.
    ///
    /// `system_prompt` is carried over (it is agent-level config, not
    /// profile-level), and the caller must update `ctx.config`'s context window
    /// from the new active profile's `max_context_window`.
    pub(crate) fn reset_and_rebuild(
        &mut self,
        set: ProfileSet,
        registry: Arc<ProfileRegistry>,
    ) -> Result<GenerationClient> {
        let profile = set.active_profile();
        let client = registry.build_client(profile, self.system_prompt.clone())?;
        self.set_name = set.name.clone();
        self.set = set;
        self.registry = registry;
        self.profile_idx = 0;
        self.write_snapshot(self.set.active_profile());
        Ok(client)
    }
}
/// The active profile's identity, mirrored into [`FailoverState`]'s shared cell for the
/// tagma's status surfaces: the active set's name, the registry profile id, the
/// provider (endpoint) id it connects through, and the concrete model string sent to the
/// backend. Lets an operator see which model the client is actually using, which
/// differs from the spawn-time active after a within-set failover advance or an online
/// profile apply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileSnapshot {
    /// The active set's name (a registry map key).
    pub set_name: String,
    pub profile_id: String,
    /// The endpoint (provider) id this profile connects through.
    pub provider: String,
    pub model: String,
}

/// Outcome of one within-set failover advance attempt (see `crate::acquisition::advance_failover`).
///
/// `messages` is returned on [`Advanced`](Self::Advanced) — recomputed if compaction ran, else
/// unchanged — so the acquisition loop can rebind its local without `advance_failover` taking
/// it by `&mut`.
///
/// `Debug` is manual because [`Advanced`](Self::Advanced) carries `Vec<Message>` and
/// [`ChainExhausted`](Self::ChainExhausted) carries `anyhow::Error` (neither critical for the
/// diagnostic line tests need).
pub(crate) enum FailoverOutcome {
    /// Advanced to a new active profile. `from`/`to` are profile ids. Under skip, `from`→`to`
    /// may jump over unbuildable intermediates (those are `warn!`-ed server-side, not surfaced
    /// here); the carried `messages` are recomputed if compaction ran.
    Advanced {
        from: String,
        to: String,
        messages: Vec<Message>,
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
    /// Compaction ran and hit the tagma token budget.
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

#[cfg(test)]
mod tests;
