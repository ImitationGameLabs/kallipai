//! Sender identity for delivered inter-agent messages.
//!
//! When the daemon delivers a message via `POST /agents/{id}/message`, it
//! derives who sent it from the caller's auth identity and prepends a `[From:
//! ...]` header to the enqueued text, so the receiver knows whom to reply to
//! and how the sender relates to it. These types are daemon-internal: they are
//! never serialized over the wire (the header is baked into the enqueued
//! `String` before it reaches the prompt channel), so they live in the daemon,
//! not in `kallip-common`.

use kallip_common::agentid::AgentId;

/// Who a delivered message is from. Derived by the daemon from the caller's
/// auth identity, never supplied by the sender, so it cannot be spoofed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageSender {
    /// The human operator.
    Operator,
    /// A specific agent. `role` is the sender's display role captured at send
    /// time (looked up from the registry; `"unknown"` if the sender had already
    /// been unregistered, possibly empty for a root agent that never had one).
    Agent { id: AgentId, role: String },
}

/// Relationship of the sender to the receiving agent, computed by the daemon
/// from the supervisor (`created_by`) chains. Tells the receiver how to treat
/// the message (e.g. a `Superior` message is an instruction; a `Subordinate`
/// message is a report). `Unknown` is used only when neither a superior nor
/// subordinate relation could be established and a chain walk failed -- an
/// informational best-effort, never an authorization decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderRelation {
    Operator,
    Superior,
    Subordinate,
    Peer,
    Same,
    Unknown,
}

impl SenderRelation {
    /// Lowercase label for the `[From: ...]` header. Every variant has a label;
    /// the renderer suppresses it for the operator sender (which renders as just
    /// `[From: operator]`).
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Superior => "superior",
            Self::Subordinate => "subordinate",
            Self::Peer => "peer",
            Self::Same => "same",
            Self::Unknown => "unknown",
        }
    }
}

/// Render an incoming message with a `[From: ...]` header so the receiver knows
/// who sent it and how they relate. The header is bracketed to match the
/// daemon's existing notification convention (`[Interjected message]`,
/// `[Approval Request]`, ...) and to avoid colliding with user-authored text.
pub fn format_incoming(sender: &MessageSender, relation: SenderRelation, text: &str) -> String {
    let header = match sender {
        MessageSender::Operator => String::from("[From: operator]"),
        MessageSender::Agent { id, role } => {
            let role_display = if role.is_empty() {
                "<none>"
            } else {
                role.as_str()
            };
            format!(
                "[From: agent {id} (role: {role_display}, {})]",
                relation.as_label()
            )
        }
    };
    format!("{header}\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_header_has_no_relation() {
        let rendered = format_incoming(&MessageSender::Operator, SenderRelation::Operator, "hi");
        assert_eq!(rendered, "[From: operator]\nhi");
    }

    #[test]
    fn agent_header_includes_role_and_relation() {
        let sender = MessageSender::Agent {
            id: AgentId::from("a1".to_owned()),
            role: "researcher".to_owned(),
        };
        let rendered = format_incoming(&sender, SenderRelation::Superior, "do X");
        assert_eq!(
            rendered,
            "[From: agent a1 (role: researcher, superior)]\ndo X"
        );
    }

    #[test]
    fn empty_role_renders_none_placeholder() {
        let sender = MessageSender::Agent {
            id: AgentId::from("r".to_owned()),
            role: String::new(),
        };
        let rendered = format_incoming(&sender, SenderRelation::Peer, "hey");
        assert_eq!(rendered, "[From: agent r (role: <none>, peer)]\nhey");
    }
}
