use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use just_llm_client::types::chat::ChatMessage;
use kallip_common::toolresult::ToolResultEnvelope;
use serde::{Deserialize, Serialize};

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;

/// Maximum characters of the call arguments shown in the card's source
/// header — enough to recognize the call, never the whole payload.
const CARD_COMMAND_CHARS: usize = 100;

/// Maximum characters of the pinned card echoed back as a `preview`, so a
/// mismatch (e.g. the agent batched this with a read and pinned the wrong,
/// older result) is visible in the same turn rather than only next turn.
const PREVIEW_CHARS: usize = 200;

#[derive(Debug, Deserialize, Serialize)]
struct PinLastArgs {
    label: String,
}

/// Tool that pins the agent's most recent tool result **by reference**.
///
/// Whereas [`super::ContextPinTool`] is by-value (the agent supplies composed
/// content, and re-stating reinforces attention), `context_pin_last` is the
/// by-reference companion for tool output the agent has already received —
/// a file it read, a command output — where retyping would be pure cost plus
/// truncation risk.
///
/// The pinned item is a self-contained user message (a "reference card"):
/// a one-line source header naming the tool and its call arguments, then the
/// result payload. It carries no `tool_call_id`/`tool_calls`, so the pinned
/// copy stays provider-valid even after the round that produced the result
/// leaves the window — the pin no longer depends on its pairing surviving.
///
/// Read-then-pin is two sequential turns: `record_turn` runs after the whole
/// tool batch, so a `context_pin_last` call batched in the same assistant
/// message as the call would resolve to an older result. Read first, then
/// pin in the next turn.
pub struct ContextPinLastTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl ContextPinLastTool {
    /// Tool name exposed to the LLM and referenced by the policy layer.
    pub const NAME: &str = "context_pin_last";

    pub fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LlmTool for ContextPinLastTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Pin your most recent tool result into persistent context, by reference (no re-typing) — e.g. a file you just read, a command output. \\
            The pinned item is a self-contained reference card: a source header naming the tool and its call, then the result payload. \\
            MUST be called in a turn AFTER the result you want to pin, never batched in the same assistant message as the call: \\
            the current turn's results are not recorded until the turn ends, so a batched call would pin an older result. \\
            For content you compose yourself (decisions, constraints), use `context_pin` instead. \\
            A short preview of the pinned card is returned so a mismatch is visible immediately."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Unique identifier for this pinned item. Use 'skill:<name>' for loaded skills."
                }
            },
            "required": ["label"]
        })
    }

    async fn call(&self, args_json: &str) -> Result<String> {
        let args: PinLastArgs =
            serde_json::from_str(args_json).context("context_pin_last: invalid arguments")?;

        let mut ctx = self.ctx.lock().await;
        // Scan and pin under one lock: avoid a TOCTOU where the "last" tool
        // result changes between resolution and pinning.
        let message = ctx
            .last_conversation_message_by_role("tool")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "context_pin_last: no tool result in recorded conversation turns. \
                 If you batched this call with a read, read the file in a prior \
                 turn first, then pin in the next turn."
                )
            })?;
        let call = message.tool_call_id().and_then(|id| ctx.tool_call_info(id));
        let card = reference_card(&message, call.as_ref());
        let preview: String = card.chars().take(PREVIEW_CHARS).collect();
        ctx.pin(&args.label, ChatMessage::user(card))?;
        let labels = ctx.pinned_labels();
        Ok(serde_json::to_string(&json!({
            "pinned": args.label,
            "preview": preview,
            "pinned_labels": labels,
        }))?)
    }
}

/// Build the pinned reference card for a tool result: a one-line source
/// header (`[pinned tool result · <tool> · <command>]`) over the result
/// payload. `call` is the `(name, arguments)` of the pairing call, present
/// while the round that produced the result is still in the window.
///
/// Never fails: every step degrades — an unparseable envelope pins the full
/// body, a missing `result.output` falls back to the whole content, and a
/// missing pairing call degrades the header to `command unavailable`.
fn reference_card(message: &ChatMessage, call: Option<&(String, String)>) -> String {
    let content = message.content().unwrap_or_default();
    let envelope = serde_json::from_str::<ToolResultEnvelope>(content).ok();
    let tool = envelope
        .as_ref()
        .map(|e| e.tool_name.as_str())
        .unwrap_or("unknown tool");
    let command = match call {
        Some((_, args)) => truncate_chars(args.lines().next().unwrap_or(""), CARD_COMMAND_CHARS),
        None => "command unavailable".to_owned(),
    };
    let payload = envelope
        .as_ref()
        .and_then(|e| e.result.as_ref())
        .and_then(|r| r.get("output"))
        .and_then(Value::as_str)
        .unwrap_or(content);
    format!("[pinned tool result · {tool} · {command}]\n\n{payload}")
}

