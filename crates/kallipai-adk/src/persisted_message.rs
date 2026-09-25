//! serde boundary for persisted `Message` values (`pins.json`, history NDJSON).
//!
//! Two generations of on-disk shape meet here:
//!
//! - The **generation face** (current): an internally tagged enum whose assistant
//!   variant carries flat `tool_calls` entries (`id`/`name`/`arguments`) and a
//!   structured `reasoning` object. Upstream skips an empty `tool_calls` array when
//!   serializing (`skip_serializing_if`) yet requires the field when deserializing,
//!   so the face's own output would not read back (upstream gap, tracked as F5).
//! - The **0.2.0 chat face** (legacy, already on disk): assistant tool calls nested
//!   as `{"id", "type", "function": {"name", "arguments"}}` with a bare string
//!   `reasoning_content` alongside.
//!
//! Every persisted `Message` passes through the serde `with` modules below: writes
//! always emit `tool_calls` for assistant messages (so we can read our own output
//! back), and reads normalize legacy shapes into the generation face. The in-memory
//! model and the provider wire face are untouched — this is a persistence-boundary
//! concern only.

use just_llm_client::types::generation::Message;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// serde `with` module for a single `Message` (e.g. a pin record).
pub(crate) mod message {
    pub(super) use super::{deserialize_one, serialize_one};

    use just_llm_client::types::generation::Message;
    use serde::{Deserializer, Serializer};
    pub(crate) fn serialize<S: Serializer>(
        msg: &Message,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serialize_one(msg, serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Message, D::Error> {
        deserialize_one(deserializer)
    }
}

/// serde `with` module for a `Vec<Message>` (history records).
pub(crate) mod message_vec {
    use just_llm_client::types::generation::Message;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        msgs: &[Message],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Serialize::serialize(
            &msgs
                .iter()
                .map(|m| super::message_to_value(m).map_err(serde::ser::Error::custom))
                .collect::<Result<Vec<_>, _>>()?,
            serializer,
        )
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Message>, D::Error> {
        let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
        values
            .into_iter()
            .map(|v| super::message_from_value(v).map_err(serde::de::Error::custom))
            .collect()
    }
}

fn serialize_one<S: Serializer>(msg: &Message, serializer: S) -> Result<S::Ok, S::Error> {
    let value = message_to_value(msg).map_err(serde::ser::Error::custom)?;
    value.serialize(serializer)
}

fn deserialize_one<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Message, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    message_from_value(value).map_err(serde::de::Error::custom)
}

fn message_to_value(msg: &Message) -> serde_json::Result<serde_json::Value> {
    let mut value = serde_json::to_value(msg)?;
    if value["role"] == "assistant" {
        value
            .as_object_mut()
            .expect("an assistant message serializes to a JSON object")
            .entry("tool_calls")
            .or_insert(serde_json::Value::Array(Vec::new()));
    }
    Ok(value)
}

fn message_from_value(value: serde_json::Value) -> serde_json::Result<Message> {
    let mut value = value;
    if value["role"] == "assistant" {
        normalize_legacy_assistant(&mut value);
    }
    serde_json::from_value(value)
}

/// Rewrite a legacy 0.2.0 assistant message in place: nested `function` tool-call
/// entries are flattened, the bare `reasoning_content` string is folded into the
/// structured `reasoning` object, and a missing `tool_calls` field (plain-text
/// reply) is filled with an empty array. Generation-shaped input passes through
/// unchanged except for that fill.
fn normalize_legacy_assistant(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if let Some(calls) = obj.get_mut("tool_calls").and_then(|c| c.as_array_mut()) {
        for call in calls {
            flatten_legacy_tool_call(call);
        }
    }
    if let Some(reasoning_content) = obj.remove("reasoning_content")
        && reasoning_content.is_string()
        && !obj.contains_key("reasoning")
    {
        obj.insert(
            "reasoning".to_owned(),
            serde_json::json!({ "text": reasoning_content }),
        );
    }
    obj.entry("tool_calls")
        .or_insert(serde_json::Value::Array(Vec::new()));
}

/// Flatten one legacy tool call `{"id", "type", "function": {"name", "arguments"}}`
/// into the generation face's `{"id", "name", "arguments"}`. Already-flat entries
/// pass through untouched.
fn flatten_legacy_tool_call(call: &mut serde_json::Value) {
    let Some(obj) = call.as_object_mut() else {
        return;
    };
    let Some(function) = obj.remove("function") else {
        return; // already flat
    };
    obj.remove("type");
    if let Some(name) = function.get("name") {
        obj.insert("name".to_owned(), name.clone());
    }
    if let Some(arguments) = function.get("arguments") {
        obj.insert("arguments".to_owned(), arguments.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use just_llm_client::types::generation::{Reasoning, ToolCall};

    #[test]
    fn plain_assistant_write_carries_empty_tool_calls_and_round_trips() {
        // Upstream's own output omits `tool_calls` when empty and then rejects it
        // on read; the boundary must make the write self-readable. (F5)
        let msg = Message::assistant("summary text");
        let json = serde_json::to_value(message_to_value(&msg).unwrap()).unwrap();
        assert_eq!(json["tool_calls"], serde_json::json!([]));
        let back = message_from_value(json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn legacy_assistant_text_with_reasoning_content_reads_back() {
        let legacy = serde_json::json!({
            "role": "assistant",
            "content": "old reply",
            "reasoning_content": "done thinking"
        });
        let back = message_from_value(legacy).unwrap();
        assert_eq!(back.role(), "assistant");
        assert_eq!(back.content(), Some("old reply"));
        assert_eq!(
            back.reasoning().and_then(|r| r.text.as_deref()),
            Some("done thinking")
        );
        assert!(back.tool_calls().is_empty());
    }

    #[test]
    fn legacy_tool_result_reads_back() {
        let legacy = serde_json::json!({
            "role": "tool",
            "content": "42",
            "tool_call_id": "c1"
        });
        let back = message_from_value(legacy).unwrap();
        assert_eq!(back.role(), "tool");
        assert_eq!(back.tool_call_id(), Some("c1"));
        assert_eq!(back.content(), Some("42"));
    }

    #[test]
    fn legacy_nested_tool_calls_read_back_flat() {
        let legacy = serde_json::json!({
            "role": "assistant",
            "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]
        });
        let back = message_from_value(legacy).unwrap();
        assert_eq!(back.tool_calls().len(), 1);
        assert_eq!(back.tool_calls()[0].id, "c1");
        assert_eq!(back.tool_calls()[0].name, "f");
        assert_eq!(back.tool_calls()[0].arguments, "{}");
    }

    #[test]
    fn legacy_user_text_reads_back() {
        let legacy = serde_json::json!({"role": "user", "content": "hello"});
        let back = message_from_value(legacy).unwrap();
        assert_eq!(back.role(), "user");
        assert_eq!(back.content(), Some("hello"));
    }

    #[test]
    fn generation_shaped_assistant_round_trips() {
        let msg = Message::assistant_tool_calls(
            Some("calling".to_owned()),
            vec![ToolCall {
                id: "c1".to_owned(),
                name: "break".to_owned(),
                arguments: "{}".to_owned(),
            }],
            Some(Reasoning {
                text: Some("thought".to_owned()),
                ..Reasoning::default()
            }),
        );
        let value = message_to_value(&msg).unwrap();
        let back = message_from_value(value).unwrap();
        assert_eq!(back, msg);
    }
}
