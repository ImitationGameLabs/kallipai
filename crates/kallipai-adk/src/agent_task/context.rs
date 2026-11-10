//! What an agent carries between rounds, and the two ways it changes outside one.
//!
//! The context holds the resources it was spawned with, the lifecycle state every
//! move is recorded from, the transcript turns are appended to, and the episode
//! accounting a round charges. A profile reset is the one mutation that swaps its
//! client and its conversation chain together.

use super::message_guard::cap_external_message;
use super::rounds::RoundToken;
use super::task_loop::MessagePuller;
use crate::acquisition::release_unspent_window;
use crate::approval::ApprovalStore;
use crate::config::AgentConfig;
use crate::context::{CompactionLedger, ContextStore, ContextSummarizer, TurnId};
use crate::history::{HistoryWriter, RecordKind};
use crate::lifecycle::LifecycleState;
use crate::policy::AuthorizedToolExecutor;
use crate::retry::{AttemptOrigin, Episode};
use just_llm_client::types::generation::Message;
use kallipai_common::protocol::AgentState;
use kallipai_common::timefmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

/// Consecutive context-persist failures before the streak escalates from a
/// per-failure warning to an "agent is losing its window" error.
pub(super) const PERSIST_FAILURES_WARN_AFTER: u32 = 3;

