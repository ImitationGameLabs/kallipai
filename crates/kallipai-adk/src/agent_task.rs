//! Agent task orchestration: shared context, round execution, prompt processing.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::acquisition::release_unspent_window;
use crate::event::{AgentEvent, AgentOutcome};
use crate::lifecycle::LifecycleState;
use crate::tools::DEFAULT_BREAK_TIMEOUT_SECS;
use kallipai_common::facts::{MIN_RETRY_INTERVAL_SECS, window_end};
use kallipai_common::protocol::{AgentState, ParkedReason, TransientRetryInfo};
use kallipai_common::timefmt;

use crate::approval::ApprovalStore;
use crate::config::{
    AgentConfig, DEFAULT_MESSAGE_FULL_TOKENS, DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS,
};
use crate::context::{
    CompactionLedger, ContextStore, ContextSummarizer, TurnId, assistant_message, estimate_text,
};
use crate::history::{HistoryWriter, RecordKind};
use crate::policy::AuthorizedToolExecutor;
use crate::retry::{AttemptOrigin, Episode};
use crate::runner;
use crate::text_slice::converge_under_cap;
use just_llm_client::types::generation::Message;
use kallipai_shell::spill::{self, MESSAGE_SPILL, spill_root};

/// Consecutive context-persist failures before the streak escalates from a
/// per-failure warning to an "agent is losing its window" error.
const PERSIST_FAILURES_WARN_AFTER: u32 = 3;

/// A cancellation token scoped to a single round, always a child of the agent's lifecycle
/// token ([`AgentContext::cancel`]). Cancelled by `interrupt_agent` to abort the current
/// round without terminating the task.
///
/// Because it is a child, a lifecycle cancel (remove / tagma shutdown) propagates to it —
/// so **lifecycle-cancelled ⟹ round-cancelled**. The converse (round cancelled but
/// lifecycle not) is exactly what distinguishes an interrupt from a lifecycle cancel. This
/// holds iff the token is always minted via [`RoundToken::new`] from the lifecycle token;
/// the newtype makes that invariant structural rather than conventional.
#[derive(Clone)]
pub struct RoundToken(CancellationToken);

impl RoundToken {
    /// Mint a round token as a child of the agent lifecycle token.
    pub fn new(lifecycle: &CancellationToken) -> Self {
        Self(lifecycle.child_token())
    }

    /// Cancel this round. Called by `interrupt_agent`.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// The underlying token, for the runner (and its callees `consume_stream` /
    /// `stream_with_retry`) to select on, and for any caller to inspect
    /// (`handle().is_cancelled()`). The runner legitimately needs the raw
    /// `CancellationToken` for its `select!` arms, so this is the intended seam —
    /// not an encapsulation leak.
    pub fn handle(&self) -> &CancellationToken {
        &self.0
    }
}

/// How to treat a round-token cancel, classified by whether the lifecycle (parent)
/// token is also cancelled. Correct iff the round token is always a child of the
/// lifecycle token — see [`RoundToken`].
#[derive(Debug, PartialEq, Eq)]
enum CancelKind {
    /// The lifecycle token was cancelled (remove / shutdown) → terminate the task.
    Lifecycle,
    /// Only the round token was cancelled (interrupt) → keep the task alive.
    Interrupt,
}

impl CancelKind {
    /// Classify a round-token cancel by inspecting the lifecycle token.
    ///
    /// A lifecycle cancel propagates to its children, so if the lifecycle token is
    /// cancelled the round-token cancel was a consequence of it (terminate);
    /// otherwise the round token alone fired (interrupt).
    fn classify(lifecycle: &CancellationToken) -> Self {
        if lifecycle.is_cancelled() {
            Self::Lifecycle
        } else {
            Self::Interrupt
        }
    }
}

/// Trait for pulling undelivered inbox messages. Implemented by the tagma
/// (where InboxStore lives) and injected into AgentContext at spawn time.
/// `None` for test contexts (no inbox).
#[async_trait::async_trait]
pub trait MessagePuller: Send + Sync + 'static {
    /// Atomically mark all undelivered direct messages as delivered and return
    /// them as a formatted string. Returns `None` when no undelivered direct
    /// messages exist.
    async fn pull_undelivered(&self) -> Option<String>;
}

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
    /// The current round's cancellation token, reachable by `interrupt_agent`. `Some` only
    /// while a round is running. See [`RoundToken`].
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
    /// Tagma-wide upstream-fact table, shared by every agent. Cloned from
    /// `AppState`; `None` in contexts with no tagma behind them (unit
    /// tests), which reads as no facts at all — every read is fail-open.
    pub profile_pacing: Option<Arc<dyn kallipai_common::facts::ProfilePacing>>,
    /// Pending profile-reset cell: the tagma's apply handler writes a
    /// [`ProfileReset`](crate::failover::ProfileReset) here; the agent task drains it at the top of
    /// [`run_and_report`] and rebuilds its failover state + client. Shared
    /// (same `Arc`) with the tagma `Agent` struct so the apply route can write
    /// to it without reaching into adk internals. `None` when no reset is
    /// pending.
    pub pending_profile_reset: Arc<std::sync::Mutex<Option<crate::failover::ProfileReset>>>,
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
        let agent = self
            .config
            .agent_id
            .as_ref()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "unbound".to_string());
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
    pub(crate) fn append_history(
        &self,
        turn_id: Option<u64>,
        messages: &[Message],
        estimated_tokens: usize,
        kind: RecordKind,
        event: Option<crate::history::SystemEvent>,
        attachments: &[crate::history::AttachmentRef],
    ) {
        if let Some(ref history) = self.history
            && let Err(e) = history.append(
                turn_id,
                messages,
                estimated_tokens,
                kind,
                event,
                attachments,
            )
        {
            tracing::warn!(turn_id = ?turn_id, "history write failed: {e:#}");
        }
    }

    /// Record a turn to both the context store and the append-only history log.
    /// Returns the assigned `TurnId`.
    pub async fn record_turn(&self, messages: Vec<Message>) -> TurnId {
        self.record_turn_with_attachments(messages, Vec::new())
            .await
    }

    /// The attachment-carrying form of [`Self::record_turn`] — the frozen
    /// recording contract: the image read path fills `attachments` (and
    /// the message parts); this signature does not change again.
    /// `record_turn` delegates here with none.
    pub async fn record_turn_with_attachments(
        &self,
        messages: Vec<Message>,
        attachments: Vec<crate::history::AttachmentRef>,
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
        );
        turn_id
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
#[cfg(test)]
static SPILL_ROOT_OVERRIDE: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

/// Test-only spill-root injection: message-guard unit tests must be
/// hermetic (not write the real /tmp/kallipai tree), and the fail-open
/// branch needs a root that deterministically fails.
#[cfg(test)]
pub(crate) fn set_message_spill_root(root: Option<std::path::PathBuf>) {
    *SPILL_ROOT_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = root;
}

/// Serialize tests that touch the process-wide spill-root override
/// (cargo runs unit tests on parallel threads). Async mutex: tests
/// hold it across awaits.
#[cfg(test)]
pub(crate) async fn message_spill_serial() -> tokio::sync::MutexGuard<'static, ()> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    SERIAL.lock().await
}

/// Prefer the test override (while one is pinned) over the real root.
fn message_spill_root() -> std::path::PathBuf {
    #[cfg(test)]
    {
        if let Some(root) = SPILL_ROOT_OVERRIDE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return root;
        }
    }
    spill_root()
}

/// Entry-cap transform shared by every external message entrance: text
/// within the cap passes through unchanged; an oversized message is
/// spilled and cut down to a head+tail slice with the banner. Fail-open
/// on spill errors — entry protection must never cost a message.
/// `inbox_backed` gates the banner's inbox pointer: true only for
/// entrances that persist the message in an inbox row.
pub(crate) fn cap_external_message(text: &str, inbox_backed: bool) -> String {
    if estimate_text(text) <= DEFAULT_MESSAGE_FULL_TOKENS {
        return text.to_owned();
    }
    match spill::spill_content(&message_spill_root(), &MESSAGE_SPILL, text) {
        Ok(path) => cut_message(text, &path, inbox_backed),
        Err(e) => {
            tracing::warn!("message spill failed, recording untruncated: {e:#}");
            text.to_owned()
        }
    }
}

/// Cut an oversized external message down under
/// DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS (mirroring the tool-result cap so
/// both faces share the wedge math) and append the message banner: what was
/// kept, where the full original lives, and the resend-in-parts advice.
/// Inbox-backed entrances add an inbox-copy pointer; the prompt-channel
/// drain keeps it off. The drain sees bare lines without producer context:
/// its rowless producers (notices, kicks, parked interrupts) are short
/// texts that never reach the cap, and its heavy producer (peer
/// injections) does have an inbox row — so off can only under-claim,
/// never over-claim.
fn cut_message(text: &str, spill_path: &std::path::Path, inbox_backed: bool) -> String {
    let full_est = estimate_text(text);
    let total = text.chars().count();
    let chars_cap = DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS * total / full_est;
    let parts = converge_under_cap(text, chars_cap, DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS);
    let inbox_note = if inbox_backed {
        "; the full text is also in your inbox (kallip inbox read)"
    } else {
        ""
    };
    format!(
        "{}\n[... message truncated: kept first {} and last {} of {total} chars (~{} of ~{full_est} estimated tokens). Full original saved at {} (a temporary file — archive it yourself if you need it long-term){inbox_note}. If you need the rest, ask the sender to resend the content in smaller parts ...]",
        parts.2,
        parts.0,
        parts.1,
        estimate_text(&parts.2),
        spill_path.display(),
    )
}

