//! Inbound frames the relay pushes down a tagma's tunnel. The tunnel is the
//! tagma's only inbound channel, so it carries both forwarded data-plane
//! envelopes and app-initiated key-exchange inits (the control channel that
//! runs *before* a conversation has an E2E key).

use crate::control::KeyExchangeInit;
use crate::message::Envelope;
use kallip_agora_common::ids::ConversationId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TunnelInbound {
    /// A forwarded E2E envelope for a conversation this tagma owns.
    Envelope { envelope: Envelope },
    /// An app wants to establish a conversation E2E key. The tagma derives the
    /// shared secret and replies with a signed
    /// [`crate::control::KeyExchangeResponse`]. The agent that backs the
    /// conversation is the tagma's own concern and is not carried here.
    KeyExchange {
        conversation_id: ConversationId,
        init: KeyExchangeInit,
    },
}