/// Shared agent resources passed between modes.
pub struct AgentContext {
    pub client: crate::profile::GenerationClient,
    /// Stateful conversation chain bound to `client`: it remembers the last turn's
    /// provider response id so stateful backends receive `previous_response_id` plus
    /// a delta instead of the full history. Rebuilt at every site that replaces
    /// `client` (spawn, failover advance, profile reset) — response ids issued by one
    /// provider are meaningless to another, so a new client always gets a fresh chain.
    pub conversation: just_llm_client::Conversation,
    /// Within-set failover state: the resolved profile set, the profile registry (for
    /// rebuilding the client on advance), the system prompt, and the sticky `profile_idx` (the
    /// sole writer of which is `FailoverState::advance_to`). See `FailoverState`.
    pub failover: crate::failover::FailoverState,
    pub store: Arc<Mutex<ContextStore>>,
    pub approvals: Arc<Mutex<ApprovalStore>>,
    pub executor: AuthorizedToolExecutor,
    pub summarizer: ContextSummarizer,
    pub config: AgentConfig,
    /// Retry accounting for the current episode: sends charged and the moment the
    /// episode started. Replaced at every tool round, at every failover advance,
    /// and before a rejected request is re-sent without its images; a mid-stream
    /// re-entry keeps it. See [`Episode`].
    pub episode: Episode,
    /// Whether the next attempt is the wake-triggered execution's first: the
    /// first send of a wake that carried a user message or a task delivery reads
    /// [`AttemptOrigin::External`] once, and every later attempt reads `System`.
    /// Set at each `run_and_report` entry from the wake that caused it and taken
    /// by the pre-send reading. See [`AttemptOrigin`].
    pub attempt_origin: AttemptOrigin,
    /// Agent directory for persistence.
    pub agent_dir: Option<PathBuf>,
    /// Append-only conversation history writer. `Some` when `agent_dir` is `Some`.
    pub history: Option<HistoryWriter>,
    /// Cancellation signal for graceful interruption.
    pub cancel: CancellationToken,
    /// The current round's cancellation token, reachable by the tagma's
    /// interrupt path. `Some` only while a round is running. See [`RoundToken`].
    pub round_cancel: Arc<std::sync::Mutex<Option<RoundToken>>>,
    /// Wake signal triggered by external events (e.g. approval notifications).
    /// The agent task awaits this in the outer loop; callers signal via `notify_one()`.
    pub notify: Arc<Notify>,
    /// Wake signal for the timed transient-retry path. A separate [`Notify`] (not
    /// `notify`) so the approval arm's `has_notifications()` guard stays the single
    /// authority for approval wakes — see the outer-loop comment. Driven by a
    /// best-effort spawned sleep task armed by `schedule_transient_retry`.
    pub retry_notify: Arc<Notify>,
    /// The armed transient-retry deadline, or `None` when no retry is pending. The
    /// retry select arm is authoritative: it re-enters only when this is `Some(t)`
    /// with `t <= now`, then clears it. Cleared on every non-retry wake so a stale
    /// stored permit (a sleep that fired after a different arm won the race) cannot
    /// trigger a spurious round.
    pub retry_at: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
    /// Consecutive transient (failover-chain-exhausted) parks. Survives outer-loop
    /// wakes so consecutive parks count together; reset on any non-transient round
    /// outcome. Hard-parks (surfaces to the operator) once it exceeds
    /// `config.max_transient_retries`.
    pub transient_fails: u32,
    /// Authoritative lifecycle state (the outer loop keeps
    /// a single `select!` whose per-state arm differences are guards; this enum
    /// owns the state itself, replacing the scattered wait/park Option-fields).
    /// Written only via `LifecycleState::transition`, which asserts the
    /// legal-edge table in debug builds. `Running` is held only inside
    /// `run_and_report`.
    pub lifecycle: std::sync::Mutex<LifecycleState>,
    /// Compaction-failure accounting for this agent: the failing stretch's
    /// attempt count and endpoint facts, plus the latch deadline that keeps the
    /// pre-call gate from repeating a doomed summarizer call before the backend
    /// has had time to recover.
    pub compaction: CompactionLedger,
    /// The armed wait-timer deadline (`break(wait)` or a budget-blocked
    /// re-arm), or `None`. Mirrors `retry_at`'s authority pattern: the wait
    /// select arm honors a stored permit only when this is `Some(t)` with
    /// `t <= now`, then clears it; the prompt wake and the notify wake clear
    /// it too, so a stale stored permit cannot fire a spurious turn.
    pub wait_until: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
    /// Wake signal for the wait timer. Separate from `notify` for the same
    /// reason as `retry_notify` (each signal's guard stays the sole
    /// authority for its wake). Driven by a best-effort, cancel-aware
    /// spawned sleep.
    pub wait_notify: Arc<Notify>,
    /// The `timeout_secs` the wait timer was last armed with (for the
    /// elapsed-turn text). Meaningful only while `wait_until` is armed.
    pub wait_armed_secs: u64,
    /// The armed pacing-wake deadline: the earliest window end on the chain a
    /// quota park waits out, plus jitter, or `None` when no such wake is
    /// pending. Same authority pattern as `retry_at`: the pacing select arm
    /// honors a stored permit only when this is `Some(t)` with `t <= now`,
    /// then clears it; the prompt wake and the notify wake clear it too, so a
    /// permit left by a sleep that lost the race cannot start a round nobody
    /// asked for.
    pub pacing_wake_at: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
    /// Wake signal for the pacing timer. Separate from `notify` for the same
    /// reason as `retry_notify` (each signal's guard stays the sole authority
    /// for its wake). Driven by a best-effort, cancel-aware spawned sleep.
    pub pacing_notify: Arc<Notify>,
    /// Instance-wide token budget shared by all agents.
    /// Cloned from `AppState` — same underlying Arc counters across all agents.
    pub token_budget: crate::token_budget::TokenBudget,
    /// Per-agent, single-launch token usage shared process-wide.
    /// Cloned from `AppState` — same underlying map across all agents;
    /// the two accounting points record into one shared view.
    pub usage_stats: crate::usage_stats::UsageStats,
    /// Wake, model and tool counters, shared process-wide.
    /// Taken from the process's own set, not from the tagma state: recording
    /// happens deep in the acquisition loop, below the layers that hold that
    /// state. A context built outside a running tagma (tests, tools) keeps its
    /// own set, which nothing exports.
    pub metrics: crate::metrics::AdkMetrics,
    /// Tagma-wide upstream-fact table, shared by every agent. Cloned from
    /// `AppState`; `None` in contexts with no tagma behind them (unit
    /// tests), which reads as no facts at all — every read is fail-open.
    pub profile_pacing: Option<Arc<dyn kallipai_common::facts::ProfilePacing>>,
    /// Pending profile-reset cell: the tagma's apply handler writes a
    /// [`ProfileReset`](crate::failover::ProfileReset) here; the agent task
    /// drains it in the notify arm and at the top of [`run_and_report`](crate::agent_task::run_and_report),
    /// rebuilding its failover state + client. Shared (same `Arc`) with the
    /// tagma `Agent` struct so the apply route can write to it without
    /// reaching into adk internals. `None` when no reset is pending.
    pub pending_profile_reset: Arc<std::sync::Mutex<Option<crate::failover::ProfileReset>>>,
    /// Generation counter of the armed park (a `break(wait)` fuse, a
    /// budget-probe re-arm, or a chain-transient retry). Bumped by
    /// `AgentContext::arm_park` — the single place a wake park is armed —
    /// so every park publishes its generation before the caller settles the
    /// mirrored state: a mirror saying `Waiting`/`Retrying` therefore always
    /// names a park whose epoch is already out. A generation rather than a due
    /// time: a due time sits in `wait_until` or `retry_at`, both cleared to
    /// `None`, so its absence cannot be told from no park at all; the
    /// generation is always there and one bump covers all three arm sites.
    /// Shared (same `Arc`) with the tagma `Agent`, where the interrupt route
    /// reads it.
    pub park_epoch: Arc<AtomicU64>,
    /// Pending park-cancel cell: the interrupt route stamps it with the
    /// epoch it read and calls `notify_one`; the notify arm drains it right
    /// after the reset and cancels only the park that stamp names. Same
    /// shape as `pending_profile_reset`: a handler writes, the loop drains.
    /// Shared (same `Arc`) with the tagma `Agent`.
    pub park_cancel: Arc<std::sync::Mutex<Option<u64>>>,
    /// Inbox message puller. Injected by the tagma so the runtime can pull
    /// undelivered direct messages on a notify wake without depending on the
    /// tagma crate. `None` for test contexts and agents without an inbox.
    pub message_puller: Option<Arc<dyn MessagePuller>>,
    /// Consecutive context-persist failures. A single miss is a logged
    /// error; a streak means the agent is silently losing its context
    /// window (every persist rewrites it), so the third consecutive miss
    /// escalates to an explicit data-loss warning. Any success clears the
    /// streak.
    pub persist_failures: AtomicU32,
    /// Gateway auth-rejection observer (401/403 surfaced at failover-chain
    /// exhaustion). Injected by the tagma in gateway-proxy mode; None (local
    /// mode, test contexts) is zero behavior. See [`crate::GatewaySignal`].
    pub gateway_signals: Option<tokio::sync::mpsc::UnboundedSender<crate::GatewaySignal>>,
}

