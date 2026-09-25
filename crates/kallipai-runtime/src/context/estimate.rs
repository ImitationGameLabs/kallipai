//! Prompt-token estimation for the next LLM request.
//!
//! [`estimate_context_tokens`] produces an incremental estimate anchored to the store's last
//! authoritative `last_prompt_tokens` when no prefix-mutating op has occurred, and falls back to
//! a full render otherwise. Reads only the store's anchor API — no
//! [`crate::agent_task::AgentContext`] dependency, keeping this layer decoupled from the task.

use anyhow::Result;
use just_llm_client::types::generation::{Message, ToolDefinition};
use tokio::sync::Mutex;
use tracing::warn;

use super::store::ContextStore;
use super::tokens::{estimate_text, estimation_views};
use crate::profile::GenerationClient;

/// Estimate the prompt-token size of the next request.
///
/// **Incremental** (normal round, no prefix change since the last response): the authoritative
/// `last_prompt_tokens` from the last provider response — which already counts system prompt +
/// tools + pinned + turns[0..anchor] — plus a tokenx render-estimate (`context::tokens`) of
/// *only* the turns added since that response (the assistant turn just completed, its tool
/// results, and the next user prompt). Exact base + a small approximated delta: cheaper than a
/// full render and more accurate than re-estimating the whole history.
///
/// **Full** (first round ever, after any prefix-mutating op, after failover, or after restore):
/// a tokenx render of `system_prompt + messages + tools`. Required whenever the persisted anchor
/// can't be trusted — e.g. a restore following an agent-version upgrade may have changed the
/// system prompt or tool set (see `ContextStore::needs_full_estimate`). `messages` is the
/// `compose_context` output (pinned turns first, then conversation; no system prompt); the
/// system prompt is rendered separately so the full estimate matches what the provider receives.
pub(crate) async fn estimate_context_tokens(
    client: &GenerationClient,
    store: &Mutex<ContextStore>,
    messages: &[Message],
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
            let delta: Vec<Message> = g
                .turns()
                .iter()
                .skip(anchored.min(turns_len))
                .flat_map(|t| t.messages.iter().cloned())
                .collect();
            drop(g);
            // An empty delta (no turns added since the anchor) contributes 0 tokens; skip the
            // render so the `[]` envelope doesn't add a spurious token.
            // The delta renders as estimation views: image parts cost vision tokens (see
            // `context::tokens`), not their serialized Base64.
            let delta_tokens = if delta.is_empty() {
                0
            } else {
                let (views, image_tokens) = estimation_views(&delta);
                estimate_text(&client.render_messages(&views)?) + image_tokens
            };
            Ok(base as usize + delta_tokens)
        }
        _ => {
            drop(g);
            // Full: system + messages + tools (the historical behavior), with the messages
            // rendered as estimation views (image parts swapped for the marker; their vision-
            // token approximation added on top).
            let mut rendered = String::new();
            if let Some(sp) = system_prompt {
                rendered.push_str(&client.render_messages(&[Message::system(sp)])?);
            }
            let (views, image_tokens) = estimation_views(messages);
            rendered.push_str(&client.render_messages(&views)?);
            rendered.push_str(&client.render_tools(tools)?);
            Ok(estimate_text(&rendered) + image_tokens)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::context::AgenticContext;
    use crate::test_support::{make_ctx, profile, usage, user_msg};
    use base64::Engine as _;
    use just_llm_client::types::generation::ContentPart;

    /// With an anchor and no turns added since, the incremental estimate equals the authoritative
    /// base exactly (empty delta → +0). Pins the incremental path and the anchor mechanic.
    #[tokio::test]
    async fn incremental_estimate_equals_base_with_no_new_turns() {
        let ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
        {
            let mut s = ctx.store.lock().await;
            s.push_turn(vec![user_msg("first turn")]);
            s.push_turn(vec![user_msg("second turn")]);
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
            s.push_turn(vec![user_msg("first")]);
            s.accumulate_usage(&usage(5_000));
        }
        let before = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        ctx.store
            .lock()
            .await
            .push_turn(vec![user_msg("a brand new turn with some content")]);
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
            s.push_turn(vec![user_msg("turn one")]);
            s.push_turn(vec![user_msg("turn two")]);
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

    #[tokio::test]
    async fn full_estimate_ignores_base64_size_and_tracks_dimensions() {
        let ctx = make_ctx(vec![profile("p1", "p1", 100_000)], &["p1"]).await;
        // Minimal PNG header (see `context::tokens` tests for the shape).
        let png = |width: u32, height: u32| {
            let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
            b.extend_from_slice(b"IHDR");
            b.extend_from_slice(&width.to_be_bytes());
            b.extend_from_slice(&height.to_be_bytes());
            b.extend_from_slice(&[8, 6, 0, 0, 0]);
            base64::engine::general_purpose::STANDARD.encode(b)
        };
        let image_message = |data: String| {
            Message::user_parts(vec![
                ContentPart::Text {
                    text: "a chart".to_owned(),
                },
                ContentPart::Image {
                    source: just_llm_client::types::generation::ImageSource::Base64 {
                        data,
                        media_type: "image/png".to_owned(),
                    },
                    detail: None,
                },
            ])
        };
        async fn run_estimate(
            ctx: &crate::agent_task::AgentContext,
            messages: &[Message],
        ) -> usize {
            estimate_context_tokens(&ctx.client, &ctx.store, messages, &[], None)
                .await
                .unwrap()
        }

        let small = vec![image_message(png(64, 64))];
        let mut padded = png(64, 64);
        padded.push_str(&base64::engine::general_purpose::STANDARD.encode(vec![0u8; 100_000]));
        let large = vec![image_message(padded)];
        // Same dimensions, wildly different serialized sizes — the bytes
        // are not tokens, so the full render estimate must not move.
        assert_eq!(
            run_estimate(&ctx, &small).await,
            run_estimate(&ctx, &large).await
        );
        // A larger image does estimate higher.
        let bigger = vec![image_message(png(640, 640))];
        assert!(
            run_estimate(&ctx, &bigger).await
                > run_estimate(&ctx, &[image_message(png(64, 64))]).await
        );
    }
}
