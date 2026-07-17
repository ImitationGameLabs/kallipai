//! Identifier newtypes for the agora subsystem.
//!
//! Each is a thin wrapper over a UUID string, defined via
//! [`kallip_common::id_type!`] (the same macro behind `AgentId`).

use kallip_common::id_type;

id_type! {
    /// Unique identifier for a registered agent tagma (one `kallip-daemon` instance).
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
