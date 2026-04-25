//! Context composition: assembles layers into `Vec<ChatMessage>`.

use std::sync::Arc;

use just_llm_client::types::chat::ChatMessage;
use tokio::sync::Mutex;

use super::store::ContextStore;

/// Build the context for the next LLM call.
///
/// Layers are filled in priority order: pinned → summary → turns.
/// Returns all messages without budget filtering — the caller is
/// responsible for estimating tokens and triggering compaction.
pub async fn compose_context(store: Arc<Mutex<ContextStore>>) -> Vec<ChatMessage> {
    let guard = store.lock().await;
    let mut messages = Vec::new();

    // Layer 1: Pinned items (always included).
    for item in guard.pinned() {
        messages.push(item.message.clone());
    }

    // Layer 2: Summary (if present).
    if let Some(summary) = guard.summary() {
        messages.push(ChatMessage::assistant(summary));
    }

    // Layer 3: All turns (oldest to newest).
    for turn in guard.turns() {
        messages.extend(turn.messages.iter().cloned());
    }

    messages
}
