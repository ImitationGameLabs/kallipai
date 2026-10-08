//! The link domain: the agent SSE stream (`GET /tagmata/{id}/events`)
//! -- the tagma's delivery carrier for room-family events; `agent_sse`
//! holds the stream's auth and frame contract. The cross-domain seams
//! below are its only boundary.

mod agent_sse;

use axum::Router;

use kallipai_lesche_common::event::LescheAgentEvent;
use kallipai_lesche_common::rooms::MemberId;

use crate::state::{Registry, SharedConvState};

/// The link domain's data-plane routes.
pub(crate) fn router() -> Router<SharedConvState> {
    Router::new().merge(agent_sse::router())
}

// ---- Cross-domain seams (chat <-> link) ----
// Every cross-domain dependency crosses these named functions;
// nothing reaches into the other domain's modules.

/// Deliver one delivery-class event to a tagma's live agent stream.
/// Best-effort: `false` when the tagma is offline or its channel closed;
/// the caller counts the member missed and relies on the pull-back paths
/// (history re-pull; the membership pump's first tick on reconnect).
pub(crate) fn send_to_tagma(
    registry: &Registry,
    member: &MemberId,
    event: LescheAgentEvent,
) -> bool {
    registry
        .agent_stream_by_member(member)
        .is_some_and(|a| a.tx.send(event).is_ok())
}