/// First `max` chars of `s`, ellipsized when truncated.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut truncated: String = s.chars().take(max).collect();
        truncated.push('…');
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextStore;
    use just_llm_client::types::chat::{ChatToolCall, FunctionCall, ToolType};

    fn handle(store: ContextStore) -> Arc<Mutex<dyn AgenticContext>> {
        Arc::new(Mutex::new(store))
    }

    async fn run(tool: &ContextPinLastTool, args: &Value) -> Result<String> {
        tool.call(&serde_json::to_string(args).unwrap()).await
    }

    fn envelope(tool: &str, output: Option<&str>) -> String {
        serde_json::to_string(&ToolResultEnvelope {
            ok: true,
            tool_name: tool.to_owned(),
            result: output.map(|o| json!({ "output": o })),
            error: None,
            pending_approval: None,
            rest: serde_json::Map::new(),
        })
        .unwrap()
    }

    fn dispatch(id: &str, name: &str, args: &str) -> ChatMessage {
        ChatMessage::assistant_tool_calls(vec![ChatToolCall {
            id: id.into(),
            kind: ToolType::Function,
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }])
    }

    /// The card carries the envelope's tool name, the pairing call's first
    /// arguments line, and the extracted result.output payload — a
    /// self-contained user message with no tool protocol fields.
    #[test]
    fn reference_card_uses_envelope_and_pairing_call() {
        let msg = ChatMessage::tool_result(envelope("read_file", Some("FILE BODY")), "c1");
        let call = ("read_file".to_owned(), "{\"path\":\"a.txt\"}".to_owned());
        let card = reference_card(&msg, Some(&call));
        let mut lines = card.lines();
        assert_eq!(
            lines.next().unwrap(),
            "[pinned tool result · read_file · {\"path\":\"a.txt\"}]"
        );
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some("FILE BODY"));
    }

    #[test]
    fn reference_card_degrades_when_call_missing() {
        let msg = ChatMessage::tool_result(envelope("bash", Some("out")), "c1");
        let card = reference_card(&msg, None);
        assert!(card.starts_with("[pinned tool result · bash · command unavailable]"));
    }

    #[test]
    fn reference_card_truncates_long_commands() {
        let long = "x".repeat(150);
        let msg = ChatMessage::tool_result(envelope("t", Some("o")), "c1");
        let card = reference_card(&msg, Some(&("t".to_owned(), long)));
        let header = card.lines().next().unwrap();
        let cmd = header
            .trim_end_matches(']')
            .rsplit('·')
            .next()
            .unwrap()
            .trim();
        assert_eq!(cmd.chars().count(), 101, "100 chars + ellipsis: {cmd}");
        assert!(cmd.ends_with('…'));
    }

    /// Every payload fallback pins the full body rather than failing: not
    /// JSON, an envelope without result.output, and a non-string output all
    /// degrade to the whole content.
    #[test]
    fn reference_card_falls_back_to_full_content() {
        let plain = ChatMessage::tool_result("plain text", "c1");
        let card = reference_card(&plain, None);
        assert!(card.starts_with("[pinned tool result · unknown tool · command unavailable]"));
        assert!(card.ends_with("plain text"));

        let no_output = ChatMessage::tool_result(envelope("t", None), "c1");
        let card = reference_card(&no_output, None);
        assert!(card.contains(envelope("t", None).as_str()));

        let raw = serde_json::to_string(&ToolResultEnvelope {
            ok: true,
            tool_name: "t".into(),
            result: Some(json!({ "output": 42 })),
            error: None,
            pending_approval: None,
            rest: serde_json::Map::new(),
        })
        .unwrap();
        let msg = ChatMessage::tool_result(raw.clone(), "c1");
        let card = reference_card(&msg, None);
        assert!(card.contains(raw.as_str()));
    }

    #[tokio::test]
    async fn pins_reference_card_skipping_pinned_and_resolving_call() {
        let mut store = ContextStore::new();
        // A pinned assistant summary must be skipped by the scan.
        store
            .pin("context_summary", ChatMessage::assistant("summary"))
            .unwrap();
        store.push_turn(vec![
            dispatch("c1", "read_file", "{\"path\":\"old.txt\"}"),
            ChatMessage::tool_result(envelope("read_file", Some("OLD")), "c1"),
        ]);
        store.push_turn(vec![
            dispatch("c2", "read_file", "{\"path\":\"new.txt\"}"),
            ChatMessage::tool_result(envelope("read_file", Some("NEW BODY")), "c2"),
        ]);
        let store = handle(store);

        let tool = ContextPinLastTool::new(store.clone());
        let out = run(&tool, &json!({ "label": "skill:foo" })).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["pinned"], "skill:foo");
        // The preview carries the NEW result's card header — the pairing
        // call was resolved from the same recorded turn.
        let preview = v["preview"].as_str().unwrap();
        assert!(preview.contains("[pinned tool result · read_file · {\"path\":\"new.txt\"}]"));
        assert!(preview.contains("NEW BODY"));
        assert!(
            store
                .lock()
                .await
                .pinned_labels()
                .contains(&"skill:foo".to_owned())
        );
    }

    /// Old callers that still send kind=tool-result keep working: serde
    /// ignores unknown fields, so the parameter removal is compatible.
    #[tokio::test]
    async fn legacy_kind_argument_is_ignored() {
        let mut store = ContextStore::new();
        store.push_turn(vec![ChatMessage::tool_result(
            envelope("t", Some("o")),
            "c1",
        )]);
        let store = handle(store);
        let tool = ContextPinLastTool::new(store);
        let out = run(&tool, &json!({ "label": "x", "kind": "tool-result" }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["pinned"], "x");
    }

    #[tokio::test]
    async fn within_turn_reverse_order_picks_newest() {
        // A single turn carrying two tool results: the inner reverse scan
        // must resolve to the newer one.
        let mut store = ContextStore::new();
        store.push_turn(vec![
            ChatMessage::tool_result(envelope("t", Some("old within turn")), "c1"),
            ChatMessage::tool_result(envelope("t", Some("new within turn")), "c2"),
        ]);
        let store = handle(store);
        let tool = ContextPinLastTool::new(store);
        let out = run(&tool, &json!({ "label": "r" })).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["preview"].as_str().unwrap().contains("new within turn"));
    }

    #[tokio::test]
    async fn errors_when_no_tool_result_yet() {
        let store = handle(ContextStore::new());
        let tool = ContextPinLastTool::new(store);
        let err = run(&tool, &json!({ "label": "x" })).await.unwrap_err();
        assert!(
            err.to_string().contains("no tool result"),
            "diagnostic error expected: {err}"
        );
    }
}
