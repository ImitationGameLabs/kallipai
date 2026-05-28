//! Agent session orchestration: shared context, round execution, command handling.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::command::{SlashCommand, UserInput};
use crate::config::AgentConfig;
use crate::context::{AgenticContext, ContextStore, ContextSummarizer};
use crate::deferred::DeferredQueue;
use crate::policy::AuthorizedToolExecutor;
use crate::runner;
use crate::types::{AgentEvent, AgentOutcome};
use just_llm_client::types::chat::ChatMessage;

/// Shared agent resources passed between modes.
pub struct AgentContext {
    pub client: just_llm_client::ChatClient,
    pub store: Arc<Mutex<ContextStore>>,
    pub deferred: Arc<Mutex<DeferredQueue>>,
    pub executor: AuthorizedToolExecutor,
    pub summarizer: ContextSummarizer,
    pub config: AgentConfig,
    /// Session directory for persistence.
    pub session_dir: Option<PathBuf>,
    /// Cancellation signal for graceful interruption.
    pub cancel: CancellationToken,
}

impl AgentContext {
    /// Persist context and deferred state to disk. Logs warnings on failure.
    pub async fn persist(&self) {
        let Some(ref dir) = self.session_dir else {
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
            let guard = self.deferred.lock().await;
            if let Ok(json) = serde_json::to_string(&*guard)
                && let Err(e) = crate::persistence::persist_deferred(&json, dir)
            {
                tracing::error!("deferred persist failed: {e:#}");
            }
        }
    }
}

/// Agent task: receives user input, runs rounds, sends events back.
pub async fn agent_task(
    mut ctx: AgentContext,
    initial_prompt: Option<String>,
    mut prompt_rx: tokio::sync::mpsc::Receiver<UserInput>,
    agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
) {
    // Pre-loop compaction: handle context overflow from restored sessions.
    if let Err(e) = runner::compact_if_needed(&ctx).await {
        tracing::warn!("pre-loop compaction failed: {e:#}");
    }

    if let Some(p) = initial_prompt {
        if p.is_empty() {
            return;
        }
        ctx.store
            .lock()
            .await
            .push_turn(vec![ChatMessage::user(&p)]);
        if run_and_report(&mut ctx, &agent_tx).await.is_some() {
            return;
        }
    }

    loop {
        tokio::select! {
            input = prompt_rx.recv() => {
                match input {
                    Some(UserInput::Prompt(text)) => {
                        ctx.store
                            .lock()
                            .await
                            .push_turn(vec![ChatMessage::user(&text)]);
                        if run_and_report(&mut ctx, &agent_tx).await.is_some() {
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
) -> Option<AgentOutcome> {
    agent_tx.send(AgentEvent::Busy).await.ok();
    match runner::run_agent_rounds(ctx, agent_tx).await {
        Ok(AgentOutcome::Finished { content }) => {
            ctx.store
                .lock()
                .await
                .push_turn(vec![ChatMessage::assistant(&content)]);
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
        Err(e) => {
            agent_tx
                .send(AgentEvent::Error(format!("{e:#}")))
                .await
                .ok();
            None
        }
    }
}