/// Cleanup of spill files belongs to the system /tmp mechanisms; the agent
/// runtime does not duplicate it in-process.
pub async fn agent_task(
    mut ctx: AgentContext,
    initial_prompt: Option<String>,
    mut prompt_rx: tokio::sync::mpsc::Receiver<String>,
    agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
) {
    // Pre-loop compaction: a restored agent whose context is over budget is
    // compacted before the first round. The wrapper charges a failure and warns;
    // what it records waits for the first round's gate (a stop the gate fails
    // on, a backoff the gate honors), so this execution emits no error event
    // for a stop it has not taken. The wake restarts the count, not those
    // records (see `restart`).
    crate::context::compact_pre_loop(&mut ctx).await;

    if let Some(p) = initial_prompt {
        if p.is_empty() {
            // No round runs for an empty prompt, and this execution ends here: its
            // context goes with the task's future, taking this execution's ledger
            // with it. That ledger is the only holder of a stop the pre-loop parked,
            // so no gate ever takes that stop. What the pre-loop's failure wrote
            // down outlives the execution because its holders do, whichever of them
            // the verdict filled: the shared context store (retry records), the
            // pacing table, and the agent's own history (a stop's line).
            return;
        }
        ctx.record_turn(vec![Message::user(&p)]).await;
        let disposition =
            run_and_report(&mut ctx, &agent_tx, &mut prompt_rx, AttemptOrigin::External).await;
        if disposition == WakeDisposition::Terminated {
            return;
        }
    }

    loop {
        tokio::select! {
            input = prompt_rx.recv() => {
                // Declared race window (accepted, microseconds): this arm has
                // no parked guard, so a busy notice enqueued just before the
                // recipient parks is presented here and runs one round while
                // parked. Nothing is lost: the message body already sits in
                // the inbox and the notice is only a visibility hint. The
                // sender side of this window is documented in delivery.rs.
                match input {
                    Some(text) => {
                        clear_transient_retry(&ctx);
                        clear_wait_timer(&ctx);
                        clear_pacing_wake(&ctx);
                        ctx.record_message_turn(&text).await;
                        let disposition = run_and_report(
                            &mut ctx,
                            &agent_tx,
                            &mut prompt_rx,
                            AttemptOrigin::External,
                        )
                        .await;
                        if disposition == WakeDisposition::Terminated {
                            break;
                        }
                        // Post-round inbox drain: a kick round un-parks the
                        // agent, so messages that arrived while parked (or
                        // during the round) are pulled now instead of waiting
                        // for the next notify — the parked guard consumes
                        // notify permits, so notify alone never delivers them.
                        // pull_undelivered is an atomic drain; a racing notify
                        // arm simply finds nothing left to pull. Gated on the wake
                        // having run a round: a refused wake delivers nothing, so
                        // its buffered message waits for a wake that can serve it.
                        if disposition == WakeDisposition::Ran
                            && let Some(ref puller) = ctx.message_puller
                            && let Some(msg) = puller.pull_undelivered().await
                        {
                            ctx.record_message_turn(&msg).await;
                            let disposition = run_and_report(
                                &mut ctx,
                                &agent_tx,
                                &mut prompt_rx,
                                AttemptOrigin::External,
                            )
                            .await;
                            if disposition == WakeDisposition::Terminated {
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
            // Lifecycle cancel (remove / tagma shutdown): terminate the task.
            // Per-agent interrupt never reaches here — it cancels only the current
            // round token inside `run_and_report`, not the lifecycle token.
            _ = ctx.cancel.cancelled() => {
                tracing::info!("agent task: lifecycle cancel, persisting and exiting");
                terminate_cancelled(&ctx, &agent_tx).await;
                break;
            }
            _ = ctx.notify.notified() => {
                // Three producers share this Notify: inbox message delivery,
                // approval notifications, and the profile-apply handler (which
                // writes a pending-reset cell then calls notify_one). Because
                // Notify coalesces permits (at most 1 stored), a single wake may
                // carry work from multiple producers — evaluate ALL sequentially.
                let has_reset = ctx
                    .pending_profile_reset
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
                if has_reset {
                    apply_pending_profile_reset(&mut ctx);
                }

                // Parked guard (design guard matrix: notify events buffer,
                // they do not wake a parked agent). The delivery gate's kick
                // turn (auto-wake on message arrival) is the sole exit from
                // Parked; a bare notify must not run a round the agent never
                // consented to (the kick turn tells the agent why it parked;
                // the message itself waits in the inbox for the prompt arm's
                // post-round drain).
                // The reset cell was applied above — memory-only, no round
                // runs — so a later kick runs on the fresh client.
                if matches!(
                    *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()),
                    LifecycleState::Parked { .. }
                ) {
                    continue;
                }

                let mut should_run = false;

                // Pull undelivered inbox messages (after reset so the round
                // uses the fresh client). Drains ALL undelivered in one atomic
                // call to survive notify coalescing.
                if let Some(ref puller) = ctx.message_puller
                    && let Some(msg) = puller.pull_undelivered().await
                {
                    ctx.record_message_turn(&msg).await;
                    should_run = true;
                }

                // Approval notifications.
                if ctx.approvals.lock().await.has_notifications() {
                    should_run = true;
                }

                if should_run {
                    clear_transient_retry(&ctx);
                    clear_wait_timer(&ctx);
                    clear_pacing_wake(&ctx);
                    let disposition = run_and_report(
                        &mut ctx,
                        &agent_tx,
                        &mut prompt_rx,
                        AttemptOrigin::External,
                    )
                    .await;
                    if disposition == WakeDisposition::Terminated {
                        break;
                    }
                }
            }
            // Timed transient retry: the spawned sleep from `schedule_transient_retry`
            // fired. The guard is authoritative — a stored permit is only honored when
            // the armed deadline has actually passed, so a wake that raced a hard-park
            // or a different arm is a no-op.
            _ = ctx.retry_notify.notified() => {
                if transient_retry_due(&ctx) {
                    clear_transient_retry(&ctx);
                    let disposition = run_and_report(
                        &mut ctx,
                        &agent_tx,
                        &mut prompt_rx,
                        AttemptOrigin::System,
                    )
                    .await;
                    if disposition == WakeDisposition::Terminated {
                        break;
                    }
                }
            }
            // Wait timer: the fuse from `break(wait)` (or a budget-blocked
            // re-arm) elapsed. Guard-authoritative, mirroring the retry arm.
            // Unlike Retrying's automatic re-run, the wake is an injected
            // [system] turn — the agent decides what to do next.
            _ = ctx.wait_notify.notified() => {
                if wait_timer_due(&ctx) {
                    let armed_secs = ctx.wait_armed_secs;
                    clear_wait_timer(&ctx);
                    // A budget-probe wake (the fuse the budget gate itself
                    // re-armed) skips the injection: the round gate still
                    // blocks, so the [system] turn could never reach a
                    // model — it would only accumulate context noise on
                    // every probe cycle. Real wait wakes inject as usual.
                    if !ctx.token_budget.is_exceeded() {
                        ctx.record_turn(vec![Message::user(wait_elapsed_text(armed_secs))])
                            .await;
                    }
                    let disposition = run_and_report(
                        &mut ctx,
                        &agent_tx,
                        &mut prompt_rx,
                        AttemptOrigin::System,
                    )
                    .await;
                    if disposition == WakeDisposition::Terminated {
                        break;
                    }
                }
            }
            // Pacing wake: the window a quota park was waiting out has ended.
            // Guard-authoritative like the other timer arms. The round starts
            // fresh, so its episode claims the window rather than obeying the
            // record this wake was armed for.
            _ = ctx.pacing_notify.notified() => {
                if pacing_wake_due(&ctx) {
                    clear_pacing_wake(&ctx);
                    let disposition = run_and_report(
                        &mut ctx,
                        &agent_tx,
                        &mut prompt_rx,
                        AttemptOrigin::System,
                    )
                    .await;
                    if disposition == WakeDisposition::Terminated {
                        break;
                    }
                }
            }
        }
    }
}

/// Persist context + approval state and emit the terminal [`AgentEvent::Cancelled`].
///
/// The single exit path for a lifecycle cancel (remove / tagma shutdown), shared by the
/// outer-loop cancel arm (idle) and `run_and_report`'s mid-round lifecycle-cancel branch.
async fn terminate_cancelled(ctx: &AgentContext, agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>) {
    ctx.persist().await;
    agent_tx.send(AgentEvent::Cancelled).await.ok();
}

/// Drain the pending profile-reset cell (if set by the tagma's apply handler)
/// and rebuild the failover state + client. Called at two sites: once at the
/// top of each `run_and_report` wake-up, and once in the notify arm when a
/// pending reset is detected. On error, logs and continues on prior config —
/// the cell is cleared regardless so a bad reset does not retry forever.
fn apply_pending_profile_reset(ctx: &mut AgentContext) {
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
            tracing::info!("agent profile reset applied");
        }
        Err(e) => {
            tracing::error!("profile reset failed, continuing on prior config: {e:#}");
        }
    }
}

/// How the wake ended, as the outer loop needs to read it: `Ran` means the
/// wake passed every pre-loop gate and entered the round loop, `NotRun` means a
/// pre-loop gate refused it (today only the wake modality gate), and
/// `Terminated` means the run must end (a lifecycle cancel mid-round).
///
/// The split matters because a round's aftermath is not free: the post-round
/// inbox drain delivers buffered messages into a round, so a wake that never ran
/// one must not drain.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeDisposition {
    /// The wake entered the round loop (whatever its rounds then decided).
    Ran,
    /// A pre-loop gate refused the wake: no round started.
    NotRun,
    /// A lifecycle cancel ended the run: the task terminates.
    Terminated,
}

/// Run agent rounds for one external wake and send results via channel.
///
/// Owns the heartbeat loop: a bare-assistant round (no `break`, no tool calls) no
/// longer terminates the run — the harness records the assistant turn, injects a
/// heartbeat prompt, and re-enters the round loop. The no-progress guardrail
/// (`config.max_heartbeat_rounds`) force-idles after a bounded storm. Only `break`
/// (or a non-deliberate park reason) returns.
///
/// Owns the round-token lifecycle per iteration: mints a fresh child of the
/// lifecycle token, publishes it into `ctx.round_cancel` (so `interrupt_agent` can
/// reach it), runs the round, then clears the slot. It returns
/// [`WakeDisposition::Terminated`] only on a lifecycle cancel (the task should
/// terminate); every other outcome keeps the task alive.
pub async fn run_and_report(
    ctx: &mut AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
    origin: AttemptOrigin,
) -> WakeDisposition {
    // The wake that caused this execution decides how its first attempt reads:
    // an external one is served once even against a marked endpoint, a timer one
    // is not. Taken by the first pre-send reading below.
    ctx.attempt_origin = origin;
    // An external wake restarts the compaction count: a prompt from outside is new
    // work, so the failure count and its deadline start over with it. What the last
    // verdict left behind — an armed backoff, a stop waiting for a gate — stays,
    // because both answer for the context a round would send, not for the count.
    if origin == AttemptOrigin::External {
        ctx.compaction.restart(timefmt::now_epoch());
    }
    // Drain any pending profile reset before starting the round loop.
    apply_pending_profile_reset(ctx);
    // This entry owns the lifecycle move: the wake is served from here on, so
    // the agent is Running for its span and a refusal below parks it over the
    // table's legal Running -> Parked edge; no other state enters Parked.
    // The entry is reached only from a non-Running state, and the move sits
    // after attempt_origin is set above, so the Running log line names this wake.
    ctx.settle(LifecycleState::Running, "wake entry");
    // Wake modality gate: a restored context whose assembled content needs
    // modalities the bound set cannot serve must not start a round — the
    // model would silently never see the referenced content. Surface one
    // actionable error and park; a rebind (profile apply) clears the gate
    // on the next wake. The requirement is computed from what the store
    // actually holds (window + pins), never from a history rescan.
    let required = {
        let store = ctx.store.lock().await;
        crate::context::required_modalities(&store)
    };
    if let Err(blocked) = ctx.failover.set().ensure_supports(&required) {
        tracing::warn!(error = %blocked, "wake blocked by modality gate");
        agent_tx
            .send(AgentEvent::Error(blocked.to_string()))
            .await
            .ok();
        ctx.settle(
            LifecycleState::Parked {
                reason: ParkedReason::FatalError {
                    message: blocked.to_string(),
                },
                at: std::time::Instant::now(),
            },
            "modality gate",
        );
        return WakeDisposition::NotRun;
    }
    let mut no_progress: u32 = 0;
    loop {
        let round = RoundToken::new(&ctx.cancel);
        // Publish the round token for the duration of this round so `interrupt_agent`
        // can cancel it. `Some` only while the round is in flight.
        *ctx.round_cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(round.clone());

        agent_tx.send(AgentEvent::Busy).await.ok();
        let result = runner::run_agent_rounds(ctx, agent_tx, prompt_rx, round.handle()).await;

        // Always clear the slot: a stale token cancelled by a later interrupt would be a
        // no-op (nobody selects on it), but clearing keeps the invariant tight.
        *ctx.round_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;

        match result {
            Ok(runner::RoundOutcome::Break(until)) => {
                // Deliberate yield: park as Idle (`until:"idle"`) or Waiting
                // (armed wake timer, the default). Any tool calls preceding
                // `break` were already recorded inside the round loop.
                ctx.transient_fails = 0;
                match until {
                    runner::BreakUntil::Idle => {
                        ctx.settle(LifecycleState::Idle, "break(idle)");
                        agent_tx.send(AgentEvent::Idle).await.ok();
                    }
                    runner::BreakUntil::Wait { timeout_secs } => {
                        let deadline = arm_wait_timer(ctx, timeout_secs);
                        ctx.settle(
                            LifecycleState::Waiting {
                                until: deadline.into_std(),
                            },
                            "break(wait)",
                        );
                        agent_tx
                            .send(AgentEvent::Waiting { timeout_secs })
                            .await
                            .ok();
                    }
                }
                return WakeDisposition::Ran;
            }
            Ok(runner::RoundOutcome::BareAssistant { content, reasoning }) => {
                // No `break`, no tool calls — heartbeat and continue. Reset the
                // transient-retry counter (a real response is progress).
                ctx.transient_fails = 0;
                no_progress += 1;
                if no_progress > ctx.config.max_heartbeat_rounds {
                    // Self-monologue guardrail: force-idle instead of looping forever.
                    ctx.settle(LifecycleState::Idle, "heartbeat limit");
                    agent_tx.send(AgentEvent::Idle).await.ok();
                    return WakeDisposition::Ran;
                }
                ctx.record_turn(vec![assistant_message(
                    Some(content),
                    Vec::new(),
                    &reasoning,
                )])
                .await;
                ctx.record_turn(vec![Message::user(HEARTBEAT_TEXT)]).await;
                continue;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::Cancelled)) => {
                ctx.transient_fails = 0;
                match CancelKind::classify(&ctx.cancel) {
                    // Lifecycle cancel propagated to the round token → terminate.
                    CancelKind::Lifecycle => {
                        tracing::info!(
                            "agent task: lifecycle cancel mid-round, persisting and exiting"
                        );
                        terminate_cancelled(ctx, agent_tx).await;
                        return WakeDisposition::Terminated;
                    }
                    // Only the round token was cancelled (interrupt) → keep living.
                    CancelKind::Interrupt => {
                        agent_tx.send(AgentEvent::Interrupted).await.ok();
                        ctx.settle(LifecycleState::Idle, "interrupt");
                        return WakeDisposition::Ran;
                    }
                }
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::FailoverChainExhausted {
                reason,
                detail,
                ..
            })) => {
                // Transient: the whole chain is down. Schedule a timed retry
                // (bounded by `max_transient_retries`); the outer loop's retry
                // arm re-enters after the backoff. On the last attempt,
                // hard-park and surface so the operator can reconfigure
                // failover. The arming rides the terminal event's payload so
                // the bridge can mark RETRYING (not PARKED) without flicker.
                ctx.transient_fails = ctx.transient_fails.saturating_add(1);
                let armed = ctx.transient_fails <= ctx.config.max_transient_retries;
                let retry_deadline = armed.then(|| schedule_transient_retry(ctx));
                agent_tx
                    .send(AgentEvent::FailoverChainExhausted {
                        reason,
                        detail,
                        transient_retry: retry_deadline.map(|(_, delay_secs)| TransientRetryInfo {
                            attempt: ctx.transient_fails,
                            max_attempts: ctx.config.max_transient_retries,
                            retry_in_secs: delay_secs,
                        }),
                    })
                    .await
                    .ok();
                match retry_deadline {
                    Some((deadline, _)) => {
                        ctx.settle(
                            LifecycleState::Retrying {
                                attempt: ctx.transient_fails,
                                max_attempts: ctx.config.max_transient_retries,
                                retry_at: deadline.into_std(),
                            },
                            "transient retry armed",
                        );
                    }
                    None => {
                        ctx.settle(
                            LifecycleState::Parked {
                                reason: ParkedReason::TransientRetryExhausted,
                                at: std::time::Instant::now(),
                            },
                            "transient retries exhausted",
                        );
                    }
                }
                return WakeDisposition::Ran;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::QuotaBlocked(info))) => {
                // A quota or payment block: park with the structured reason and
                // wake at the end of the window the fact opened. The park lasts
                // as long as that window and no longer: the round comes back on
                // its own. `transient_fails` is left where it stands (this is not
                // a transient park), and an external trigger still wakes the
                // agent first when it arrives before the window ends.
                agent_tx
                    .send(AgentEvent::QuotaBlocked(info.clone()))
                    .await
                    .ok();
                // The alarm answers to the fact this park wrote, read here: a
                // window another instance recorded paces this wake too.
                let _ = arm_pacing_wake(ctx, &info.profile);
                ctx.settle(
                    LifecycleState::Parked {
                        reason: ParkedReason::QuotaBlocked(info),
                        at: std::time::Instant::now(),
                    },
                    "quota blocked",
                );
                return WakeDisposition::Ran;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::TokenBudgetExceeded {
                consumed,
                budget,
            })) => {
                // Non-fatal: the task stays alive — and per the
                // budget-probe decision it stays *Waiting* with a re-armed
                // timer rather than parking: the next timer wake re-checks
                // the budget before any LLM call (the round gate runs
                // first), so the armed fuse doubles as a zero-cost
                // budget-recovery probe.
                ctx.transient_fails = 0;
                agent_tx
                    .send(AgentEvent::TokenBudgetExceeded { consumed, budget })
                    .await
                    .ok();
                let deadline = arm_wait_timer(ctx, DEFAULT_BREAK_TIMEOUT_SECS);
                ctx.settle(
                    LifecycleState::Waiting {
                        until: deadline.into_std(),
                    },
                    "budget probe re-arm",
                );
                return WakeDisposition::Ran;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::MaxRoundsExceeded)) => {
                ctx.transient_fails = 0;
                ctx.settle(
                    LifecycleState::Parked {
                        reason: ParkedReason::MaxRoundsExceeded,
                        at: std::time::Instant::now(),
                    },
                    "max rounds",
                );
                agent_tx.send(AgentEvent::MaxRoundsExceeded).await.ok();
                return WakeDisposition::Ran;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::Idle)) => {
                // Not produced by the round loop today (only Break/BareAssistant/Park
                // are); parked here for exhaustiveness and forward-compat.
                ctx.transient_fails = 0;
                ctx.settle(LifecycleState::Idle, "park(idle)");
                agent_tx.send(AgentEvent::Idle).await.ok();
                return WakeDisposition::Ran;
            }
            Err(e) => {
                // Permanent (Fatal) error — park and surface; the operator acts.
                ctx.transient_fails = 0;
                let rendered = crate::llm_error::render_error(e.as_ref());
                ctx.settle(
                    LifecycleState::Parked {
                        reason: ParkedReason::FatalError {
                            message: rendered.clone(),
                        },
                        at: std::time::Instant::now(),
                    },
                    "fatal error",
                );
                agent_tx.send(AgentEvent::Error(rendered)).await.ok();
                return WakeDisposition::Ran;
            }
        }
    }
}

