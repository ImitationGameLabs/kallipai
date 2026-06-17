//! Agent task orchestration: shared context, round execution, prompt processing.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, AgentOutcome};

use crate::approval::ApprovalStore;
use crate::config::AgentConfig;
use crate::context::{ContextStore, ContextSummarizer, TurnId};
use crate::history::{HistoryWriter, RecordKind};
use crate::policy::AuthorizedToolExecutor;
use crate::runner;
use just_llm_client::types::chat::ChatMessage;

/// A cancellation token scoped to a single round, always a child of the agent's lifecycle
/// token ([`AgentContext::cancel`]). Cancelled by `interrupt_agent` to abort the current
/// round without terminating the task.
///
/// Because it is a child, a lifecycle cancel (delete / daemon shutdown) propagates to it —
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
    /// The lifecycle token was cancelled (delete / shutdown) → terminate the task.
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

/// Shared agent resources passed between modes.
pub struct AgentContext {
    pub client: crate::profile::ChatClient,
    /// Within-tier failover state: the resolved capability tier, the profile registry (for
    /// rebuilding the client on advance), the system prompt, and the sticky `profile_idx` (the
    /// sole writer of which is `FailoverState::advance_to`). See `FailoverState`.
    pub failover: crate::failover::FailoverState,
    pub store: Arc<Mutex<ContextStore>>,
    pub approvals: Arc<Mutex<ApprovalStore>>,
    pub executor: AuthorizedToolExecutor,
    pub summarizer: ContextSummarizer,
    pub config: AgentConfig,
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
    /// Daemon-wide token budget shared by all agents.
    /// Cloned from `AppState` — same underlying Arc counters across all agents.
    pub token_budget: crate::token_budget::TokenBudget,
}

