use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::{
    AgenticContext, CONTEXT_SUMMARY_LABEL, KeepCaps, KeepWindow, plan_keep_window,
    summary_record_with_range,
};

#[derive(Debug, Deserialize, Serialize)]
struct EvictArgs {
    /// Summary text preserving key facts from the evicted turns.
    summary: String,
}

/// Evicts the oldest conversation turns, keeps the newest window, and replaces
/// what it evicted with a summary.
///
/// The window is configuration — a turn count and a token slice of the
/// effective budget — never the call's arguments, and it is read from the store
/// on each call, so a profile change that moves the window reaches this tool
/// too; the summary states the range it covers.
/// The summary replaces any earlier `context_summary`, so the new text must
/// carry the old one's facts; no other pin is touched. This is the only
/// supported eviction path: you must preserve what matters.
pub struct ContextEvictTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl ContextEvictTool {
    /// Tool name exposed to the LLM and referenced by the policy layer.
    pub const NAME: &str = "context_evict";

    /// `ctx` is the store the spawn installed the agent's keep caps in: the
    /// caps are read per call, so they follow the active configuration.
    pub(crate) fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }
}

/// The reply one call returns.
///
/// Every call answers with the same fields, so a caller reads one shape: `note`
/// covers a call that condensed nothing (an empty conversation, or a window
/// already covering every turn) and one that condensed everything because
/// `max_turns` is 0; `degraded` is set only when the token slice alone kept
/// the newest turn out.
fn reply(
    evicted: usize,
    remaining_turns: usize,
    window: &KeepWindow,
    freed_tokens: usize,
    caps: KeepCaps,
    note: Option<&'static str>,
) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "evicted": evicted,
        "remaining_turns": remaining_turns,
        "kept_turns": window.kept.len(),
        "freed_tokens": freed_tokens,
        "first_kept_turn": window.kept.first().map(|id| id.0),
        "max_turns": caps.max_turns,
        "max_tokens": caps.max_tokens,
        "note": note,
        "degraded": window.degraded.as_ref().map(|d| d.reason()),
    }))?)
}

