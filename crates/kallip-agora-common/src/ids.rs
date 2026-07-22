//! Identifier newtypes for the agora subsystem.
//!
//! Each is a thin wrapper over a UUID string, defined via
//! [`kallip_common::id_type!`] (the same macro behind `AgentId`).

use kallip_common::id_type;
use uuid::Uuid;

id_type! {
    /// Unique identifier for a registered agent tagma (one `kallip-tagma` instance).
    TagmaId
}
id_type! {
    /// Unique identifier for a user account.
    UserId
}
id_type! {
    /// Unique identifier for a conversation.
    ConversationId
}
id_type! {
    /// Distributed-trace identifier propagated on envelopes. The agora passes it
    /// through unchanged so relay and endpoints can be correlated at the telemetry
    /// backend.
    TraceId
}

/// Namespace UUID for the deterministic `ConversationId` <- `TagmaId` derivation.
/// Pinned so the agora (producer of `TagmaView.conversation_id`) and any client
/// reproducing the derivation agree byte-for-byte. Change only by introducing a
/// new namespace and migrating.
const CONVERSATION_NAMESPACE: Uuid = Uuid::from_u128(0xd8e2e7c4_5a91_4b3f_8c2d_6e7a8b9c0d1e);

impl ConversationId {
    /// Derive the stable conversation id for a tagma.
    ///
    /// One tagma owns exactly one conversation (its single channel to its
    /// owner): the conversation id is a v5 UUID over this tagma's id string, so
    /// it is stable across reconnects and agora restarts and requires no
    /// storage. Reconnects re-KEX on the *same* id to rotate the E2E key and
    /// reset the sequence window.
    pub fn for_tagma(tagma_id: &TagmaId) -> Self {
        Self(Uuid::new_v5(&CONVERSATION_NAMESPACE, tagma_id.as_ref().as_bytes()).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_tagma_is_deterministic_and_invariant() {
        let t = TagmaId::from("tagma-abc".to_string());
        // Same input -> same id.
        assert_eq!(ConversationId::for_tagma(&t), ConversationId::for_tagma(&t));
        // Different input -> different id.
        let t2 = TagmaId::from("tagma-xyz".to_string());
        assert_ne!(
            ConversationId::for_tagma(&t),
            ConversationId::for_tagma(&t2)
        );
        // Produces a real UUID string.
        ConversationId::for_tagma(&t)
            .as_ref()
            .parse::<Uuid>()
            .expect("derived conversation id is a UUID");
    }
}
