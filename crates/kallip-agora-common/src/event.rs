//! Events on the app's multiplexed SSE stream (`GET /v1/me/events`).
//!
//! A single per-user connection carries envelope deliveries for all of the
//! user's conversations plus presence transitions, multiplexed by
//! `conversation_id` / `team_id`.
//!
//! Key exchange is NOT delivered here: it is a synchronous request/reply on
//! `POST /v1/conversations/{id}/key-exchange/init`, whose response body carries
//! the herald's signed key-exchange response directly.
//!
//! The presence variants (`TeamOnline`, `TeamOffline`, `AgentState`) are part
//! of the wire contract but reserved: until presence transitions are wired, the
//! app polls `/v1/teams`. They are kept here so the app SDK's deserializer is
//! stable from day one.

use crate::ids::TeamId;
use crate::message::Envelope;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::AgentState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgoraEvent {
    /// An envelope was delivered for one of the user's conversations.
    Envelope { envelope: Envelope },
    /// A team came online (its herald established a live, key-verified tunnel).
    #[allow(dead_code)]
    TeamOnline { team_id: TeamId },
    /// A team went offline (tunnel dropped, past the reconnect grace window).
    #[allow(dead_code)]
    TeamOffline { team_id: TeamId },
    /// A surfaced agent's lifecycle state changed.
    #[allow(dead_code)]
    AgentState {
        team_id: TeamId,
        agent_id: AgentId,
        state: AgentState,
    },
}
