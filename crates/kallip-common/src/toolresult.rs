//! The tool-result envelope the runtime emits and local clients parse.
//!
//! Every tool call's result reaches the LLM transcript (and from there the
//! TUI) wrapped in one JSON object: success `{"ok":true,"tool_name":...,
//! "result":...}`, error `{"ok":false,...,"error":...}`, or
//! approval-deferred `{"ok":true,...,"pending_approval":true,...}`. The
//! single type keeps producer and consumers from drifting apart silently.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One envelope for all three tool-result shapes; unset fields serialize
/// away, so a success envelope is exactly `{"ok":true,"tool_name":...,
/// "result":...}` on the wire.
///
/// `rest` carries the open part of the protocol (the approval `id` and
/// `next_steps` today): producers set what they know, parsers keep what
/// they do not model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultEnvelope {
    pub ok: bool,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<bool>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(ok: bool, tool_name: &str) -> ToolResultEnvelope {
        ToolResultEnvelope {
            ok,
            tool_name: tool_name.to_owned(),
            result: None,
            error: None,
            pending_approval: None,
            rest: Map::new(),
        }
    }

    #[test]
    fn success_shape_is_exact() {
        let mut e = envelope(true, "bash_exec");
        e.result = Some(serde_json::json!({"stdout": "x", "exit_code": 0}));
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"ok":true,"tool_name":"bash_exec","result":{"stdout":"x","exit_code":0}}"#
        );
    }

    #[test]
    fn error_shape_is_exact() {
        let mut e = envelope(false, "cat");
        e.error = Some("denied".to_owned());
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"ok":false,"tool_name":"cat","error":"denied"}"#
        );
    }

    #[test]
    fn deferred_carries_rest_and_round_trips() {
        let mut e = envelope(true, "bash_exec");
        e.pending_approval = Some(true);
        e.rest.insert("id".into(), serde_json::json!("ap_1"));
        e.rest.insert(
            "next_steps".into(),
            serde_json::json!("call approval_commit"),
        );
        let s = serde_json::to_string(&e).unwrap();
        // Known fields serialize first, `rest` keys after, insertion-ordered.
        assert_eq!(
            s,
            r#"{"ok":true,"tool_name":"bash_exec","pending_approval":true,"id":"ap_1","next_steps":"call approval_commit"}"#
        );
        let back: ToolResultEnvelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn parses_legacy_key_order() {
        // Envelopes written before the shared type put pending_approval
        // second; JSON key order carries no meaning for parsing.
        let e: ToolResultEnvelope = serde_json::from_str(
            r#"{"ok":true,"pending_approval":true,"tool_name":"bash_exec","id":"ap_1","next_steps":"go"}"#,
        )
        .unwrap();
        assert!(e.ok);
        assert_eq!(e.pending_approval, Some(true));
        assert_eq!(e.rest["id"], "ap_1");
    }

    #[test]
    fn rejects_non_envelope_input() {
        assert!(serde_json::from_str::<ToolResultEnvelope>("not json").is_err());
        assert!(serde_json::from_str::<ToolResultEnvelope>("{\"stdout\":1}").is_err());
    }
}