/// The protocol-facing label of a lifecycle state: the log line and the protocol
/// status vocabulary come from this table; the bridge maps the UI separately.
fn state_label(state: &LifecycleState) -> AgentState {
    match state {
        LifecycleState::Running => AgentState::Busy,
        LifecycleState::Idle => AgentState::Idle,
        LifecycleState::Waiting { .. } => AgentState::Waiting,
        LifecycleState::Parked { .. } => AgentState::Parked,
        LifecycleState::Retrying { .. } => AgentState::Retrying,
    }
}

impl AgentContext {
    /// Apply the next authoritative lifecycle state and record the move.
    ///
    /// Every production transition goes through here, so the log channel and the
    /// state machine cannot drift apart. `cause` is the site's own phrase
    /// (`break(wait)`, `budget probe re-arm`, ...) — the coarser `origin` cannot
    /// separate the wait timer from the retry timer.
    ///
    /// Takes the lifecycle lock internally: callers must not hold it.
    ///
    /// Applies the move before recording it, so a rejected transition (the
    /// debug-asserted legality check) cannot leave a line claiming a move the
    /// agent never made.
    pub fn settle(&self, next: LifecycleState, cause: &'static str) {
        let mut guard = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let from = state_label(&guard);
        let agent = crate::spans::agent_label(self.config.agent_id.as_ref());
        guard.transition(next);
        let to = state_label(&guard);
        match &*guard {
            LifecycleState::Parked { reason, .. } => tracing::warn!(
                agent = %agent, from = %from, to = %to, cause = %cause, reason = %reason,
                "agent parked"
            ),
            LifecycleState::Retrying {
                attempt,
                max_attempts,
                ..
            } => tracing::info!(
                agent = %agent, from = %from, to = %to, cause = %cause,
                attempt = *attempt, max = *max_attempts,
                "agent state changed"
            ),
            LifecycleState::Running => tracing::info!(
                agent = %agent, from = %from, to = %to, cause = %cause, origin = ?self.attempt_origin,
                "agent state changed"
            ),
            LifecycleState::Idle | LifecycleState::Waiting { .. } => tracing::info!(
                agent = %agent, from = %from, to = %to, cause = %cause,
                "agent state changed"
            ),
        }
    }