/// The synthetic user turn injected after a bare-assistant round, nudging the
/// agent to either make progress, reply, or call `break`. Persisted like an
/// approval notification (bounded by the no-progress guardrail).
const HEARTBEAT_TEXT: &str = "[system] You produced a response with no tool action and did not \
call `break`. If you are done, call `break` with `{\"until\":\"idle\"}`; if you are blocked \
waiting on something, call `break` (the default parks you with a wake timer). Otherwise \
continue your work.";

/// Arm a timed retry for the transient (failover-chain-exhausted) path: back off
/// and notify `retry_at` after the delay. Returns the armed `(deadline, delay)`
/// so the caller can ride the same numbers on the terminal event's
/// `transient_retry` payload and the lifecycle state. The spawned sleep is
/// best-effort and cancel-aware — the outer-loop guard is authoritative, so a
/// wake that fires after a hard-park or a different-arm win is a no-op (the
/// guard re-checks `retry_at`).
fn schedule_transient_retry(ctx: &AgentContext) -> (tokio::time::Instant, f64) {
    // `transient_fails` was just incremented (1-based); backoff uses a 0-based index.
    let attempt = ctx.transient_fails.saturating_sub(1);
    let delay = crate::retry::backoff_delay(&ctx.config.retry_policy, attempt);
    let deadline = tokio::time::Instant::now() + delay;
    *ctx.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(deadline);
    let notify = ctx.retry_notify.clone();
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => notify.notify_one(),
            _ = cancel.cancelled() => {}
        }
    });
    (deadline, delay.as_secs_f64())
}