impl AgentContext {
    /// Persist context and approval state to disk. Logs warnings on failure.
    pub async fn persist(&self) {
        let Some(ref dir) = self.agent_dir else {
            return;
        };

        {
            let guard = self.store.lock().await;
            if let Ok(json) = serde_json::to_string(&*guard)
                && let Err(e) = crate::persistence::persist_context(&json, dir)
            {
                tracing::error!("context persist failed: {e:#}");
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
        messages: &[ChatMessage],
        estimated_tokens: usize,
        kind: RecordKind,
        event: Option<crate::history::SystemEvent>,
    ) {
        if let Some(ref history) = self.history
            && let Err(e) = history.append(turn_id, messages, estimated_tokens, kind, event)
        {
            tracing::warn!(turn_id = ?turn_id, "history write failed: {e:#}");
        }
    }

    /// Record a turn to both the context store and the append-only history log.
    /// Returns the assigned `TurnId`.
    pub async fn record_turn(&self, messages: Vec<ChatMessage>) -> TurnId {
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
        );
        turn_id
    }
}
pub async fn agent_task(
    mut ctx: AgentContext,
    initial_prompt: Option<String>,
    mut prompt_rx: tokio::sync::mpsc::Receiver<String>,
    agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
) {
    // Pre-loop compaction: handle context overflow from restored agents.
    if let Err(e) = runner::compact_if_needed(&ctx).await {
        tracing::warn!("pre-loop compaction failed: {e:#}");
    }

    if let Some(p) = initial_prompt {
        if p.is_empty() {
            return;
        }
        ctx.record_turn(vec![ChatMessage::user(&p)]).await;
        if run_and_report(&mut ctx, &agent_tx, &mut prompt_rx).await {
            return;
        }
    }

    loop {
        tokio::select! {
            input = prompt_rx.recv() => {
                match input {
                    Some(text) => {
                        ctx.record_turn(vec![ChatMessage::user(&text)]).await;
                        if run_and_report(&mut ctx, &agent_tx, &mut prompt_rx).await {
                            break;
                        }
                    }
                    None => break,
                }
            }
            // Lifecycle cancel (delete / daemon shutdown): terminate the task.
            // Per-agent interrupt never reaches here — it cancels only the current
            // round token inside `run_and_report`, not the lifecycle token.
            _ = ctx.cancel.cancelled() => {
                tracing::info!("agent task: lifecycle cancel, persisting and exiting");
                terminate_cancelled(&ctx, &agent_tx).await;
                break;
            }
            _ = ctx.notify.notified() => {
                // Guard: drain may have already consumed the notification during
                // the previous round.  Skip the LLM call if nothing is pending.
                // NOTE: This guard assumes all notify_one() producers push data
                // to ApprovalStore::notifications.  If additional wakeup sources
                // are added, either use a separate Notify or update this check.
                if ctx.approvals.lock().await.has_notifications()
                    && run_and_report(&mut ctx, &agent_tx, &mut prompt_rx).await
                {
                    break;
                }
            }
        }
    }
}

/// Persist context + approval state and emit the terminal [`AgentEvent::Cancelled`].
///
/// The single exit path for a lifecycle cancel (delete / daemon shutdown), shared by the
/// outer-loop cancel arm (idle) and `run_and_report`'s mid-round lifecycle-cancel branch.
async fn terminate_cancelled(ctx: &AgentContext, agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>) {
    ctx.persist().await;
    agent_tx.send(AgentEvent::Cancelled).await.ok();
}

/// Run agent rounds for one prompt and send results via channel.
///
/// Owns the round-token lifecycle: mints a fresh child of the lifecycle token, publishes it
/// into `ctx.round_cancel` (so `interrupt_agent` can reach it), runs the round, then clears
/// the slot. Returns `true` only on a lifecycle cancel (the task should terminate); every
/// other outcome — including interrupt and budget-exceeded — returns `false` so the outer
/// loop continues and the task stays alive.
pub async fn run_and_report(
    ctx: &mut AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
) -> bool {
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
        Ok(AgentOutcome::Finished { content }) => {
            ctx.record_turn(vec![ChatMessage::assistant(&content)])
                .await;
            agent_tx.send(AgentEvent::Finished(content)).await.ok();
            false
        }
        Ok(AgentOutcome::MaxRoundsExceeded) => {
            agent_tx.send(AgentEvent::MaxRoundsExceeded).await.ok();
            false
        }
        Ok(AgentOutcome::Cancelled) => match CancelKind::classify(&ctx.cancel) {
            // Lifecycle cancel propagated to the round token → terminate.
            CancelKind::Lifecycle => {
                tracing::info!("agent task: lifecycle cancel mid-round, persisting and exiting");
                terminate_cancelled(ctx, agent_tx).await;
                true
            }
            // Only the round token was cancelled (interrupt) → emit Interrupted, keep living.
            CancelKind::Interrupt => {
                agent_tx.send(AgentEvent::Interrupted).await.ok();
                false
            }
        },
        Ok(AgentOutcome::TokenBudgetExceeded { consumed, budget }) => {
            // Non-fatal: the task stays alive. The round's turns are already
            // recorded (in-memory store + history file); they flush to
            // `context.json` on the next `persist()`. The next round re-checks the
            // budget and succeeds once the operator raises it.
            agent_tx
                .send(AgentEvent::TokenBudgetExceeded { consumed, budget })
                .await
                .ok();
            false
        }
        Ok(AgentOutcome::FailoverChainExhausted { reason, detail }) => {
            // Non-fatal: the task stays alive. The failover chain ran out — the operator may
            // reconfigure failover (or fix backup credentials) and re-prompt. No turn to record
            // (no assistant content) and no extra persist (the failover arm already flushed the
            // endpoint's retry records before exhausting the chain).
            agent_tx
                .send(AgentEvent::FailoverChainExhausted { reason, detail })
                .await
                .ok();
            false
        }
        Err(e) => {
            agent_tx
                .send(AgentEvent::Error(format!("{e:#}")))
                .await
                .ok();
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Lifecycle cancel (delete / shutdown): propagates to the round token (child).
        let lifecycle = CancellationToken::new();
        let round = RoundToken::new(&lifecycle);
        lifecycle.cancel();
        assert!(round.handle().is_cancelled());
        assert_eq!(CancelKind::classify(&lifecycle), CancelKind::Lifecycle);
    }
}
