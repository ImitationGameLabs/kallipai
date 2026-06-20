//! Context composition: assembles layers into `Vec<ChatMessage>`.

use std::sync::Arc;

use just_llm_client::types::chat::ChatMessage;
use tokio::sync::Mutex;

use super::store::ContextStore;

/// Build the context for the next LLM call.
///
/// `turns` are stored `[pinned…][conversation…]` (see `ContextStore::pinned_turn_count`), so a
/// single iteration yields persistent pinned context first, then the conversation in order.
/// Returns all messages without budget filtering — the caller is responsible for estimating
/// tokens and triggering summarize_and_evict.
pub async fn compose_context(store: Arc<Mutex<ContextStore>>) -> Vec<ChatMessage> {
    let guard = store.lock().await;
    let mut messages = Vec::new();
    for turn in guard.turns() {
        messages.extend(turn.messages.iter().cloned());
    }
    messages
}
