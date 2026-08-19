//! The message-delivery marker shared by the `kallip lesche send` CLI emitter
//! and the local-client parsers (TUI, transcript reducers).
//!
//! The agent addresses the user by invoking `kallip lesche send` (a subcommand
//! of the `kallip` CLI) via `bash_exec`. The CLI prints a stable marker line to
//! its stdout so local clients render the message from the tool result; it also
//! POSTs the plaintext to the tagma's `POST /agents/{id}/lesche/messages`
//! route, which holds the E2E key in-process and posts the envelope to the
//! relay.
//!
//! [`message_sent_line`] is the sibling echo for `kallip message` (agent-to-agent)
//! sends; its key is deliberately distinct from the lesche marker.

use serde::{Deserialize, Serialize};

/// Wire key of the lesche message marker (see [`marker_line`]). Shared by
/// the CLI emitter and the local-client parsers so the two cannot drift.
pub const LESCHE_MESSAGE_KEY: &str = "kallip.lesche.message";

/// Wire key of the agent-message send echo (see [`message_sent_line`]).
/// Deliberately distinct from [`LESCHE_MESSAGE_KEY`] so clients can tell
/// the two apart.
pub const MESSAGE_SENT_KEY: &str = "kallip.message.sent";

/// The stable stdout marker the CLI always prints, exactly one JSON line:
/// `{"kallip.lesche.message":{"text":"<message>"}}`. Recognized by clients in a
/// tool result to render the message as an assistant chat line.
pub fn marker_line(text: &str) -> String {
    // Serialize so the text is escaped correctly (newlines, quotes, etc.).
    #[derive(Serialize)]
    struct Message<'a> {
        text: &'a str,
    }
    #[derive(Serialize)]
    struct Wrapper<'a> {
        #[serde(rename = "kallip.lesche.message")]
        kallip_message: Message<'a>,
    }
    serde_json::to_string(&Wrapper {
        kallip_message: Message { text },
    })
    .unwrap_or_else(|_| r#"{"kallip.lesche.message":{"text":""}}"#.to_owned())
}

/// The stable stdout echo `kallip message` prints after a successful send,
/// exactly one JSON line:
/// `{"kallip.message.sent":{"to":"<id>","text":"<message>","queue_depth":N}}`
/// (plus `"warning"` when the tagma included one). The key is deliberately
/// distinct from the lesche marker: local clients (TUI, transcript reducers)
/// match on `kallip.lesche.message` to render user chat lines, and must not
/// render an agent-to-agent send as one.
pub fn message_sent_line(
    to: &str,
    text: &str,
    queue_depth: usize,
    warning: Option<&str>,
) -> String {
    #[derive(Serialize)]
    struct Sent<'a> {
        to: &'a str,
        text: &'a str,
        queue_depth: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        warning: Option<&'a str>,
    }
    #[derive(Serialize)]
    struct Wrapper<'a> {
        #[serde(rename = "kallip.message.sent")]
        sent: Sent<'a>,
    }
    serde_json::to_string(&Wrapper {
        sent: Sent {
            to,
            text,
            queue_depth,
            warning,
        },
    })
    .unwrap_or_else(|_| r#"{"kallip.message.sent":{"to":"","text":"","queue_depth":0}}"#.to_owned())
}

/// Find the lesche message marker in a tool-result string, returning the
/// message text on a match, `None` for any other result.
///
/// The marker object embeds [`LESCHE_MESSAGE_KEY`]; it may sit anywhere in
/// the result (the CLI's stdout line verbatim, wrapped by the executor's
/// `{"ok":true,...}` envelope, or inside shell wrappers), so the needle is
/// matched anywhere and the enclosing `{...}` object is then parsed — the
/// brace walk tolerates nested objects and string escapes.
pub fn find_lesche_message(result: &str) -> Option<String> {
    let needle = format!("\"{LESCHE_MESSAGE_KEY}\"");
    let idx = result.find(needle.as_str())?;
    let start = result[..idx].rfind('{')?;
    // Walk braces from `start` to the matching close, tolerating nested
    // objects and strings, then extract `[LESCHE_MESSAGE_KEY].text`.
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, ch) in result[start..].char_indices() {
        if in_str {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let obj: serde_json::Value =
                        serde_json::from_str(&result[start..start + i + 1]).ok()?;
                    return obj
                        .get(LESCHE_MESSAGE_KEY)
                        .and_then(|m| m.get("text"))
                        .and_then(|t| t.as_str())
                        .map(String::from);
                }
            }
            _ => {}
        }
    }
    None
}
/// The tagma lesche-message route's response to a delivered message.
#[derive(Debug, Serialize, Deserialize)]
pub struct DeliveryResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_line_escapes_correctly() {
        let m = marker_line("hello\nworld \"quoted\"");
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(
            v["kallip.lesche.message"]["text"],
            "hello\nworld \"quoted\""
        );
    }

    #[test]
    fn marker_line_handles_empty() {
        let m = marker_line("");
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["kallip.lesche.message"]["text"], "");
    }
    #[test]
    fn message_sent_line_escapes_and_skips_warning() {
        let s = message_sent_line("id-1", "hi `whoami` \"q\"\nnewline", 2, None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kallip.message.sent"]["to"], "id-1");
        assert_eq!(
            v["kallip.message.sent"]["text"],
            "hi `whoami` \"q\"\nnewline"
        );
        assert_eq!(v["kallip.message.sent"]["queue_depth"], 2);
        assert!(v["kallip.message.sent"].get("warning").is_none());
    }

    #[test]
    fn message_sent_line_includes_warning_and_distinct_key() {
        let s = message_sent_line("id-1", "queued", 3, Some("queued behind 3"));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kallip.message.sent"]["warning"], "queued behind 3");
        // For ordinary text the lesche marker key does not appear in the echo
        // at all; the pathological case (text equal to the key) is covered
        // by `message_sent_echo_is_not_a_lesche_marker` below.
        assert!(!s.contains("kallip.lesche.message"));
    }

    #[test]
    fn marker_keys_match_serialized_lines() {
        let m = marker_line("x");
        assert!(m.contains(&format!("\"{LESCHE_MESSAGE_KEY}\":")));
        let s = message_sent_line("id", "x", 0, None);
        assert!(s.contains(&format!("\"{MESSAGE_SENT_KEY}\":")));
    }

    #[test]
    fn message_sent_echo_is_not_a_lesche_marker() {
        // The needle stage hits (the key appears as a JSON value), but the
        // brace walk yields the `kallip.message.sent` object, which has no
        // `kallip.lesche.message` key — so no user chat line renders.
        let echo = message_sent_line("id-1", LESCHE_MESSAGE_KEY, 0, None);
        assert!(find_lesche_message(&echo).is_none());
    }

    #[test]
    fn find_lesche_message_extracts_bare_and_wrapped() {
        assert_eq!(
            find_lesche_message(&marker_line("hi \"q\"")),
            Some("hi \"q\"".into())
        );
        // The executor embeds the CLI's stdout line as a structured
        // `result` object inside its envelope.
        let wrapped = format!(
            "{{\"ok\":true,\"tool_name\":\"bash_exec\",\"result\":{}}}",
            marker_line("wrapped")
        );
        assert_eq!(find_lesche_message(&wrapped), Some("wrapped".into()));
        // Any other tool result stays unrecognized.
        assert_eq!(find_lesche_message("{\"ok\":true,\"result\":1}"), None);
    }
}
