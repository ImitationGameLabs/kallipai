//! Messages the agora pushes to a herald over its tunnel. The tunnel is the
//! herald's only inbound channel, so it carries both forwarded data-plane
//! envelopes and app-initiated key-exchange inits (the control channel that
//! runs *before* a conversation has an E2E key).

use crate::control::KeyExchangeInit;
use crate::ids::ConversationId;
use crate::message::Envelope;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HeraldInbound {
    /// A forwarded E2E envelope for a conversation the herald's tagma owns.
    Envelope { envelope: Envelope },
    /// An app wants to establish a conversation E2E key. The herald derives the
    /// shared secret and replies with a signed
    /// [`crate::control::KeyExchangeResponse`]. The agent that backs the
    /// conversation is the herald's own concern and is not carried here.
    KeyExchange {
        conversation_id: ConversationId,
        init: KeyExchangeInit,
    },
}
