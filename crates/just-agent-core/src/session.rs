//! Agent session orchestration: shared context, round execution, command handling.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::command::{SlashCommand, UserInput};
use crate::config::AgentConfig;
use crate::context::{AgenticContext, CompactionStrategy, ContextStore};
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
    pub strategy: Box<dyn CompactionStrategy>,
    pub config: AgentConfig,
}

/// Agent task: receives user input, runs rounds, sends events back.
pub async fn agent_task(
    mut ctx: AgentContext,
    initial_prompt: Option<String>,
    mut prompt_rx: tokio::sync::mpsc::Receiver<UserInput>,
    agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
) {
    if let Some(p) = initial_prompt {
        if p.is_empty() {
            return;
        }
        ctx.store
            .lock()
            .await
            .push_turn(vec![ChatMessage::user(&p)]);
        run_and_report(&mut ctx, &agent_tx).await;
    }

    while let Some(input) = prompt_rx.recv().await {
        match input {
            UserInput::Prompt(text) => {
                ctx.store
                    .lock()
                    .await
                    .push_turn(vec![ChatMessage::user(&text)]);
                run_and_report(&mut ctx, &agent_tx).await;
            }
            UserInput::Command(cmd) => {
                handle_command(&cmd, &mut ctx, &agent_tx).await;
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
    match cmd {
        SlashCommand::Status => {
            let usage = ctx.store.lock().await.usage_snapshot();
            let pinned_tokens: usize = usage.pinned_items.iter().map(|(_, t)| *t).sum();
            let msg = format!(
                "turns: {} ({} est tokens), pinned: {} ({} tokens), summary: {} tokens, last prompt: {}",
                usage.turn_count,
                usage.turn_tokens,
                usage.pinned_items.len(),
                pinned_tokens,
                usage.summary_tokens,
                usage
                    .last_prompt_tokens
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "n/a".into()),
            );
            agent_tx.send(AgentEvent::Status(msg)).await.ok();
        }
        SlashCommand::Compact => {
            agent_tx.send(AgentEvent::Busy).await.ok();
            match runner::compact_context(ctx).await {
                Ok(_) => {
                    agent_tx
                        .send(AgentEvent::Status("compaction complete".into()))
                        .await
                        .ok();
                }
                Err(e) => {
                    agent_tx
                        .send(AgentEvent::Error(format!("compaction failed: {e:#}")))
                        .await
                        .ok();
                }
            }
        }
        SlashCommand::Skill { name } => {
            match crate::tools::pin_skill(&mut *ctx.store.lock().await, name) {
                Ok(()) => {
                    agent_tx
                        .send(AgentEvent::Status(format!("skill '{name}' loaded")))
                        .await
                        .ok();
                }
                Err(e) => {
                    agent_tx
                        .send(AgentEvent::Error(format!("skill load failed: {e:#}")))
                        .await
                        .ok();
                }
            }
        }
        // Local commands (Help, Quit, Clear) are handled in the TUI layer.
        _ => {}
    }
}

/// Run agent rounds for one prompt and send results via channel.
pub async fn run_and_report(
    ctx: &mut AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    agent_tx.send(AgentEvent::Busy).await.ok();
    match runner::run_agent_rounds(ctx, agent_tx).await {
        Ok(AgentOutcome::Finished { content }) => {
            ctx.store
                .lock()
                .await
                .push_turn(vec![ChatMessage::assistant(&content)]);
            agent_tx.send(AgentEvent::Finished(content)).await.ok();
        }
        Ok(AgentOutcome::MaxRoundsExceeded) => {
            agent_tx.send(AgentEvent::MaxRoundsExceeded).await.ok();
        }
        Err(e) => {
            agent_tx
                .send(AgentEvent::Error(format!("{e:#}")))
                .await
                .ok();
        }
    }
}