/// Clear an armed transient-retry deadline. Called on every non-retry wake so a
/// stale stored permit (a sleep that fired after a different arm won the race)
/// cannot trigger a spurious round via the retry arm's guard.
fn clear_transient_retry(ctx: &AgentContext) {
    *ctx.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Arm the wait timer (`break(wait)` or a budget-blocked re-arm): park with a
/// fuse that wakes the outer loop. Mirrors `schedule_transient_retry`'s
/// best-effort, cancel-aware spawn; the `wait_notify` arm's due guard is
/// authoritative. Returns the armed deadline.
fn arm_wait_timer(ctx: &mut AgentContext, timeout_secs: u64) -> tokio::time::Instant {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    *ctx.wait_until.lock().unwrap_or_else(|e| e.into_inner()) = Some(deadline);
    ctx.wait_armed_secs = timeout_secs;
    let notify = ctx.wait_notify.clone();
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => notify.notify_one(),
            _ = cancel.cancelled() => {}
        }
    });
    deadline
}

/// Clear an armed wait timer. Called on every non-wait wake so a stale stored
/// permit cannot fire a spurious turn via the wait arm's guard (same
/// discipline as `clear_transient_retry`).
fn clear_wait_timer(ctx: &AgentContext) {
    *ctx.wait_until.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether the wait timer is genuinely due right now. The wait select arm's
/// authority: a stored permit is only honored when the armed deadline has
/// passed.
fn wait_timer_due(ctx: &AgentContext) -> bool {
    ctx.wait_until
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t <= tokio::time::Instant::now())
        .unwrap_or(false)
}

/// The `[system]` turn injected when the wait timer elapses: the agent decides
/// (keep waiting, finish, or work) — the timer never auto-runs work.
fn wait_elapsed_text(armed_secs: u64) -> String {
    format!(
        "[system] wait timer elapsed (armed {armed_secs}s). Continue waiting with break({{\"until\":\"wait\"}}), \
         finish with break({{\"until\":\"idle\"}}), or work."
    )
}

/// Whether a transient retry is genuinely due right now. The retry select arm's
/// authority: a stored permit is only honored when the armed deadline has passed.
fn transient_retry_due(ctx: &AgentContext) -> bool {
    ctx.retry_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t <= tokio::time::Instant::now())
        .unwrap_or(false)
}
/// Arm the wake that ends a quota park: the earliest window end on the chain,
/// plus jitter. The window is read from the table here, so a fact another
/// instance recorded paces this wake too; a chain with no readable fact falls
/// back to `MIN_RETRY_INTERVAL_SECS` counted from now. The fallback only ever
/// sets the alarm — it writes nothing. Mirrors the other timers' best-effort,
/// cancel-aware spawn; the pacing arm's due guard is authoritative.
fn arm_pacing_wake(ctx: &AgentContext, profile_id: &str) -> tokio::time::Instant {
    let now = timefmt::now_epoch();
    let wake_epoch = pacing_wake_epoch(ctx, profile_id, now);
    let wait = std::time::Duration::from_secs(wake_epoch.saturating_sub(now));
    let deadline = tokio::time::Instant::now() + wait + crate::retry::jitter_now();
    *ctx.pacing_wake_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(deadline);
    let notify = ctx.pacing_notify.clone();
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => notify.notify_one(),
            _ = cancel.cancelled() => {}
        }
    });
    deadline
}

/// The moment a parked quota round may send again: the earliest window end
/// among the profile it stopped on and the candidates ahead of it.
///
/// A wake starts on the profile the round stopped on; the chain only
/// moves forward from there. A skipped marked candidate leaves the
/// index where it was, so a park can sit mid-chain with profiles ahead.
/// Sleeping on the stopping profile alone would outlast a window that
/// opens sooner.
fn pacing_wake_epoch(ctx: &AgentContext, profile_id: &str, now: u64) -> u64 {
    let Some(table) = ctx.profile_pacing.as_deref() else {
        return now.saturating_add(MIN_RETRY_INTERVAL_SECS);
    };
    let mut earliest = table
        .paced_for(profile_id, now)
        .map(|fact| window_end(&fact.block));
    let mut offset = 1;
    while let Some(candidate) = ctx.failover.candidate_profile(offset) {
        if let Some(end) = table
            .paced_for(&candidate.id, now)
            .map(|fact| window_end(&fact.block))
        {
            earliest = Some(earliest.map_or(end, |open| open.min(end)));
        }
        offset += 1;
    }
    earliest.unwrap_or_else(|| now.saturating_add(MIN_RETRY_INTERVAL_SECS))
}