#[async_trait]
impl LlmTool for ContextEvictTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Evict the oldest conversation turns and keep the newest window, replacing \
         what you evicted with a summary you provide. The window comes from \
         configuration — a turn count and a token slice of the budget — not from \
         this call, and no pin except the summary moves: the summary is pinned as \
         context_summary and replaces any earlier summary, so carry the old one's \
         facts into the new text. The summary states which turns it covers: write \
         it to preserve the key facts, decisions, and current state that you need. \
         A configured turn cap of 0 condenses every turn and reports that as a \
         note; a newest turn that alone exceeds the token slice keeps nothing and \
         is reported as degraded. Use context_status to review current turns before \
         writing your summary."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Summary preserving the key facts from the conversation turns being evicted. It replaces any earlier summary, so keep that one's facts too."
                }
            },
            "required": ["summary"]
        })
    }

    async fn call(&self, args_json: &str) -> Result<String> {
        let args: EvictArgs =
            serde_json::from_str(args_json).context("context_evict: invalid arguments")?;

        let mut ctx = self.ctx.lock().await;
        let caps = ctx.eviction_caps();
        let convo = ctx.conversation_turn_sizes();
        let window = plan_keep_window(&convo, caps);
        if window.keeps_everything() {
            // Nothing to condense: an old summary must not be replaced by one
            // stating an empty range, and the pins stay exactly as they are.
            let note = if convo.is_empty() {
                "the conversation has no turns: nothing to condense"
            } else {
                "the keep window covers every turn: nothing to condense"
            };
            return reply(0, convo.len(), &window, 0, caps, Some(note));
        }

        let range = window
            .range
            .as_ref()
            .expect("a window that condenses something states its range");
        ctx.replace_pin(
            CONTEXT_SUMMARY_LABEL,
            summary_record_with_range(&args.summary, range),
        )?;
        let result = ctx.evict_turns(window.evict);
        ctx.reset_context_warnings();

        let note = (caps.max_turns == 0).then_some(
            "max_turns is 0: condensing every conversation turn is the configured \
             behaviour, not a lost cap",
        );
        reply(
            result.evicted,
            result.remaining_turns,
            &window,
            result.freed_tokens,
            caps,
            note,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;
    use crate::context::{
        CONTEXT_SUMMARY_HEADER, CONTEXT_SUMMARY_LABEL, CONTEXT_SUMMARY_RANGE_MARKER, ContextStore,
        TurnId, compose_context,
    };
    use crate::test_support::{tool_call_msg, tool_result_msg, user_msg};

    async fn run(tool: &ContextEvictTool, args: Value) -> Result<String> {
        tool.call(&serde_json::to_string(&args).unwrap()).await
    }

    /// The reply of one call whose summary text is `summary`.
    async fn reply_of(tool: &ContextEvictTool, summary: &str) -> Value {
        serde_json::from_str(&run(tool, json!({ "summary": summary })).await.unwrap()).unwrap()
    }

    /// The reply's field names, sorted, so two answers can be compared shape by
    /// shape.
    fn keys(reply: &Value) -> Vec<String> {
        let mut keys: Vec<String> = reply
            .as_object()
            .expect("a reply is an object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// Caps that remove every conversation turn: the shape the round-shape
    /// tests below were written against, where eviction means a full condense.
    fn wipe_all() -> KeepCaps {
        KeepCaps::new(0, usize::MAX)
    }

    /// A tool over `store` with `caps` installed the way a spawn installs them:
    /// the caps live in the store, so the tool reads them on every call.
    fn tool_over(
        mut store: ContextStore,
        caps: KeepCaps,
    ) -> (Arc<Mutex<ContextStore>>, ContextEvictTool) {
        store.set_eviction_caps(caps);
        let store = Arc::new(Mutex::new(store));
        let tool = ContextEvictTool::new(store.clone());
        (store, tool)
    }

    /// Pushes a conversation turn and sets its size, so a test can steer the
    /// token cap without building a message of that exact length.
    fn push_sized(store: &mut ContextStore, tokens: usize) -> TurnId {
        let (id, _) = store.push_turn(vec![user_msg("work")]);
        let turn = store.turns_mut().back_mut().expect("just pushed");
        turn.estimated_tokens = tokens;
        id
    }

    fn summary_text(store: &ContextStore) -> String {
        store
            .turns()
            .iter()
            .find(|t| t.is_pinned() && t.label() == Some(CONTEXT_SUMMARY_LABEL))
            .map(|t| t.messages[0].content().unwrap_or_default().to_owned())
            .unwrap_or_default()
    }

    /// The pinned summary is a user record: our note about earlier turns, never
    /// a model turn. The range line is marked as an eviction's own.
    #[tokio::test]
    async fn evict_pins_the_summary_as_a_user_record() {
        let mut store = ContextStore::new();
        store.push_turn(vec![user_msg("earlier work")]);
        let (store, tool) = tool_over(store, wipe_all());

        run(&tool, json!({ "summary": "we did the thing" }))
            .await
            .unwrap();

        let guard = store.lock().await;
        let pinned = guard
            .turns()
            .iter()
            .find(|t| t.is_pinned() && t.label() == Some(CONTEXT_SUMMARY_LABEL))
            .expect("the summary is pinned");
        let message = &pinned.messages[0];
        assert_eq!(message.role(), "user");
        assert_eq!(message.tool_call_id(), None);
        let text = message.content().unwrap_or_default();
        assert!(text.starts_with(CONTEXT_SUMMARY_HEADER));
        assert!(
            text.contains(CONTEXT_SUMMARY_RANGE_MARKER),
            "the range line is marked as an eviction's: {text}"
        );
        assert!(text.ends_with("we did the thing"));
    }

    /// The request that follows an eviction carries the pinned block and then
    /// the eviction round itself, and it must never carry two adjacent
    /// assistant messages (nor end on one).
    #[tokio::test]
    async fn request_after_eviction_never_pairs_assistant_messages() {
        let mut store = ContextStore::new();
        store.push_turn(vec![user_msg("earlier work")]);
        store.push_turn(vec![user_msg("more work")]);
        let (store, tool) = tool_over(store, wipe_all());

        run(&tool, json!({ "summary": "we did the thing" }))
            .await
            .unwrap();

        // The round that called the tool is recorded after the drain.
        {
            let mut guard = store.lock().await;
            guard.push_turn(vec![
                tool_call_msg("c1", "context_evict", "{}"),
                tool_result_msg("{}", "c1"),
            ]);
        }

        let messages = compose_context(store.clone()).await;
        let paired = messages.windows(2).any(|pair| {
            pair[0].role() == "assistant"
                && pair[0].reasoning().is_none()
                && pair[1].role() == "assistant"
        });
        assert!(
            !paired,
            "an assistant message without reasoning must not be followed by another assistant message"
        );
        let last = messages.last().expect("the request carries messages");
        assert!(
            !(last.role() == "assistant" && last.reasoning().is_none()),
            "the request must not end on an assistant message without reasoning"
        );
    }

    /// The keep rule leaves a contiguous newest suffix and the pin states which
    /// turns it covers; the reply reports the same boundary.
    #[tokio::test]
    async fn the_newest_window_survives_with_its_range_stated() {
        let mut store = ContextStore::new();
        let ids: Vec<TurnId> = (0..4).map(|_| push_sized(&mut store, 10)).collect();
        let (store, tool) = tool_over(store, KeepCaps::new(2, 1_000));

        let reply = reply_of(&tool, "we did the thing").await;
        assert_eq!(reply["evicted"], 2);
        assert_eq!(reply["remaining_turns"], 2);
        assert_eq!(reply["kept_turns"], 2);
        assert_eq!(reply["freed_tokens"], 20);
        assert_eq!(reply["first_kept_turn"], ids[2].0);
        assert_eq!(reply["max_turns"], 2);
        assert_eq!(reply["max_tokens"], 1_000);
        assert!(reply["degraded"].is_null());
        assert!(reply["note"].is_null());

        let guard = store.lock().await;
        assert_eq!(guard.usage_snapshot().turn_count, 2);
        assert_eq!(
            guard.turns().iter().find(|t| !t.is_pinned()).map(|t| t.id),
            Some(ids[2])
        );
        let text = summary_text(&guard);
        let kept = format!("#{}", ids[2].0);
        assert!(text.contains(&kept), "the surviving range: {text}");
        assert!(text.ends_with("we did the thing"));
    }

    /// The caps derive from the agent's configuration and are read per call, so
    /// a window change that re-installs them — failover reapply, profile reset —
    /// moves what the next eviction keeps instead of leaving a spawn-time copy
    /// in force.
    #[tokio::test]
    async fn the_window_follows_a_cap_reinstall() {
        let mut config = AgentConfig::default();
        config.set_context_keep_turns(2);
        config
            .set_context_keep_tokens_ratio(0.5)
            .expect("half the budget is in range");
        let mut store = ContextStore::new();
        for _ in 0..4 {
            push_sized(&mut store, 10);
        }
        let (store, tool) = tool_over(store, config.eviction_caps());

        let reply = reply_of(&tool, "condensed").await;
        assert_eq!(reply["max_turns"], 2, "the override sizes the window");
        assert_eq!(reply["max_tokens"], config.context_keep_tokens());
        assert_eq!(reply["kept_turns"], 2);
        assert_eq!(reply["evicted"], 2);

        // A tighter window arrives while the tool is already built.
        config.set_context_keep_turns(1);
        config
            .set_context_keep_tokens_ratio(0.1)
            .expect("a tenth is in range");
        {
            let mut guard = store.lock().await;
            guard.set_eviction_caps(config.eviction_caps());
            for _ in 0..2 {
                push_sized(&mut guard, 10);
            }
        }

        let reply = reply_of(&tool, "condensed again").await;
        assert_eq!(reply["max_turns"], 1, "the re-installed cap applies");
        assert_eq!(reply["max_tokens"], config.context_keep_tokens());
        assert_eq!(reply["kept_turns"], 1);
        assert_eq!(reply["evicted"], 3);
    }

    /// The token cap binds before the turn cap when turns are expensive.
    #[tokio::test]
    async fn the_token_cap_binds_before_the_turn_cap() {
        let mut store = ContextStore::new();
        for _ in 0..4 {
            push_sized(&mut store, 100);
        }
        let (_, tool) = tool_over(store, KeepCaps::new(10, 250));

        let reply = reply_of(&tool, "condensed").await;
        assert_eq!(reply["kept_turns"], 2, "two turns fit under 250 tokens");
        assert_eq!(reply["evicted"], 2);
    }

    /// A newest turn too large to keep degrades to a full condense rather than
    /// surviving and blowing up the window.
    #[tokio::test]
    async fn a_newest_turn_over_the_token_cap_degrades_to_a_full_condense() {
        let mut store = ContextStore::new();
        let id = push_sized(&mut store, 500);
        let (store, tool) = tool_over(store, KeepCaps::new(20, 100));

        let reply = reply_of(&tool, "condensed").await;
        assert_eq!(reply["evicted"], 1);
        assert_eq!(reply["kept_turns"], 0);
        assert_eq!(reply["degraded"], "newest turn exceeds max_tokens");
        assert!(reply["first_kept_turn"].is_null());
        assert!(
            reply["note"].is_null(),
            "the note is for configured condensing, not a lost cap: {reply}"
        );

        let guard = store.lock().await;
        assert_eq!(guard.usage_snapshot().turn_count, 0);
        let text = summary_text(&guard);
        assert!(text.contains(&format!("#{}", id.0)), "{text}");
    }

    /// A turn cap of 0 is the operator's configuration, not a lost cap: the
    /// reply notes it instead of reporting a degradation.
    #[tokio::test]
    async fn a_zero_turn_cap_reports_a_configured_full_condense() {
        let mut store = ContextStore::new();
        for _ in 0..3 {
            push_sized(&mut store, 10);
        }
        let (_, tool) = tool_over(store, wipe_all());

        let reply = reply_of(&tool, "condensed").await;
        assert_eq!(reply["evicted"], 3);
        assert_eq!(reply["kept_turns"], 0);
        assert!(reply["degraded"].is_null());
        assert!(
            reply["note"].as_str().unwrap().contains("max_turns is 0"),
            "the note names the configuration: {reply}"
        );
    }

    /// A window that already covers the conversation changes nothing: no summary
    /// write, no drain, and the reply says so.
    #[tokio::test]
    async fn a_window_covering_every_turn_leaves_the_pins_alone() {
        let mut store = ContextStore::new();
        let ids: Vec<TurnId> = (0..3).map(|_| push_sized(&mut store, 10)).collect();
        let (store, tool) = tool_over(store, KeepCaps::new(20, 1_000));

        let reply = reply_of(&tool, "condensed").await;
        assert_eq!(reply["evicted"], 0);
        assert_eq!(reply["kept_turns"], 3);
        assert_eq!(reply["first_kept_turn"], ids[0].0);
        assert!(reply["degraded"].is_null());
        assert!(
            reply["note"]
                .as_str()
                .unwrap()
                .contains("covers every turn"),
            "the reply says why nothing moved: {reply}"
        );

        let guard = store.lock().await;
        assert_eq!(guard.usage_snapshot().turn_count, 3);
        assert!(
            guard.pinned_labels().is_empty(),
            "no summary is pinned when there is nothing to condense"
        );
    }

    /// Both do-nothing answers — an empty conversation and a window that covers
    /// every turn — come back in one shape, differing only in the note.
    #[tokio::test]
    async fn an_empty_conversation_answers_in_the_same_shape_as_a_no_op() {
        let (_, empty_tool) = tool_over(ContextStore::new(), KeepCaps::new(20, 1_000));
        let empty = reply_of(&empty_tool, "nothing yet").await;

        let mut store = ContextStore::new();
        push_sized(&mut store, 10);
        let (_, tool) = tool_over(store, KeepCaps::new(20, 1_000));
        let no_op = reply_of(&tool, "nothing yet").await;

        assert_eq!(
            keys(&empty),
            keys(&no_op),
            "one shape for both do-nothing answers"
        );
        assert_eq!(empty["evicted"], 0);
        assert_eq!(empty["kept_turns"], 0);
        assert!(empty["first_kept_turn"].is_null());
        assert!(
            empty["note"].as_str().unwrap().contains("no turns"),
            "the empty answer says why: {empty}"
        );
        assert!(
            no_op["note"]
                .as_str()
                .unwrap()
                .contains("covers every turn"),
            "the no-op answer says why: {no_op}"
        );
    }

    /// Pinned items are neither evicted nor counted against either cap.
    #[tokio::test]
    async fn pinned_items_are_outside_the_window() {
        let mut store = ContextStore::new();
        store
            .pin("note", user_msg("keep me"))
            .expect("the pin fits");
        for _ in 0..3 {
            push_sized(&mut store, 10);
        }
        let (store, tool) = tool_over(store, KeepCaps::new(1, 1_000));

        let reply = reply_of(&tool, "condensed").await;
        assert_eq!(reply["evicted"], 2, "three turns, one kept");
        assert_eq!(reply["kept_turns"], 1);

        let guard = store.lock().await;
        assert!(guard.pinned_labels().iter().any(|l| l == "note"));
        assert_eq!(guard.usage_snapshot().turn_count, 1);
    }

    /// The caps come from the store: an argument of the same name is not read,
    /// so a model cannot widen its own window.
    #[tokio::test]
    async fn arguments_cannot_widen_the_window() {
        let mut store = ContextStore::new();
        for _ in 0..4 {
            push_sized(&mut store, 10);
        }
        let (_, tool) = tool_over(store, KeepCaps::new(2, 1_000));

        let reply: Value = serde_json::from_str(
            &run(
                &tool,
                json!({ "summary": "condensed", "max_turns": 4, "max_tokens": 1_000_000 }),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(reply["max_turns"], 2);
        assert_eq!(reply["evicted"], 2, "the configured window still binds");
    }

    /// The steady state: the store already carries a pinned summary, and a later
    /// call condenses the turns that arrived since. The new text replaces the old
    /// pin (there is never a second summary) and restates the range it now
    /// covers, which is what lets facts survive across repeated evictions.
    #[tokio::test]
    async fn a_second_eviction_replaces_the_summary_and_restates_the_range() {
        let mut store = ContextStore::new();
        let first: Vec<TurnId> = (0..4).map(|_| push_sized(&mut store, 10)).collect();
        let (store, tool) = tool_over(store, KeepCaps::new(2, 1_000));

        run(&tool, json!({ "summary": "first note" }))
            .await
            .unwrap();
        let later: Vec<TurnId> = {
            let mut guard = store.lock().await;
            (0..2).map(|_| push_sized(&mut guard, 10)).collect()
        };

        let reply = reply_of(&tool, "second note").await;
        assert_eq!(reply["evicted"], 2, "the turns the first call kept");
        assert_eq!(reply["kept_turns"], 2);
        assert_eq!(reply["first_kept_turn"], later[0].0);

        let guard = store.lock().await;
        let summaries = guard
            .turns()
            .iter()
            .filter(|t| t.is_pinned() && t.label() == Some(CONTEXT_SUMMARY_LABEL))
            .count();
        assert_eq!(summaries, 1, "the new summary replaces the old pin");
        let text = summary_text(&guard);
        assert!(text.ends_with("second note"), "{text}");
        assert!(
            text.contains(&format!("#{}", later[0].0)),
            "the range is restated for the window this call kept: {text}"
        );
        assert!(
            !text.contains(&format!("#{}", first[2].0)),
            "the replaced summary does not stand for the old boundary: {text}"
        );
    }

    /// A later call whose window already covers everything leaves the pinned
    /// summary exactly as the earlier call wrote it.
    #[tokio::test]
    async fn a_call_that_condenses_nothing_keeps_the_earlier_summary() {
        let mut store = ContextStore::new();
        for _ in 0..4 {
            push_sized(&mut store, 10);
        }
        let (store, tool) = tool_over(store, KeepCaps::new(2, 1_000));
        run(&tool, json!({ "summary": "first note" }))
            .await
            .unwrap();
        let pinned_before = summary_text(&*store.lock().await);

        {
            let mut guard = store.lock().await;
            guard.set_eviction_caps(KeepCaps::new(20, 1_000));
        }
        let reply = reply_of(&tool, "an unrelated note").await;
        assert_eq!(reply["evicted"], 0);

        let guard = store.lock().await;
        assert_eq!(
            summary_text(&guard),
            pinned_before,
            "a call that condenses nothing must not replace the summary"
        );
        assert!(summary_text(&guard).ends_with("first note"));
    }

    /// The summary is pinned before anything is drained, so a pin the budget
    /// cannot hold fails the whole call: no turn moves, and no summary lands.
    #[tokio::test]
    async fn a_pin_budget_too_small_for_the_summary_leaves_the_store_alone() {
        let mut store = ContextStore::new();
        for _ in 0..3 {
            push_sized(&mut store, 10);
        }
        store.set_pinned_budget(1);
        let (store, tool) = tool_over(store, KeepCaps::new(1, 1_000));

        let err = run(
            &tool,
            json!({ "summary": "a summary the pinned budget cannot hold" }),
        )
        .await
        .expect_err("the pin does not fit");
        assert!(
            format!("{err:#}").contains("pinned budget exceeded"),
            "{err:#}"
        );

        let guard = store.lock().await;
        assert_eq!(
            guard.usage_snapshot().turn_count,
            3,
            "the refused call drained nothing"
        );
        assert!(
            guard.pinned_labels().is_empty(),
            "the refused call left no summary behind"
        );
    }
}
