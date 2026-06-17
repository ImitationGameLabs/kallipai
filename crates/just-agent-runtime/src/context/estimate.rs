//! Prompt-token estimation for the next LLM request.
//!
//! [`estimate_context_tokens`] produces an incremental estimate anchored to the store's last
//! authoritative `last_prompt_tokens` when no prefix-mutating op has occurred, and falls back to
//! a full render otherwise. Reads only the store's anchor API — no
//! [`crate::agent_task::AgentContext`] dependency, keeping this layer decoupled from the task.

use anyhow::Result;
use just_llm_client::types::chat::{ChatMessage, ToolDefinition};
use tokio::sync::Mutex;
use tracing::warn;

use super::store::ContextStore;
use crate::profile::ChatClient;

/// Estimate the prompt-token size of the next request.
///
/// **Incremental** (normal round, no prefix change since the last response): the authoritative
/// `last_prompt_tokens` from the last provider response — which already counts system prompt +
/// tools + pinned + turns[0..anchor] — plus a chars/4 render-estimate of *only* the turns added
/// since that response (the assistant turn just completed, its tool results, and the next user
/// prompt). Exact base + a small approximated delta: cheaper than a full render and more accurate
/// than re-estimating the whole history.
///
/// **Full** (first round ever, after any prefix-mutating op, after failover, or after restore):
/// a chars/4 render of `system_prompt + messages + tools`. Required whenever the persisted anchor
/// can't be trusted — e.g. a restore following an agent-version upgrade may have changed the
/// system prompt or tool set (see `ContextStore::needs_full_estimate`). `messages` is the
/// `compose_context` output (`pinned ++ turns`, no system prompt); the system prompt is rendered
/// separately so the full estimate matches what the provider receives.
pub(crate) async fn estimate_context_tokens(
    client: &ChatClient,
    store: &Mutex<ContextStore>,
    messages: &[ChatMessage],
    tools: &[ToolDefinition],
    system_prompt: Option<&str>,
) -> Result<usize> {
    let g = store.lock().await;
    match (g.last_prompt_tokens(), g.needs_full_estimate()) {
        (Some(base), false) => {
            // Incremental: only the turns added since the anchor.
            let turns_len = g.turns().len();
            let anchored = g.anchored_turn_count();
            debug_assert!(
                anchored <= turns_len,
                "anchor out of range — a needs_full_estimate flag-set was missed"
            );
            if anchored > turns_len {
                warn!(
                    anchored,
                    turns_len, "estimate anchor clamped to turns length"
                );
            }
            let delta: Vec<ChatMessage> = g
                .turns()
                .iter()
                .skip(anchored.min(turns_len))
                .flat_map(|t| t.messages.iter().cloned())
                .collect();
            drop(g);
            Ok(base as usize + client.render_messages(&delta)?.chars().count() / 4)
        }
        _ => {
            // Full: system + messages + tools (the historical behavior).
            drop(g);
            let mut rendered = String::new();
            if let Some(sp) = system_prompt {
                rendered.push_str(&client.render_messages(&[ChatMessage::system(sp)])?);
            }
            rendered.push_str(&client.render_messages(messages)?);
            rendered.push_str(&client.render_tools(tools)?);
            Ok(rendered.chars().count() / 4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use just_llm_client::types::chat::ChatMessage;

    use crate::context::AgenticContext;
    use crate::test_support::{make_ctx, profile, usage};

    /// With an anchor and no turns added since, the incremental estimate equals the authoritative
    /// base exactly (empty delta → +0). Pins the incremental path and the anchor mechanic.
    #[tokio::test]
    async fn incremental_estimate_equals_base_with_no_new_turns() {
        let ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
        {
            let mut s = ctx.store.lock().await;
            s.push_turn(vec![ChatMessage::user("first turn")]);
            s.push_turn(vec![ChatMessage::user("second turn")]);
            s.accumulate_usage(&usage(5_000));
        }
        let est = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        assert_eq!(est, 5_000, "anchored + empty delta → base exactly");
    }

    /// A turn added after the anchor is reflected as a positive delta on top of the base.
    #[tokio::test]
    async fn incremental_estimate_grows_with_new_turn() {
        let ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
        {
            let mut s = ctx.store.lock().await;
            s.push_turn(vec![ChatMessage::user("first")]);
            s.accumulate_usage(&usage(5_000));
        }
        let before = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        ctx.store.lock().await.push_turn(vec![ChatMessage::user(
            "a brand new turn with some content",
        )]);
        let after = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        assert!(
            after > before,
            "delta of the new turn is added: {after} > {before}"
        );
    }

    /// A prefix-mutating op (evict) forces full mode: the estimate drops from the anchored base
    /// to a fresh full render, proving the flag flips the path off the (stale) anchor.
    #[tokio::test]
    async fn evict_forces_full_estimate_off_the_anchor() {
        let ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
        {
            let mut s = ctx.store.lock().await;
            s.push_turn(vec![ChatMessage::user("turn one")]);
            s.push_turn(vec![ChatMessage::user("turn two")]);
            // A huge authoritative base; the incremental path would report ~the base.
            s.accumulate_usage(&usage(5_000_000));
        }
        let incremental = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        assert!(
            incremental >= 5_000_000,
            "incremental path used before evict, got {incremental}"
        );

        // Evict invalidates the anchor → full mode recomputes a fresh render.
        ctx.store.lock().await.evict_turns(1);
        let full = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        assert!(
            full < 5_000_000,
            "evict forces full mode (fresh render), got {full}"
        );
    }
}
