use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;

/// Maximum characters of the pinned message echoed back as a `preview`, so a
/// mismatch (e.g. the agent batched this with a read and pinned the wrong,
/// older result) is visible in the same turn rather than only next turn.
const PREVIEW_CHARS: usize = 200;

#[derive(Debug, Deserialize, Serialize)]
struct PinLastArgs {
    label: String,
    /// Which message to pin: `"tool-result"` (the most recent tool result) or
    /// `"assistant"` (your most recent assistant message with text content;
    /// pure tool-call dispatches are skipped).
    kind: String,
}

/// Tool that pins a message already in the agent's context **by reference**.
///
/// Whereas [`super::ContextPinTool`] is by-value (the agent supplies composed
/// content, and re-stating reinforces attention), `context_pin_last` is the
/// by-reference companion for content the agent has already received — a file
/// it read, a command output, or its own prior reply — where retyping would be
/// pure cost plus truncation risk. It resolves the most recent recorded
/// **conversation** (non-pinned) message of the requested kind and pins it
/// under a label.
///
/// Read-then-pin is two sequential turns: `record_turn` runs after the whole
/// tool batch, so a `context_pin_last` call batched in the same assistant
/// message as the read would resolve to an older result. Read first, then pin
/// in the next turn.
pub struct ContextPinLastTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl ContextPinLastTool {
    /// Tool name exposed to the LLM and referenced by the policy layer.
    pub const NAME: &str = "context_pin_last";

    pub fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }

    /// Map a pin kind to the message role it resolves to. Returns `None` for
    /// an unknown kind so the caller can produce a diagnostic error.
    fn role_for_kind(kind: &str) -> Option<&'static str> {
        match kind {
            "tool-result" => Some("tool"),
            "assistant" => Some("assistant"),
            _ => None,
        }
    }
}