/// Clear an armed pacing wake. Called on every non-pacing wake so a stale
/// stored permit (a sleep that fired after a different arm won the race) cannot
/// start a round nobody asked for, the same discipline `clear_transient_retry`
/// carries.
fn clear_pacing_wake(ctx: &AgentContext) {
    *ctx.pacing_wake_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether the pacing wake is genuinely due right now. The pacing arm's
/// authority: a stored permit is honored only when the armed deadline passed.
fn pacing_wake_due(ctx: &AgentContext) -> bool {
    ctx.pacing_wake_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t <= tokio::time::Instant::now())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message-entry guard cuts an oversized external message to a
    /// banner-topped slice and spills the full original; the spill equality
    /// pins the banner's promise — the path the agent is told to Read really
    /// holds the untouched original.
    #[tokio::test]
    async fn message_guard_cuts_oversized_and_spills_original() {
        let _serial = message_spill_serial().await;
        let spill_dir = tempfile::TempDir::new().unwrap();
        set_message_spill_root(Some(spill_dir.path().join("spill")));
        let ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        // 40K CJK chars ≈ 40K estimated tokens: over the full cap.
        let original = "错".repeat(40_000);
        ctx.record_message_turn(&original).await;

        let store = ctx.store.lock().await;
        let turn = store.turns().back().unwrap();
        let content = turn.messages[0].content().unwrap_or_default();
        assert!(
            content.contains("message truncated"),
            "oversized message must be cut with a banner"
        );
        assert!(content.contains("Full original saved at"));
        assert!(content.contains("resend the content in smaller parts"));
        assert!(
            content.contains("temporary file"),
            "banner must not promise long-term readability"
        );
        assert!(content.chars().count() < original.chars().count());
        // The banner's numbers describe the real keep (same pin style as the
        // tool-result cap banner).
        let banner = &content[content.find("message truncated: kept first").unwrap()..];
        let nums: Vec<usize> = banner
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        assert_eq!(
            nums[2],
            original.chars().count(),
            "banner total must be the message size"
        );
        assert!(nums[0] > nums[1], "head must keep more than the tail");
        assert!(
            nums[0] + nums[1] < nums[2],
            "kept ends must not cover the whole original"
        );
        let spill_path = content
            .split("Full original saved at ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap();
        assert_eq!(std::fs::read_to_string(spill_path).unwrap(), original);
        set_message_spill_root(None);
    }

    /// The banner's inbox pointer follows the inbox_backed flag: an
    /// oversized message cut through an inbox-backed entrance points at
    /// the inbox copy, and the same cut through the prompt-channel drain
    /// does not — both branches spill and cut identically otherwise.
    #[tokio::test]
    async fn cut_banner_inbox_pointer_follows_inbox_backed_flag() {
        let _serial = message_spill_serial().await;
        let spill_dir = tempfile::TempDir::new().unwrap();
        set_message_spill_root(Some(spill_dir.path().join("spill")));
        // 40K CJK chars ≈ 40K estimated tokens: over the full cap.
        let original = "错".repeat(40_000);
        let inbox_backed = cap_external_message(&original, true);
        let drained = cap_external_message(&original, false);
        assert!(
            inbox_backed.contains("kallip inbox read"),
            "inbox-backed entrance must point at the inbox copy"
        );
        assert!(
            !drained.contains("kallip inbox read"),
            "drain-entrance cut must not claim an inbox copy"
        );
        assert!(inbox_backed.contains("message truncated"));
        assert!(drained.contains("message truncated"));
        set_message_spill_root(None);
    }

    /// Fail-open branch: with the spill root pinned to an unwritable path
    /// (a regular file — creating the family dirs under it fails
    /// deterministically), the oversized message is still recorded,
    /// untruncated: entry protection must never cost a message.
    #[tokio::test]
    async fn message_guard_fails_open_when_spill_root_is_unwritable() {
        let _serial = message_spill_serial().await;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("not-a-dir");
        std::fs::write(&root, "file").unwrap();
        set_message_spill_root(Some(root));

        let ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        let original = "错".repeat(40_000);
        ctx.record_message_turn(&original).await;

        let store = ctx.store.lock().await;
        let content = store.turns().back().unwrap().messages[0]
            .content()
            .unwrap()
            .to_owned();
        assert_eq!(
            content, original,
            "fail-open must record the original untruncated"
        );
        set_message_spill_root(None);
    }

    /// A message under the cap passes through byte-for-byte: the guard only
    /// exists for the oversized tail of the distribution.
    #[tokio::test]
    async fn message_guard_passes_ordinary_messages_byte_for_byte() {
        let ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        let text = "a normal sized prompt";
        ctx.record_message_turn(text).await;
        let store = ctx.store.lock().await;
        let content = store.turns().back().unwrap().messages[0]
            .content()
            .unwrap()
            .to_owned();
        assert_eq!(content, text);
    }

    // The initial-prompt exemption is structural, not behavioral: the
    // initial call site inside agent_task stays on raw record_turn, and
    // driving it end-to-end needs a full agent_task harness with a
    // responding endpoint. The guard tests above pin the shared cut/spill
    // path the exemption deliberately avoids.

    /// The persist-failure streak: each failed context persist counts, the
    /// warning threshold is hit on the third consecutive miss, and any
    /// success clears the streak. An agent_dir pointing at a regular file
    /// forces the write failure (root-proof).
    #[tokio::test]
    async fn persist_failure_streak_counts_and_resets() {
        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        // agent_dir pointing at a regular file: every temp-file creation
        // inside it fails (ENOTDIR), which works even when tests run as
        // root (where directory permissions would not).
        let dir = tempfile::TempDir::new().unwrap();
        let not_a_dir = dir.path().join("not-a-dir");
        std::fs::write(&not_a_dir, "file").unwrap();
        ctx.agent_dir = Some(not_a_dir);

        for expected in 1..=PERSIST_FAILURES_WARN_AFTER {
            ctx.persist().await;
            assert_eq!(ctx.persist_failures.load(Ordering::Relaxed), expected);
        }

        ctx.agent_dir = Some(dir.path().to_path_buf());
        ctx.persist().await;
        assert_eq!(ctx.persist_failures.load(Ordering::Relaxed), 0);
    }
    /// The core inference: a round-token cancel is classified as an interrupt (keep
    /// living) unless the lifecycle token was also cancelled — which also verifies
    /// the parent→child propagation that makes the classification work.
    #[test]
    fn cancel_kind_classifies_by_lifecycle_token() {
        // Interrupt: cancel only the round token; lifecycle stays uncancelled.
        let lifecycle = CancellationToken::new();
        let round = RoundToken::new(&lifecycle);
        round.cancel();
        assert!(round.handle().is_cancelled());
        assert!(!lifecycle.is_cancelled());
        assert_eq!(CancelKind::classify(&lifecycle), CancelKind::Interrupt);

        // Lifecycle cancel (remove / shutdown): propagates to the round token (child).
        let lifecycle = CancellationToken::new();
        let round = RoundToken::new(&lifecycle);
        lifecycle.cancel();
        assert!(round.handle().is_cancelled());
        assert_eq!(CancelKind::classify(&lifecycle), CancelKind::Lifecycle);
    }

    /// Guard-authority pin: a stored wait permit is inert once the timer was
    /// cleared (an external event won the race) — `wait_timer_due` is the
    /// wait arm's sole authority, so the stale permit cannot fire a turn.
    #[tokio::test]
    async fn stale_wait_permit_is_inert_after_clear() {
        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        arm_wait_timer(&mut ctx, 600);
        assert!(!wait_timer_due(&ctx), "freshly armed fuse is not yet due");
        ctx.wait_notify.notify_one(); // stale permit: timer fires after the clear
        clear_wait_timer(&ctx);
        assert!(!wait_timer_due(&ctx), "stale permit must be inert");
    }

    /// Re-arming replaces the fuse: the second `break(wait)` wins (latest
    /// deadline + its own armed-secs), mirroring the transient-retry
    /// overwrite semantics.
    #[tokio::test]
    async fn rearming_wait_replaces_deadline_and_secs() {
        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        arm_wait_timer(&mut ctx, 600);
        let first = ctx.wait_until.lock().unwrap().unwrap();
        arm_wait_timer(&mut ctx, 0); // zero-delay fuse: due immediately
        let second = ctx.wait_until.lock().unwrap().unwrap();
        assert!(
            second < first,
            "re-arm must replace, not min/max, the deadline"
        );
        assert_eq!(ctx.wait_armed_secs, 0);
        assert!(wait_timer_due(&ctx));
    }

    /// Integration test: a MessagePuller returning a message drives the notify
    /// arm to record the message as a user turn before any network call.
    #[tokio::test]
    async fn notify_pull_drives_round() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct StubPuller(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl MessagePuller for StubPuller {
            async fn pull_undelivered(&self) -> Option<String> {
                if self.0.swap(false, Ordering::SeqCst) {
                    Some("inbox test message".to_string())
                } else {
                    None
                }
            }
        }

        // Loopback mock: this poll waits on a turn the round writes after it sends.
        let (mut ctx, _server) = crate::test_support::ctx_with_mock(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
            crate::test_support::PLACEHOLDER_KEY_STATUS,
        )
        .await;

        let flag = Arc::new(AtomicBool::new(true));
        ctx.message_puller = Some(Arc::new(StubPuller(flag.clone())));

        let notify = ctx.notify.clone();
        let cancel = ctx.cancel.clone();
        let store = ctx.store.clone();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (_prompt_tx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

        let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

        // Trigger the notify arm.
        notify.notify_one();

        // Poll the store until the message appears (record_turn runs before
        // any network call in run_and_report).
        let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                let has_msg = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
                    m.content()
                        .map(|t| t.contains("inbox test message"))
                        .unwrap_or(false)
                });
                drop(guard);
                if has_msg {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await;

        cancel.cancel();
        let _ = rx.recv().await;
        handle.abort();

        assert!(found.unwrap_or(false), "inbox message should drive a round");
    }

    /// A peer notice that arrives as the outer loop takes the prompt arm is
    /// recorded, and the post-round pull still brings the message body —
    /// the race-window guarantee that a notice is never lost even when it
    /// lands between rounds.
    #[tokio::test]
    async fn prompt_arm_notice_is_followed_by_post_round_body_pull() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct StubPuller(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl MessagePuller for StubPuller {
            async fn pull_undelivered(&self) -> Option<String> {
                if self.0.swap(false, Ordering::SeqCst) {
                    Some("pulled message body".to_string())
                } else {
                    None
                }
            }
        }
        // Loopback mock, as in the prompt twin: this poll waits on the notify turn.
        let (mut ctx, _server) = crate::test_support::ctx_with_mock(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
            crate::test_support::PLACEHOLDER_KEY_STATUS,
        )
        .await;

        let flag = Arc::new(AtomicBool::new(true));
        ctx.message_puller = Some(Arc::new(StubPuller(flag.clone())));

        let cancel = ctx.cancel.clone();
        let store = ctx.store.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (prompt_tx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

        prompt_tx
            .send("peer message from alice is in your inbox (preview: hi)".to_string())
            .await
            .unwrap();

        let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                let mut contents = Vec::new();
                for t in guard.turns().iter() {
                    for m in &t.messages {
                        if let Some(c) = m.content() {
                            contents.push(c.to_owned());
                        }
                    }
                }
                drop(guard);
                let notice = contents
                    .iter()
                    .any(|c| c.contains("peer message from alice"));
                let body = contents.iter().any(|c| c.contains("pulled message body"));
                if notice && body {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await;

        cancel.cancel();
        let _ = rx.recv().await;
        handle.abort();
        assert!(
            found.unwrap_or(false),
            "notice turn must be recorded and the post-round pull must bring the body"
        );
    }
    /// Guard-authority twin for the retry arm (the wait twin is above): a
    /// stored permit is inert once the retry deadline was cleared — a sleep
    /// that fired after an external wake won the race cannot fire a turn.
    /// Together with the wait twin this pins both directions of the
    /// silent-retry-loss suspicion: permits are doorbells only,
    /// the deadline check is the sole authority.
    #[tokio::test]
    async fn stale_retry_permit_is_inert_after_clear() {
        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        ctx.transient_fails = 1;
        let (_deadline, _) = schedule_transient_retry(&ctx);
        assert!(
            !transient_retry_due(&ctx),
            "freshly armed backoff is not yet due"
        );
        ctx.retry_notify.notify_one(); // stale permit: sleep fired after the clear
        clear_transient_retry(&ctx);
        assert!(!transient_retry_due(&ctx), "stale permit must be inert");
    }

    /// Five-wake-set pin: an approval decision (approve/
    /// deny) wakes the parked agent through the shared notify arm and the
    /// notification reaches the next round's context — no prompt needed.
    /// (Inbox: `notify_pull_drives_round` above; prompt: every full-loop
    /// test; duty: `update_duty_on_notifies_agent` at the tagma layer;
    /// profile reset: `apply_pending_profile_reset`'s tagma tests.)
    #[tokio::test]
    async fn approval_decision_wakes_agent_and_drives_round() {
        // Loopback mock, as in the inbox twin: this poll waits on the approval notice.
        let (ctx, _server) = crate::test_support::ctx_with_mock(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
            crate::test_support::PLACEHOLDER_KEY_STATUS,
        )
        .await;

        // Commit + deny one approval so a notification is pending.
        let id = {
            let mut q = ctx.approvals.lock().await;
            let id = q.enqueue("bash_exec", "{}", None);
            q.commit(&id, "test justification").unwrap();
            q.deny(&id, "test deny").unwrap();
            assert!(q.has_notifications());
            id
        };

        let notify = ctx.notify.clone();
        let cancel = ctx.cancel.clone();
        let store = ctx.store.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (_ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

        notify.notify_one();

        // The notification is injected as a user turn before any network
        // call (same ordering as the inbox twin).
        let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                let has = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
                    m.content()
                        .map(|t| t.contains(&format!("Approval {id} has been denied")))
                        .unwrap_or(false)
                });
                drop(guard);
                if has {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await;

        cancel.cancel();
        let _ = rx.recv().await;
        handle.abort();
        assert!(
            found.unwrap_or(false),
            "an approval decision must wake the agent and reach the round context"
        );
    }

    /// Guard-matrix pin: a bare notify must NOT wake a parked agent — only
    /// the delivery gate's kick turn (auto-wake on message arrival) exits
    /// Parked. A notify-class event arriving while parked stays buffered;
    /// no round runs.
    #[tokio::test]
    async fn notify_does_not_wake_parked_agent() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct StubPuller(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl MessagePuller for StubPuller {
            async fn pull_undelivered(&self) -> Option<String> {
                if self.0.swap(false, Ordering::SeqCst) {
                    Some("message while parked".to_string())
                } else {
                    None
                }
            }
        }

        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()) = LifecycleState::Parked {
            reason: ParkedReason::FatalError {
                message: "test park".to_string(),
            },
            at: std::time::Instant::now(),
        };
        let flag = Arc::new(AtomicBool::new(true));
        ctx.message_puller = Some(Arc::new(StubPuller(flag.clone())));

        let notify = ctx.notify.clone();
        let cancel = ctx.cancel.clone();
        let store = ctx.store.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (_ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

        notify.notify_one();
        // Give the (wrong) wake a clear window to misbehave, then assert
        // nothing ran.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let guard = store.lock().await;
        let ran = guard.turns().iter().flat_map(|t| &t.messages).any(|m| {
            m.content()
                .map(|t| t.contains("message while parked"))
                .unwrap_or(false)
        });
        drop(guard);

        cancel.cancel();
        let _ = rx.recv().await;
        handle.abort();
        assert!(
            !ran,
            "a notify wake must not run a round while parked (the kick is the sole exit)"
        );
    }

    /// Delivery-gate twin of the guard pin above: the kick turn (sent by the
    /// delivery gate's auto-wake) exits Parked via the prompt arm, and the
    /// post-round drain then pulls the buffered inbox message into its own
    /// round — the deterministic kick→un-park→drain delivery chain.
    #[tokio::test]
    async fn kick_turn_unparks_then_drains_inbox_message() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct StubPuller(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl MessagePuller for StubPuller {
            async fn pull_undelivered(&self) -> Option<String> {
                if self.0.swap(false, Ordering::SeqCst) {
                    Some("message while parked".to_string())
                } else {
                    None
                }
            }
        }
        // Loopback mock: the drain shows up only if the round finishes in the budget.
        let (mut ctx, _server) = crate::test_support::ctx_with_mock(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
            crate::test_support::PLACEHOLDER_KEY_STATUS,
        )
        .await;

        *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()) = LifecycleState::Parked {
            reason: ParkedReason::FatalError {
                message: "test park".to_string(),
            },
            at: std::time::Instant::now(),
        };
        let flag = Arc::new(AtomicBool::new(true));
        ctx.message_puller = Some(Arc::new(StubPuller(flag.clone())));

        let cancel = ctx.cancel.clone();
        let store = ctx.store.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

        // The kick turn, exactly as the delivery gate's auto-wake sends it.
        ptx.send(
            "[system] you were parked 1s ago: fatal error: test park. Decide whether to retry, adjust, or report."
                .to_string(),
        )
        .await
        .expect("kick send");

        let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                let texts: Vec<String> = guard
                    .turns()
                    .iter()
                    .flat_map(|t| &t.messages)
                    .filter_map(|m| m.content().map(|s| s.to_string()))
                    .collect();
                drop(guard);
                let kick = texts
                    .iter()
                    .position(|t| t.starts_with("[system] you were parked"));
                let msg = texts
                    .iter()
                    .position(|t| t.contains("message while parked"));
                if let (Some(k), Some(m)) = (kick, msg) {
                    return (k, m);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await;

        cancel.cancel();
        let _ = rx.recv().await;
        handle.abort();
        let (kick_at, msg_at) =
            found.expect("kick round must run and the drain must deliver the message");
        assert!(
            kick_at < msg_at,
            "the kick round runs first; the drained message is its own later turn"
        );
    }

    /// A kick wake that the modality gate refuses never reaches the round loop, so
    /// the post-round inbox drain never runs: the buffered message stays buffered
    /// for a wake that can serve it. Draining it here would deliver a message into
    /// a round the wake itself could not start.
    #[tokio::test]
    async fn a_refused_kick_wake_leaves_the_buffered_message_undelivered() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct CountingPuller {
            calls: Arc<AtomicUsize>,
            pending: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl MessagePuller for CountingPuller {
            async fn pull_undelivered(&self) -> Option<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if self.pending.swap(false, Ordering::SeqCst) {
                    Some("message while parked".to_string())
                } else {
                    None
                }
            }
        }

        let (mut ctx, backend) = crate::test_support::ctx_with_unservable_image().await;
        *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()) = LifecycleState::Parked {
            reason: ParkedReason::FatalError {
                message: "test park".to_string(),
            },
            at: std::time::Instant::now(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        ctx.message_puller = Some(Arc::new(CountingPuller {
            calls: calls.clone(),
            pending: Arc::new(AtomicBool::new(true)),
        }));

        let cancel = ctx.cancel.clone();
        let store = ctx.store.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (ptx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let handle = tokio::spawn(agent_task(ctx, None, prompt_rx, tx));

        // The kick turn, exactly as the delivery gate's auto-wake sends it.
        ptx.send(
            "[system] you were parked 1s ago: fatal error: test park. Decide whether to retry, adjust, or report."
                .to_string(),
        )
        .await
        .expect("kick send");

        // Read the channel out to a quiet window: the refused wake emits its
        // verdict and then nothing.
        let mut events = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await
        {
            events.push(event);
        }
        cancel.cancel();
        let _ = rx.recv().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a wake that never ran a round must not drain the inbox"
        );
        let guard = store.lock().await;
        let drained = guard
            .turns()
            .iter()
            .flat_map(|turn| &turn.messages)
            .any(|m| {
                m.content()
                    .is_some_and(|content| content.contains("message while parked"))
            });
        drop(guard);
        assert!(
            !drained,
            "the buffered message waits for a wake that can serve it"
        );

        handle.abort();

        match events.as_slice() {
            [AgentEvent::Error(text)] => assert!(
                text.contains("cannot serve the restored context"),
                "the refused kick wake surfaces the gate's verdict: {text}"
            ),
            other => panic!("expected exactly one Error event, got {other:?}"),
        }
        assert!(
            backend.take_requests().is_empty(),
            "the refused wake never reaches the model"
        );
    }

    /// A rerun of the prompt is a new execution: the episode ledger restarts, so
    /// the rerun sends. Carrying the spent ledger over would turn every send into a
    /// failover and park again without a single request.
    #[tokio::test]
    async fn a_rerun_starts_a_fresh_episode_and_sends() {
        use crate::test_support::{
            MapSource, RecordingBackend, ctx_from_source, profile, user_msg,
        };
        use just_llm_client::{BackendError, LlmBackend, TransportError};

        fn endpoint_down() -> BackendError {
            BackendError::provider(
                "recording",
                TransportError::HttpStatus {
                    status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    body: "down".to_owned(),
                },
            )
        }

        let backend = RecordingBackend::new();
        // Execution 1: both sends of the episode fail transiently, so the episode is
        // spent by the time the chain exhausts and the agent parks for a retry.
        backend.queue_error(endpoint_down());
        backend.queue_error(endpoint_down());
        // Execution 2: the endpoint answers (plus the heartbeat rounds that follow).
        for _ in 0..5 {
            backend.queue_stream("resp-1");
        }

        let source = Arc::new(MapSource(std::collections::HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let policy = crate::retry::RetryPolicy {
            max_retries: 1,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
            retry_timeout: std::time::Duration::from_secs(10),
        };
        let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;
        ctx.record_turn(vec![user_msg("q1")]).await;

        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

        assert_ne!(
            run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
            WakeDisposition::Terminated,
            "a transient exhaustion parks the round instead of terminating the task"
        );
        assert_eq!(
            backend.take_requests().len(),
            2,
            "the episode spent its two sends"
        );
        assert_eq!(
            ctx.episode.attempts(),
            2,
            "the parked agent keeps the spent ledger"
        );

        assert_ne!(
            run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
            WakeDisposition::Terminated
        );
        let rerun = backend.take_requests();
        assert!(
            !rerun.is_empty(),
            "the rerun sends: its episode budget is fresh"
        );
        assert_eq!(rerun[0].model, "a-model");
    }

    /// `start_episode` replaces the ledger: sends charged in the previous episode do
    /// not carry over, and the deadline restarts with the new one.
    #[tokio::test]
    async fn start_episode_replaces_the_ledger() {
        let mut ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("a", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        ctx.episode = crate::retry::Episode::new(kallipai_common::timefmt::now_epoch() - 100);
        ctx.episode = ctx.episode.after_attempt();
        ctx.episode = ctx.episode.after_attempt();

        ctx.start_episode();

        assert_eq!(ctx.episode.attempts(), 0, "the send count restarts");
        assert_eq!(
            ctx.episode.elapsed(kallipai_common::timefmt::now_epoch()),
            std::time::Duration::ZERO,
            "the deadline restarts with the episode"
        );
    }

    /// A quota rejection ends the round as a structured park instead of arming a
    /// transient re-run: the endpoint is asked once, the agent parks with the
    /// payload, and the wake it leaves answers to the window the fact opened.
    #[tokio::test]
    async fn a_quota_block_parks_and_arms_a_wake_at_the_window_end() {
        use crate::test_support::{
            MapSource, RecordingBackend, ctx_from_source, profile, user_msg,
        };
        use just_llm_client::{BackendError, LlmBackend, TransportError};

        let backend = RecordingBackend::new();
        // Four errors are queued: a re-run would spend one, so the request count
        // pins the no-re-run claim.
        for _ in 0..4 {
            backend.queue_error(BackendError::provider(
                "recording",
                TransportError::HttpStatus {
                    status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                    body: crate::test_support::QUOTA_RESET_BODY.to_owned(),
                },
            ));
        }
        let source = Arc::new(MapSource(std::collections::HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            crate::retry::RetryPolicy::default(),
        )
        .await;
        ctx.record_turn(vec![user_msg("q1")]).await;
        let retry_at = ctx.retry_at.clone();
        let wait_until = ctx.wait_until.clone();
        let pacing_wake_at = ctx.pacing_wake_at.clone();

        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

        assert_ne!(
            run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
            WakeDisposition::Terminated,
            "a quota stop parks the round instead of terminating the task"
        );
        assert_eq!(
            backend.take_requests().len(),
            1,
            "the endpoint is asked exactly once"
        );
        assert_eq!(ctx.transient_fails, 0, "a stop is not a transient failure");
        assert!(
            retry_at.lock().unwrap().is_none(),
            "a stop must leave no retry fuse armed"
        );
        assert!(
            wait_until.lock().unwrap().is_none(),
            "a stop must leave no wait fuse armed"
        );
        assert!(
            pacing_wake_at.lock().unwrap().is_some(),
            "a quota park leaves a wake armed so the round can leave on its own"
        );
        let parked = ctx
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match parked {
            LifecycleState::Parked {
                reason: ParkedReason::QuotaBlocked(info),
                ..
            } => {
                assert_eq!(info.profile, "a");
                assert!(
                    info.detail.contains("429"),
                    "the payload carries the evidence: {info:?}"
                );
            }
            other => panic!("expected a structured quota park, got {other:?}"),
        }
    }

    /// The wake is armed at the window's end, never before it: a fact the table
    /// holds for this profile sets the alarm, and a profile with no readable fact
    /// still gets one — from the kind's own interval counted at now, which only
    /// ever sets the alarm.
    #[tokio::test]
    async fn a_pacing_wake_sits_where_the_window_ends() {
        use crate::test_support::{RecordingPacing, make_ctx, profile};
        use kallipai_common::facts::{Block, BlockKind};

        let now = timefmt::now_epoch();
        let pacing = Arc::new(RecordingPacing::default());
        pacing.seed(
            "test",
            Block {
                kind: BlockKind::Quota,
                until: Some(now + 5),
                detail: "HTTP 429".into(),
                status: Some(429),
                recorded_at: now,
            },
        );
        let mut ctx = make_ctx(vec![profile("test", "ep1", 4096)], &["ep1"]).await;
        ctx.profile_pacing = Some(pacing);

        let armed = arm_pacing_wake(&ctx, "test");
        assert!(!pacing_wake_due(&ctx), "a running window is not a wake");
        let remaining = armed.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            remaining >= std::time::Duration::from_secs(4),
            "the wake may not fire before the window ends: {remaining:?}"
        );

        // No readable fact: the wake still arms, so a park is never a dead end
        // waiting on an outside trigger that may never come.
        let bare = make_ctx(vec![profile("test", "ep1", 4096)], &["ep1"]).await;
        let fallback = arm_pacing_wake(&bare, "test");
        let remaining = fallback.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            remaining > std::time::Duration::from_secs(1),
            "the fallback is a real wait, not an immediate re-run: {remaining:?}"
        );
    }

    /// The alarm answers to the chain, not to the one profile the round stopped
    /// on: with every profile inside a window, the shortest one sets the wake, so
    /// a candidate whose window opens sooner is never slept through.
    #[tokio::test]
    async fn a_pacing_wake_sits_where_the_earliest_window_ends() {
        use crate::test_support::{RecordingPacing, make_ctx, profile};
        use kallipai_common::facts::{Block, BlockKind};

        let now = timefmt::now_epoch();
        let pacing = Arc::new(RecordingPacing::default());
        let window = |until: u64| Block {
            kind: BlockKind::Quota,
            until: Some(until),
            detail: "HTTP 429".into(),
            status: Some(429),
            recorded_at: now,
        };
        pacing.seed("slow", window(now + 600));
        pacing.seed("quick", window(now + 5));
        let mut ctx = make_ctx(
            vec![profile("slow", "ep1", 4096), profile("quick", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        ctx.profile_pacing = Some(pacing);

        let armed = arm_pacing_wake(&ctx, "slow");
        let remaining = armed.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            remaining < std::time::Duration::from_secs(60),
            "the wake answers to the shorter window on the chain: {remaining:?}"
        );
        assert!(
            remaining >= std::time::Duration::from_secs(4),
            "and never fires before the window it waits out ends: {remaining:?}"
        );
    }

    /// The pacing arm's due guard is the authority: a permit a sleep stored after
    /// the wake was cleared cannot start a round nobody asked for.
    #[tokio::test]
    async fn stale_pacing_permit_is_inert_after_clear() {
        let ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;
        let _ = arm_pacing_wake(&ctx, "test");
        assert!(
            !pacing_wake_due(&ctx),
            "a freshly armed wake is not yet due"
        );
        ctx.pacing_notify.notify_one(); // stale permit: the sleep fired after the clear
        clear_pacing_wake(&ctx);
        assert!(!pacing_wake_due(&ctx), "a stale permit must be inert");
    }

    /// A parked quota round comes back on its own: the wake fires at the window's
    /// end with nothing external arriving, and the round it starts claims the
    /// reopened window and sends. A round resuming the parked episode could not:
    /// its episode already holds the window, so the claim would read as spent.
    #[tokio::test]
    async fn a_paced_park_wakes_itself_and_serves_the_round() {
        use crate::test_support::{
            MapSource, RecordingBackend, RecordingPacing, ctx_from_source, profile,
        };
        use just_llm_client::{BackendError, LlmBackend, TransportError};

        let backend = RecordingBackend::new();
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: r#"{"error":"slow down","retry_after":1}"#.to_owned(),
            },
        ));
        backend.queue_stream("r1");
        let source = Arc::new(MapSource(std::collections::HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            crate::retry::RetryPolicy::default(),
        )
        .await;
        ctx.profile_pacing = Some(Arc::new(RecordingPacing::default()));

        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (_prompt_tx, prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let handle = tokio::spawn(agent_task(ctx, Some("q1".to_owned()), prompt_rx, tx));

        let parked = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = rx.recv().await {
                if matches!(event, AgentEvent::QuotaBlocked(_)) {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(parked.ok(), Some(true), "the round parks on the rejection");

        // Nothing else can start the next round — no prompt, no notify, no
        // approval — so the send this waits for is the wake's own doing.
        backend.take_requests();
        let resumed = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if !backend.take_requests().is_empty() {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
        assert_eq!(
            resumed.ok(),
            Some(true),
            "the parked round sends again with nothing external arriving"
        );
        handle.abort();
    }
    /// A rejected move trips the legality check and leaves the state where it
    /// was: the state is applied before it is recorded, so a refused call
    /// cannot advance the agent's lifecycle.
    /// The refusal rides a debug assertion, so this test is gated to debug
    /// builds: a release build writes the new state unchecked.
    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn an_illegal_move_is_rejected_and_leaves_the_state_untouched() {
        let ctx = crate::test_support::make_ctx(
            vec![crate::test_support::profile("test", "ep1", 4096)],
            &["ep1"],
        )
        .await;

        // A fresh context sits in idle, whose only edge leads to running: asking
        // for a wait trips the legality assertion.
        let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.settle(
                LifecycleState::Waiting {
                    until: std::time::Instant::now(),
                },
                "illegal move",
            );
        }));
        assert!(rejected.is_err(), "the illegal move must be rejected");
        assert_eq!(
            *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()),
            LifecycleState::Idle,
            "the rejected wait must leave the state untouched"
        );

        // The parked arm is behind the same check: idle has no edge into
        // parked either.
        let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.settle(
                LifecycleState::Parked {
                    reason: ParkedReason::MaxRoundsExceeded,
                    at: std::time::Instant::now(),
                },
                "illegal move",
            );
        }));
        assert!(rejected.is_err(), "the illegal move must be rejected");
        assert_eq!(
            *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()),
            LifecycleState::Idle,
            "the rejected park must leave the state untouched"
        );

        // A legal move still lands: the refusals above did not wedge the context.
        ctx.settle(LifecycleState::Running, "wake entry");
        assert_eq!(
            *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()),
            LifecycleState::Running,
            "the next legal move lands"
        );
    }

    /// A repeated response shape stops the round instead of arming a transient
    /// re-run: the first shape failure retries, the second stops, and the agent
    /// parks with the shape-class message rather than a retry-exhaustion one.
    #[tokio::test]
    async fn a_mid_stream_shape_repeat_parks_without_arming_a_rerun() {
        use crate::test_support::{
            MapSource, RecordingBackend, ctx_from_source, profile, user_msg,
        };
        use just_llm_client::{LlmBackend, TransportError};

        let backend = RecordingBackend::new();
        // Four drops are queued: a re-run would spend one, so the request count
        // pins the no-re-run claim.
        for _ in 0..4 {
            backend.queue_dropped_stream(
                vec![just_llm_client::types::generation::GenerationEvent::Text {
                    delta: "partial".to_owned(),
                }],
                TransportError::InvalidResponse(
                    "stream closed before the terminal event".to_owned(),
                ),
            );
        }
        let source = Arc::new(MapSource(std::collections::HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let policy = crate::retry::RetryPolicy {
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            ..crate::retry::RetryPolicy::default()
        };
        let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;
        ctx.record_turn(vec![user_msg("q1")]).await;
        let retry_at = ctx.retry_at.clone();
        let wait_until = ctx.wait_until.clone();

        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

        assert_ne!(
            run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
            WakeDisposition::Terminated,
            "a shape stop parks the round instead of terminating the task"
        );
        assert_eq!(
            backend.take_requests().len(),
            2,
            "the first shape failure retries, the repeat stops"
        );
        assert_eq!(ctx.transient_fails, 0, "a stop is not a transient failure");
        assert!(
            retry_at.lock().unwrap().is_none(),
            "a stop must leave no retry fuse armed"
        );
        assert!(
            wait_until.lock().unwrap().is_none(),
            "a stop must leave no wait fuse armed"
        );
        let parked = ctx
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match parked {
            LifecycleState::Parked {
                reason: ParkedReason::FatalError { message },
                ..
            } => assert!(
                message.contains("the response shape failed twice in this episode"),
                "{message}"
            ),
            other => panic!("expected a FatalError park, got {other:?}"),
        }
    }

    /// A bare-assistant round records what the model reasoned along with what it
    /// said: the recorded turn carries the model's own thinking, not just its text.
    #[tokio::test]
    async fn a_bare_assistant_round_records_the_models_reasoning() {
        use crate::test_support::{
            MapSource, RecordingBackend, ctx_from_source, profile, user_msg,
        };
        use just_llm_client::LlmBackend;

        let backend = RecordingBackend::new();
        let source = Arc::new(MapSource(std::collections::HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            crate::retry::RetryPolicy::default(),
        )
        .await;
        // One answer per round: the heartbeat limit force-idles the wake, so the
        // queue never runs dry mid-test.
        for _ in 0..=ctx.config.max_heartbeat_rounds {
            backend.queue_stream_with_reasoning("r1", "weighing it");
        }
        ctx.record_turn(vec![user_msg("q1")]).await;
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

        let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

        let store = ctx.store.lock().await;
        let answer = store
            .turns()
            .iter()
            .flat_map(|t| &t.messages)
            .find(|m| m.role() == "assistant")
            .expect("the round records the assistant turn");
        assert_eq!(
            answer.reasoning().and_then(|r| r.text.as_deref()),
            Some("weighing it"),
            "the recorded turn carries the model's reasoning"
        );
    }

    /// The recorded reasoning reaches the next request itself: a stateless
    /// backend receives the full history, so the second request must carry the
    /// assistant turn with the reasoning the stream sent.
    #[tokio::test]
    async fn a_recorded_turn_carries_the_reasoning_into_the_next_request() {
        use crate::test_support::{
            MapSource, RecordingBackend, ctx_from_source, profile, user_msg,
        };
        use just_llm_client::LlmBackend;

        let backend = RecordingBackend::new_stateless();
        let source = Arc::new(MapSource(std::collections::HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            crate::retry::RetryPolicy::default(),
        )
        .await;
        for _ in 0..=ctx.config.max_heartbeat_rounds {
            backend.queue_stream_with_reasoning("r1", "weighing it");
        }
        ctx.record_turn(vec![user_msg("q1")]).await;
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

        let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

        let requests = backend.take_requests();
        assert!(
            requests.len() >= 2,
            "the wake reaches a second round: {}",
            requests.len()
        );
        let carried = requests[1]
            .messages
            .iter()
            .find(|m| m.role() == "assistant")
            .expect("the second request carries the recorded assistant turn");
        assert_eq!(
            carried.reasoning().and_then(|r| r.text.as_deref()),
            Some("weighing it"),
            "the recorded reasoning travels in the next request"
        );
    }

    /// A stop the pre-loop wrapper met outside a round is the verdict the round
    /// fails on: an external wake restarts the count and keeps the stop, so the
    /// gate takes it before sending — one summarizer request, one history line,
    /// and the round ends on the message the wrapper itself reached.
    #[tokio::test]
    async fn a_pre_loop_stop_fails_the_round_after_an_external_wake() {
        // The same setup twice: once for the message the parked verdict carries
        // (the gate called where the wrapper left it), once for the round that
        // reads it after a wake.
        let (mut taken_ctx, taken_server) = stop_pin_ctx().await;
        let prompt_tokens = taken_ctx.store.lock().await.total_estimated_tokens();
        crate::context::compact_pre_loop(&mut taken_ctx).await;
        let action = crate::budget_gate::enforce_pre_call_budget(
            &mut taken_ctx,
            prompt_tokens,
            0,
            &CancellationToken::new(),
        )
        .await;
        let crate::budget_gate::BudgetAction::Fail(error) = action else {
            panic!("the parked stop ends the round");
        };
        let verdict = crate::llm_error::render_error(error.as_ref());
        assert_eq!(
            taken_server
                .received_requests()
                .await
                .unwrap_or_default()
                .len(),
            1,
            "the wrapper's own failure is the only request"
        );

        let (mut ctx, server) = stop_pin_ctx().await;
        let dir = tempfile::tempdir().unwrap();
        ctx.history = Some(HistoryWriter::new(dir.path().to_owned()));
        crate::context::compact_pre_loop(&mut ctx).await;
        assert_eq!(ctx.compaction.attempts(), 1, "the wrapper charged the send");

        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        assert_ne!(
            run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::External).await,
            WakeDisposition::Terminated,
            "a stop parks the round instead of terminating the task"
        );

        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            1,
            "the wake keeps the stop: the summarizer is not asked again"
        );
        assert_eq!(history_lines(dir.path()), 1, "one line for the failure");
        assert_eq!(
            ctx.compaction.attempts(),
            0,
            "the wake restarts the count the wrapper charged"
        );
        assert!(
            !ctx.compaction.is_latched(timefmt::now_epoch()),
            "a stop is taken by the gate, not delayed by a wait"
        );
        let parked = ctx
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match parked {
            LifecycleState::Parked {
                reason: ParkedReason::FatalError { message },
                ..
            } => assert_eq!(
                message, verdict,
                "the round ends on the wrapper's own verdict"
            ),
            other => panic!("expected a FatalError park, got {other:?}"),
        }
    }

    /// A wake blocked by the modality gate parks the agent through the wake
    /// entry's own move: the round never starts, so the state is Running when
    /// the gate refuses it and the park rides a legal table edge. Every state
    /// the entry can be reached from is covered — a refusal from a parked,
    /// waiting, retrying or idle agent must not park over an edge the table
    /// rejects.
    #[tokio::test]
    async fn a_wake_blocked_by_the_modality_gate_parks_from_the_wake_entry() {
        for (from, start) in [
            ("idle", LifecycleState::Idle),
            (
                "waiting",
                LifecycleState::Waiting {
                    until: std::time::Instant::now(),
                },
            ),
            (
                "retrying",
                LifecycleState::Retrying {
                    attempt: 1,
                    max_attempts: 3,
                    retry_at: std::time::Instant::now(),
                },
            ),
            (
                "parked",
                LifecycleState::Parked {
                    reason: ParkedReason::MaxRoundsExceeded,
                    at: std::time::Instant::now(),
                },
            ),
        ] {
            assert_gate_refusal_parks_from(from, start).await;
        }
    }

    /// Drive one gate-refused wake from `start` and assert the whole refusal:
    /// no round runs, the park carries the gate's verdict, exactly one error
    /// event reaches the channel, and the model is never asked.
    async fn assert_gate_refusal_parks_from(from: &str, start: LifecycleState) {
        let (mut ctx, backend) = crate::test_support::ctx_with_unservable_image().await;
        *ctx.lifecycle.lock().unwrap_or_else(|e| e.into_inner()) = start;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

        assert_eq!(
            run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await,
            WakeDisposition::NotRun,
            "{from}: a blocked wake parks instead of starting a round"
        );
        let parked = ctx
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match parked {
            LifecycleState::Parked {
                reason: ParkedReason::FatalError { message },
                ..
            } => assert!(
                message.contains("cannot serve the restored context"),
                "{from}: the park carries the gate's verdict: {message}"
            ),
            other => panic!("{from}: expected a FatalError park, got {other:?}"),
        }
        let events: Vec<AgentEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        match events.as_slice() {
            [AgentEvent::Error(text)] => assert!(
                text.contains("cannot serve the restored context"),
                "{from}: the wake surfaces the refusal to the caller: {text}"
            ),
            other => panic!("{from}: expected exactly one Error event, got {other:?}"),
        }
        assert!(
            backend.take_requests().is_empty(),
            "{from}: the blocked wake never reaches the model"
        );
    }

    /// An external wake restarts the count and keeps what the last verdict left:
    /// the backoff a pre-loop failure armed still holds, so the wake's own gate
    /// calls send without asking the summarizer again.
    #[tokio::test]
    async fn an_external_wake_restarts_the_count_and_keeps_the_armed_backoff() {
        use std::time::Duration;

        const MARKER: &str = "wake-keeps-the-verdict-marker";
        let (source, server) = crate::test_support::mock_source(&["ep1"], 500).await;
        let mut ctx = crate::test_support::ctx_from_source(
            vec![crate::test_support::profile("p1", "ep1", 500_000)],
            source,
            crate::retry::RetryPolicy {
                base_delay: Duration::from_secs(60),
                ..crate::retry::RetryPolicy::default()
            },
        )
        .await;
        crate::test_support::set_context_window(&mut ctx, crate::test_support::FAILURE_PIN_WINDOW)
            .await;
        crate::test_support::seed_over_budget_store(&mut ctx).await;
        ctx.config.context_thresholds = vec![80];
        ctx.summarizer.prompt = MARKER.to_owned();

        // The round's own sends answer with a short stream — a bare assistant keeps
        // the round loop turning without asking anything of the summarizer — while
        // the requests carrying the marker, which are the summarizer's, fail. That
        // failure is what arms the backoff.
        let stream = "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n";
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains(MARKER))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(stream.as_bytes().to_vec(), "text/event-stream"),
            )
            .with_priority(1)
            .mount(&server)
            .await;

        // The stretch the last round left open: one send charged, the backoff
        // armed for a minute, which would cover the wake's own gate calls.
        crate::context::compact_pre_loop(&mut ctx).await;
        assert_eq!(ctx.compaction.attempts(), 1);
        assert!(
            ctx.compaction.is_latched(timefmt::now_epoch()),
            "the pre-loop failure armed the backoff"
        );

        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::External).await;

        let summaries = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains(MARKER))
            .count();
        assert_eq!(
            summaries, 1,
            "the wake keeps the armed backoff: the summarizer is not asked again"
        );
        assert_eq!(
            ctx.compaction.attempts(),
            0,
            "the wake starts the count over"
        );
        assert!(
            ctx.compaction.is_latched(timefmt::now_epoch()),
            "the armed backoff outlives the count reset"
        );
    }

    /// An over-budget context whose summarizer answers `400` — the class table's
    /// terminal class — so the pre-loop wrapper reaches a stop.
    async fn stop_pin_ctx() -> (AgentContext, wiremock::MockServer) {
        let (source, server) = crate::test_support::mock_source(&["ep1"], 400).await;
        let mut ctx = crate::test_support::ctx_from_source(
            vec![crate::test_support::profile("p1", "ep1", 500_000)],
            source,
            crate::retry::RetryPolicy::default(),
        )
        .await;
        crate::test_support::set_context_window(&mut ctx, crate::test_support::FAILURE_PIN_WINDOW)
            .await;
        crate::test_support::seed_over_budget_store(&mut ctx).await;
        // One threshold leaves no progressive-warning level above the trigger, so
        // the gate reaches the compaction branch on its first call.
        ctx.config.context_thresholds = vec![80];
        (ctx, server)
    }

    /// How many records the history log holds: one line per appended record.
    fn history_lines(dir: &std::path::Path) -> usize {
        let mut lines = 0;
        for entry in std::fs::read_dir(dir.join("history")).expect("the history dir exists") {
            let path = entry.expect("a readable entry").path();
            let text = std::fs::read_to_string(path).expect("a readable history file");
            lines += text.lines().filter(|line| !line.is_empty()).count();
        }
        lines
    }
}
