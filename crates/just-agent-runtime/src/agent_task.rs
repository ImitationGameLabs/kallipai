//! Agent task orchestration: shared context, round execution, command handling.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, AgentOutcome};
use just_agent_common::command::{SlashCommand, UserInput};

use crate::approval::ApprovalStore;
use crate::config::AgentConfig;
use crate::context::{AgenticContext, ContextStore, ContextSummarizer, TurnId};
use crate::history::{HistoryWriter, RecordKind};
use crate::policy::AuthorizedToolExecutor;
use crate::runner;
use just_llm_client::types::chat::ChatMessage;

/// Shared agent resources passed between modes.
pub struct AgentContext {
    pub client: just_llm_client::ChatClient,
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
    mut prompt_rx: tokio::sync::mpsc::Receiver<UserInput>,
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
        if run_and_report(&mut ctx, &agent_tx, &mut prompt_rx)
            .await
            .is_some()
        {
            return;
        }
    }

    loop {
        tokio::select! {
            input = prompt_rx.recv() => {
                match input {
                    Some(UserInput::Prompt(text)) => {
                        ctx.record_turn(vec![ChatMessage::user(&text)]).await;
                        if run_and_report(&mut ctx, &agent_tx, &mut prompt_rx).await.is_some() {
                            break;
                        }
                    }
                    Some(UserInput::Command(cmd)) => {
                        handle_command(&cmd, &mut ctx, &agent_tx).await;
                    }
                    None => break,
                }
            }
            _ = ctx.cancel.cancelled() => {
                tracing::info!("agent task: cancellation requested, persisting and exiting");
                ctx.persist().await;
                agent_tx.send(AgentEvent::Cancelled).await.ok();
                break;
            }
            _ = ctx.notify.notified() => {
                // Guard: drain may have already consumed the notification during
                // the previous round.  Skip the LLM call if nothing is pending.
                // NOTE: This guard assumes all notify_one() producers push data
                // to ApprovalStore::notifications.  If additional wakeup sources
                // are added, either use a separate Notify or update this check.
                if ctx.approvals.lock().await.has_notifications()
                    && run_and_report(&mut ctx, &agent_tx, &mut prompt_rx)
                        .await
                        .is_some()
                {
                    break;
                }
            }
        }
    }
}

/// Handle a slash command that requires agent-side resources.
async fn handle_command(
    cmd: &SlashCommand,
    ctx: &mut AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    if let SlashCommand::Status = cmd {
        let usage = ctx.store.lock().await.usage_snapshot();
        agent_tx
            .send(AgentEvent::Status(usage.format_summary()))
            .await
            .ok();
    }
}

/// Run agent rounds for one prompt and send results via channel.
pub async fn run_and_report(
    ctx: &mut AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<UserInput>,
) -> Option<AgentOutcome> {
    agent_tx.send(AgentEvent::Busy).await.ok();
    match runner::run_agent_rounds(ctx, agent_tx, prompt_rx).await {
        Ok(AgentOutcome::Finished { content }) => {
            ctx.record_turn(vec![ChatMessage::assistant(&content)])
                .await;
            agent_tx.send(AgentEvent::Finished(content)).await.ok();
            None
        }
        Ok(AgentOutcome::MaxRoundsExceeded) => {
            agent_tx.send(AgentEvent::MaxRoundsExceeded).await.ok();
            None
        }
        Ok(outcome @ AgentOutcome::Cancelled) => {
            ctx.persist().await;
            agent_tx.send(AgentEvent::Cancelled).await.ok();
            Some(outcome)
        }
        Ok(AgentOutcome::TokenBudgetExceeded { consumed, budget }) => {
            ctx.persist().await;
            agent_tx
                .send(AgentEvent::TokenBudgetExceeded { consumed, budget })
                .await
                .ok();
            Some(AgentOutcome::TokenBudgetExceeded { consumed, budget })
        }
        Err(e) => {
            agent_tx
                .send(AgentEvent::Error(format!("{e:#}")))
                .await
                .ok();
            None
        }
    }
}
