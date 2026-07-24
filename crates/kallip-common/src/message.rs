//! The message-delivery marker shared by the `kallip lesche send` CLI emitter
//! and the local-client parsers (TUI, transcript reducers).
//!
//! The agent addresses the user by invoking `kallip lesche send` (a subcommand
//! of the `kallip` CLI) via `bash_exec`. The CLI prints a stable marker line to
//! its stdout so local clients render the message from the tool result; it also
//! POSTs the plaintext to the tagma's `POST /agents/{id}/lesche/messages`
//! route, which holds the E2E key in-process and posts the envelope to the
//! relay.

use serde::{Deserialize, Serialize};

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
}
