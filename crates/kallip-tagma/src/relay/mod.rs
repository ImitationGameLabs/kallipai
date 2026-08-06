//! The relay connector: the optional online-mode subsystem that links the
//! tagma to agora/lesche. Folded in from the former standalone connector — the
//! tagma now hosts it in-process.
//!
//! Responsibilities (ported from the former standalone connector):
//! - hold the lesche tunnel SSE + reconnect loop (`run` / `connect_and_drain`);
//! - broker the per-conversation E2E key (KEX) and the AEAD epoch (`handle_kex`,
//!   `crypto::CryptoState`);
//! - decrypt inbound app ops and run them against the root agent in-process
//!   (`handle_user_op` / `execute_op`);
//! - forward the external projector's authored/signal bus onto the relay:
//!   encrypted envelope for authored content, plaintext for signals (`pump`).
//!
//! The projector (see [`crate::external`]) is the sole writer of chat content;
//! this module only encrypts + posts. The E2E key never leaves this process.
//! `Inner` holds a `Weak<AppState>` (not a strong ref) to avoid a reference
//! cycle with `AppState.relay`.

mod bilateral;
pub(crate) mod chat_history;
mod crypto;
mod dispatch;
mod kex;
pub(crate) mod ops;
mod pump;
pub(crate) mod room_poll;
pub(crate) mod status_pump;
mod tunnel;

use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use futures_util::{FutureExt, StreamExt};
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::ids::{ConversationId, ParticipantId, ParticipantKind, RoomId, TagmaId};
use kallip_e2ee::{self as e2e, DeviceKey};
use kallip_lesche_client::LescheClient;
use kallip_lesche_common::message::{
    Envelope, Participant, RoomMessage, TagmaControl, TagmaReply, TagmaRequest,
};
use kallip_lesche_common::tunnel::TunnelInbound;
use std::panic::AssertUnwindSafe;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use kallip_common::agentid::AgentId;

use crate::auth::Identity;
use crate::state::{AppState, SharedState};

use crypto::CryptoState;
use ops::{op_err_reply, op_trace};

/// The room-membership poll cadence. Slow: membership changes are rare and a
/// stale cache entry is a transient routing miss (recovered on the next tick),
/// not a correctness emergency. The interval's first tick is immediate, so a
/// tunnel-(re)connect triggers a sweep right away, and a `Wake` nudge from the
/// lesche re-polls outside the cadence.
const ROOM_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Re-exported so `activate_relay` (main.rs) can construct the configured limits.
pub(crate) use ops::{DEFAULT_MESSAGE_BURST_MAX, DEFAULT_MESSAGE_BURST_WINDOW, MessageLimits};

/// Error from a message-delivery attempt.
#[derive(Debug, thiserror::Error)]
pub enum RelayMessageError {
    /// The process-global message burst cap was exceeded.
    #[error("message burst cap exceeded")]
    BurstExceeded,
    /// Encrypting / posting the envelope failed.
    #[error(transparent)]
    Delivery(#[from] anyhow::Error),
}

/// The running pump task plus the token that stops it.
struct PumpHandle {
    task: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub struct RelayHandle {
    inner: Arc<Inner>,
}

/// A room envelope's decoded content: the relay-authenticated sender id and the
/// plaintext `RoomMessage` JSON bytes. Rooms are plaintext server-readable, so
/// there is no room crypto step -- the relay fork extracts these directly from
/// the envelope.
pub(crate) struct RoomPayload {
    pub(crate) sender_id: String,
    pub(crate) plaintext: Vec<u8>,
}

struct Inner {
    tagma_id: TagmaId,
    /// The enrolled label (or a fallback) used to stamp the agent sender's
    /// handle on outbound envelopes.
    tagma_label: String,
    conversation_id: ConversationId,
    /// The data-plane relay client (tunnel SSE, envelope + KEX POSTs). Owns the
    /// two reqwest clients internally (POST 30s + stream no-total-timeout).
    client: LescheClient,
    device: DeviceKey,
    root_agent: AgentId,
    /// AEAD session key + both sequence counters, under one lock.
    crypto: Mutex<CryptoState>,
    /// The running event pump, if any. Restarted on each KEX so a re-KEX can
    /// reset the outbound counter with no in-flight emits under the old key.
    pump: Mutex<Option<PumpHandle>>,
    /// The running status pump, if any. Bounded to the tunnel's lifetime
    /// (started on tunnel-up, stopped on tunnel-down), NOT the KEX epoch --
    /// status is plaintext and key-independent.
    status_pump: Mutex<Option<PumpHandle>>,
    /// The running room-membership poll pump, if any. Bounded to the tunnel
    /// session like the status pump: started on tunnel-up (an immediate first
    /// tick warms the joined-rooms cache after a reconnect), stopped on
    /// tunnel-down so a reconnect installs a fresh pump.
    room_pump: Mutex<Option<PumpHandle>>,
    /// In-flight per-envelope op tasks, so shutdown can abort and drain them
    /// rather than leaving them fire-and-forget. See [`RelayHandle::stop_dispatch`].
    dispatch: Mutex<tokio::task::JoinSet<()>>,
    /// `Weak` to break the `RelayHandle` ↔ `AppState` reference cycle. Upgraded
    /// at call time; `None` during shutdown → the op fails gracefully.
    state: Weak<AppState>,
}

impl RelayHandle {
    pub fn new(
        client: LescheClient,
        tagma_id: TagmaId,
        tagma_label: String,
        device: DeviceKey,
        root_agent: AgentId,
        state: Weak<AppState>,
    ) -> Self {
        let conversation_id = ConversationId::for_tagma(&tagma_id);
        Self {
            inner: Arc::new(Inner {
                tagma_id,
                tagma_label,
                conversation_id,
                client,
                device,
                root_agent,
                crypto: Mutex::new(CryptoState::new()),
                pump: Mutex::new(None),
                status_pump: Mutex::new(None),
                room_pump: Mutex::new(None),
                dispatch: Mutex::new(tokio::task::JoinSet::new()),
                state,
            }),
        }
    }

    /// The agent sender for outbound envelopes / op-replies: the tagma id + its
    /// label.
    pub fn agent_sender(&self) -> Participant {
        Participant {
            id: ParticipantId::for_tagma(&self.inner.tagma_id),
            kind: ParticipantKind::Agent,
            handle: self.inner.tagma_label.clone(),
            tagma_id: Some(self.inner.tagma_id.clone()),
        }
    }

    /// The data-plane relay client (cheap clone -- `Arc` inside). Exposed so a
    /// route that bypasses the bilateral projector (the room send, which posts
    /// the plaintext straight to `/v1/rooms/{room}/envelopes`) can reach the
    /// relay without going through the projector's frame bus.
    pub fn lesche_client(&self) -> LescheClient {
        self.inner.client.clone()
    }

    /// The tagma id this relay enrolled as (for diagnostics / compose wiring).
    pub fn tagma_id(&self) -> &TagmaId {
        &self.inner.tagma_id
    }
}

#[cfg(test)]
mod op_tests;
#[cfg(test)]
mod room_poll_tests;