    /// Persist context and approval state to disk. Logs warnings on failure.
    ///
    /// Context-persist failures are counted; see `persist_failures`.
    pub async fn persist(&self) {
        let Some(ref dir) = self.agent_dir else {
            return;
        };

        {
            let guard = self.store.lock().await;
            if let Err(e) = crate::persistence::persist_context(&guard, dir) {
                let streak = self.persist_failures.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::error!("context persist failed ({streak} in a row): {e:#}");
                if streak == PERSIST_FAILURES_WARN_AFTER {
                    tracing::error!(
                        "agent has lost {streak} consecutive persists; its context window is no longer being saved — check disk space and permissions"
                    );
                }
            } else {
                self.persist_failures.store(0, Ordering::Relaxed);
            }
        }
        {
            let guard = self.approvals.lock().await;
            if let Ok(json) = serde_json::to_string(&*guard)
                && let Err(e) = crate::persistence::persist_approvals(&json, dir)
            {
                tracing::error!("approval persist failed: {e:#}");
            }
        }
    }

    /// Fire-and-forget append to history. Logs a warning on failure.
    #[allow(clippy::too_many_arguments)] // forwards the record's own shape unchanged
    pub(crate) fn append_history(
        &self,
        turn_id: Option<u64>,
        messages: &[Message],
        estimated_tokens: usize,
        kind: RecordKind,
        event: Option<crate::history::SystemEvent>,
        attachments: &[crate::history::AttachmentRef],
        attempt: Option<&crate::history::TurnAttempt>,
    ) {
        if let Some(ref history) = self.history
            && let Err(e) = history.append(
                turn_id,
                messages,
                estimated_tokens,
                kind,
                event,
                attachments,
                attempt,
            )
        {
            tracing::warn!(turn_id = ?turn_id, "history write failed: {e:#}");
        }
    }

    /// Record a turn to both the context store and the append-only history log.
    /// Returns the assigned `TurnId`.
    pub async fn record_turn(&self, messages: Vec<Message>) -> TurnId {
        self.record_turn_entry(messages, Vec::new(), None).await
    }

    /// The attachment-carrying form of [`Self::record_turn`] — the frozen
    /// recording contract: the image read path fills `attachments` (and
    /// the message parts); this signature does not change again.
    /// `record_turn` reaches `record_turn_entry` with no attachments; this form
    /// is the only one that carries them.
    pub async fn record_turn_with_attachments(
        &self,
        messages: Vec<Message>,
        attachments: Vec<crate::history::AttachmentRef>,
    ) -> TurnId {
        self.record_turn_entry(messages, attachments, None).await
    }

    /// A turn a model round produced: the same recording path, carrying what
    /// served it (profile, round, usage, elapsed) into the history record.
    /// A sibling of [`Self::record_turn_with_attachments`] rather than another
    /// parameter: that signature is frozen, and an image turn is not a model
    /// round, so there is no attempt to carry.
    pub async fn record_turn_with_attempt(
        &self,
        messages: Vec<Message>,
        attempt: crate::history::TurnAttempt,
    ) -> TurnId {
        self.record_turn_entry(messages, Vec::new(), Some(attempt))
            .await
    }

    /// The one recording path behind every turn entry point: push into the
    /// context store, then append to history with whatever produced it.
    async fn record_turn_entry(
        &self,
        messages: Vec<Message>,
        attachments: Vec<crate::history::AttachmentRef>,
        attempt: Option<crate::history::TurnAttempt>,
    ) -> TurnId {
        let (turn_id, estimated_tokens) = {
            let mut guard = self.store.lock().await;
            guard.push_turn(messages.clone())
        };
        self.append_history(
            Some(turn_id.0),
            &messages,
            estimated_tokens,
            RecordKind::Turn,
            None,
            &attachments,
            attempt.as_ref(),
        );
        turn_id
    }

    /// What served one round, in the shape a turn record carries it: the
    /// profile in flight (after any failover advance), the round index, the
    /// response usage flattened to the five counters, and the round clock.
    /// Built here so the round loop and the outer loop cannot drift apart.
    pub(crate) fn turn_attempt(
        &self,
        round: usize,
        usage: Option<&just_llm_client::types::generation::Usage>,
        elapsed_ms: u64,
    ) -> crate::history::TurnAttempt {
        crate::history::TurnAttempt::from_profile(
            &self.failover.set().name,
            self.failover.current_profile(),
            round,
            usage.map(crate::history::TurnUsage::from_usage),
            elapsed_ms,
        )
    }