#[async_trait]
impl LlmTool for ContextPinLastTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Pin the most recent message of a given kind from your context into \
         persistent context, by reference (no re-typing). Kinds: \
         `tool-result` (the most recent tool result — e.g. a file you just \
         read) or `assistant` (your most recent assistant message that has \
         text content — pure tool-call dispatches with no preamble are \
         skipped). MUST be called in a turn AFTER the message you want to pin, \
         never batched in the same assistant message as the read: the current \
         turn's results are not recorded until the turn ends, so a batched \
         call would pin an older message. For content you compose yourself \
         (decisions, constraints), use `context_pin` instead. A short preview \
         of the pinned message is returned so a mismatch is visible \
         immediately."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Unique identifier for this pinned item. Use 'skill:<name>' for loaded skills."
                },
                "kind": {
                    "type": "string",
                    "enum": ["tool-result", "assistant"],
                    "description": "Which message to pin: 'tool-result' or 'assistant'."
                }
            },
            "required": ["label", "kind"]
        })
    }

    async fn call(&self, args_json: &str) -> Result<String> {
        let args: PinLastArgs =
            serde_json::from_str(args_json).context("context_pin_last: invalid arguments")?;

        let role = Self::role_for_kind(&args.kind).ok_or_else(|| {
            anyhow::anyhow!(
                "context_pin_last: unknown kind '{}'; expected 'tool-result' or 'assistant'",
                args.kind
            )
        })?;

        let mut ctx = self.ctx.lock().await;
        // Scan and pin under one lock: avoid a TOCTOU where the "last" message
        // changes between resolution and pinning.
        let message = ctx.last_conversation_message_by_role(role).ok_or_else(|| {
            anyhow::anyhow!(
                "context_pin_last: no {} in recorded conversation turns. \
                 If you batched this call with a read, read the file in a prior \
                 turn first, then pin in the next turn.",
                args.kind
            )
        })?;
        let preview = message
            .content()
            .unwrap_or("")
            .chars()
            .take(PREVIEW_CHARS)
            .collect::<String>();
        ctx.pin(&args.label, message)?;
        let labels = ctx.pinned_labels();
        Ok(serde_json::to_string(&json!({
            "pinned": args.label,
            "kind": args.kind,
            "preview": preview,
            "pinned_labels": labels,
        }))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextStore;
    use just_llm_client::types::chat::{ChatMessage, ChatToolCall, FunctionCall, ToolType};

    /// Build a store wrapped in the trait object the tool expects.
    fn handle(store: ContextStore) -> Arc<Mutex<dyn AgenticContext>> {
        Arc::new(Mutex::new(store))
    }

    async fn run(tool: &ContextPinLastTool, args: &Value) -> Result<String> {
        tool.call(&serde_json::to_string(args).unwrap()).await
    }

    /// Minimal assistant tool-call dispatch with no preamble text (`content:
    /// None`) -- the shape the runner records for a round that emitted only
    /// tool calls.
    fn empty_dispatch() -> ChatMessage {
        ChatMessage::assistant_tool_calls(vec![ChatToolCall {
            id: "call_1".into(),
            kind: ToolType::Function,
            function: FunctionCall {
                name: "noop".into(),
                arguments: "{}".into(),
            },
        }])
    }

    #[tokio::test]
    async fn pins_last_tool_result_skipping_pinned() {
        let mut store = ContextStore::new();
        // A pinned assistant summary must be skipped.
        store
            .pin("context_summary", ChatMessage::assistant("summary"))
            .unwrap();
        // Conversation turns with an older tool result, then a newer one.
        store.push_turn(vec![ChatMessage::tool_result("old result", "call_1")]);
        store.push_turn(vec![ChatMessage::tool_result("new result", "call_2")]);
        let store = handle(store);

        let tool = ContextPinLastTool::new(store.clone());
        let out = run(&tool, &json!({"label":"skill:foo","kind":"tool-result"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["pinned"], "skill:foo");
        assert_eq!(v["preview"], "new result");
        assert!(
            store
                .lock()
                .await
                .pinned_labels()
                .contains(&"skill:foo".to_owned())
        );
    }

    #[tokio::test]
    async fn pins_last_assistant_not_the_summary() {
        let mut store = ContextStore::new();
        store
            .pin("context_summary", ChatMessage::assistant("THE SUMMARY"))
            .unwrap();
        store.push_turn(vec![ChatMessage::assistant("real last reply")]);
        let store = handle(store);

        let tool = ContextPinLastTool::new(store.clone());
        let out = run(&tool, &json!({"label":"note","kind":"assistant"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["preview"], "real last reply");
    }

    #[tokio::test]
    async fn sequential_turn_later_call_sees_recorded_turn() {
        // Simulates: turn 1 records a tool result; a later pin call (the next
        // "batch") must see that recorded result. Validates that resolution
        // reads recorded turns, not any in-flight batch.
        let mut store = ContextStore::new();
        store.push_turn(vec![ChatMessage::tool_result("from turn 1", "c1")]);
        let store = handle(store);

        let tool = ContextPinLastTool::new(store);
        let out = run(&tool, &json!({"label":"after","kind":"tool-result"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["preview"], "from turn 1");
    }

    #[tokio::test]
    async fn errors_when_no_message_of_kind_yet() {
        let store = handle(ContextStore::new());
        let tool = ContextPinLastTool::new(store);
        let err = run(&tool, &json!({"label":"x","kind":"tool-result"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no tool-result"),
            "diagnostic error expected: {err}"
        );
    }

    #[tokio::test]
    async fn errors_on_unknown_kind() {
        let store = handle(ContextStore::new());
        let tool = ContextPinLastTool::new(store);
        let err = run(&tool, &json!({"label":"x","kind":"user-prompt"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown kind"));
    }

    #[tokio::test]
    async fn assistant_skips_empty_dispatch_header() {
        // The newest assistant message is a pure tool-call dispatch (no
        // preamble, content: None) -- the shape the runner records mid-session.
        // The accessor must skip it and resolve to the prior content-bearing
        // assistant message.
        let mut store = ContextStore::new();
        store.push_turn(vec![ChatMessage::assistant("the reasoning")]);
        store.push_turn(vec![
            empty_dispatch(),
            ChatMessage::tool_result("r", "call_1"),
        ]);
        let store = handle(store);

        let tool = ContextPinLastTool::new(store);
        let out = run(&tool, &json!({"label":"note","kind":"assistant"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["preview"], "the reasoning");
    }

    #[tokio::test]
    async fn within_turn_reverse_order_picks_newest() {
        // A single turn carrying two tool results: the inner reverse scan must
        // resolve to the newer one. Locks t.messages.iter().rev() against a
        // future "simplification" to iter().
        let mut store = ContextStore::new();
        store.push_turn(vec![
            ChatMessage::tool_result("old within turn", "c1"),
            ChatMessage::tool_result("new within turn", "c2"),
        ]);
        let store = handle(store);

        let tool = ContextPinLastTool::new(store);
        let out = run(&tool, &json!({"label":"r","kind":"tool-result"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["preview"], "new within turn");
    }

    #[tokio::test]
    async fn assistant_returns_none_when_only_empty_dispatches() {
        // Only a content-None dispatch exists -- no content-bearing assistant
        // message to pin. The accessor returns None and the tool surfaces the
        // diagnostic error (no silent empty pin, no panic).
        let mut store = ContextStore::new();
        store.push_turn(vec![
            empty_dispatch(),
            ChatMessage::tool_result("r", "call_1"),
        ]);
        let store = handle(store);

        let tool = ContextPinLastTool::new(store);
        let err = run(&tool, &json!({"label":"x","kind":"assistant"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no assistant"),
            "diagnostic error expected: {err}"
        );
    }
}