    /// Record an external message turn (user prompt, peer or relay
    /// message) through the entry cap — see cap_external_message, the
    /// shared transform behind every external entrance. Oversized
    /// messages are cut to a head+tail slice with a banner pointing at
    /// the spilled original, so a giant paste can still be recovered via
    /// Read instead of surviving only as compaction-summary fodder.
    ///
    /// Not applied to the initial prompt: that call site stays on raw
    /// record_turn — the initial prompt is the agent's identity and task
    /// start, lives inside the trust boundary, and its semantic integrity
    /// outweighs size hygiene.
    pub async fn record_message_turn(&self, text: &str) -> TurnId {
        let guarded = cap_external_message(text, true);
        self.record_turn(vec![Message::user(&guarded)]).await
    }

    /// The attachment-carrying form of [`Self::record_message_turn`] — the
    /// other half of the frozen recording contract: the image read path calls
    /// this with the sidecar refs matching the parts it assembled. The same
    /// entry-cap transform applies (it guards text size only; attachment
    /// refs are tiny metadata).
    pub async fn record_message_turn_with_attachments(
        &self,
        text: &str,
        attachments: Vec<crate::history::AttachmentRef>,
    ) -> TurnId {
        let guarded = cap_external_message(text, true);
        self.record_turn_with_attachments(vec![Message::user(&guarded)], attachments)
            .await
    }

    /// Start a fresh retry episode: the send budget and its deadline both restart.
    ///
    /// Called at each tool round (so a rerun of the prompt starts clean, and no
    /// round inherits the previous round's sends), at every failover advance
    /// (another profile may recover, so it gets its own budget), and before a
    /// rejected request is re-sent without its images. A mid-stream re-entry
    /// does not reset: it continues the episode, so a flapping endpoint cannot
    /// mint budget by dropping connections.
    pub fn start_episode(&mut self) {
        // The episode ending here gives back a window it took and never sent
        // against: the record was this episode's, and no request was charged
        // to it.
        release_unspent_window(self);
        self.episode = Episode::new(timefmt::now_epoch());
    }

    /// Take the wake origin for the next attempt: the first attempt of a
    /// wake-triggered execution reads `External` once, and every later attempt
    /// reads `System` — the user asking is served once, not on every retry the
    /// execution then makes. See [`AttemptOrigin`].
    pub(crate) fn take_attempt_origin(&mut self) -> AttemptOrigin {
        std::mem::replace(&mut self.attempt_origin, AttemptOrigin::System)
    }
}

/// Drain the pending profile-reset cell (if set by the tagma's apply handler)
/// and rebuild the failover state + client. Called at two sites: once at the
/// top of each `run_and_report` wake-up, and once in the notify arm when a
/// pending reset is detected. On error, logs and continues on prior config —
/// the cell is cleared regardless so a bad reset does not retry forever.
pub(super) async fn apply_pending_profile_reset(ctx: &mut AgentContext) {
    let reset = ctx
        .pending_profile_reset
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(reset) = reset else { return };
    let new_window = reset.set.active_profile().max_context_window;
    let new_store = reset.set.active_profile().store.unwrap_or(true);
    match ctx.failover.reset_and_rebuild(reset.set, reset.registry) {
        Ok(new_client) => {
            ctx.client = new_client;
            ctx.compaction.clear(timefmt::now_epoch());
            // A reset swaps the endpoint, so a verdict the old endpoint's
            // summarizer reached is moot for the new one: the ledger starts over
            // here rather than parking the first round after the switch on a
            // verdict of the endpoint the agent just left.
            // Fresh chain for the fresh client: the reset may switch providers, and a
            // response id issued by the old one is meaningless to the new.
            // Store rides the reset's active profile (read before `reset.set` moves into
            // the rebuild; `None` = client default, store on).
            ctx.conversation = ctx.client.conversation().with_store(new_store);
            if let Err(e) = ctx.config.set_context_window(new_window) {
                tracing::warn!(
                    window = new_window,
                    "profile reset: failed to re-apply context window, keeping prior: {e:#}"
                );
            }
            // Moved or not — the re-apply may have failed and left the old window —
            // the two values a window move re-installs go back under one lock: the
            // pinned layer's capacity and the manual eviction's caps.
            {
                let mut store = ctx.store.lock().await;
                store.set_pinned_budget(ctx.config.pinned_budget());
                store.set_eviction_caps(ctx.config.eviction_caps());
            }
            tracing::info!("agent profile reset applied");
        }
        Err(e) => {
            tracing::error!("profile reset failed, continuing on prior config: {e:#}");
        }
    }
}
