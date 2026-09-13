//! The relay connector: the optional online-mode subsystem that links the
//! tagma to archeion/lesche. Folded in from the former standalone connector — the
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
mod flusher;
mod kex;
mod manage;
pub(crate) mod ops;
mod projection_pump;
mod pump;
pub(crate) mod room_poll;
pub(crate) mod status_pump;
mod tunnel;

use std::future::Future;
use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use futures_util::{FutureExt, StreamExt};
use kallip_archeion_common::bytes::Ciphertext;
use kallip_archeion_common::ids::{ConversationId, ParticipantId, ParticipantKind, TagmaId};
use kallip_e2ee::{self as e2e, DeviceKey};
use kallip_lesche_client::LescheClient;
use kallip_lesche_common::direct::{DirectMessage, DirectSessionId};
use kallip_lesche_common::message::{
    Envelope, Participant, RoomMessage, TagmaControl, TagmaReply, TagmaRequest,
};
use kallip_lesche_common::rooms::RoomId;
use kallip_lesche_common::tunnel::{Face, ManageRestReply, TunnelInbound};
use std::panic::AssertUnwindSafe;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use kallip_common::agentid::AgentId;

use crate::auth::Identity;
use crate::messaging::Surface;
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

/// Which lifecycle a pump slot is bound to. Declared at construction so the
/// binding class is data on the slot rather than prose on a field: a new
/// pump must name its binding explicitly, and the five usages are greppable.
enum PumpBinding {
    /// Restarted on each KEX: a re-KEX stops the pump and the fresh
    /// incarnation starts against the new key (see `handle_kex`).
    KexEpoch,
    /// Bounded to the tunnel session: started on tunnel-up, stopped on
    /// tunnel-down, so a reconnect installs a fresh incarnation.
    TunnelSession,
}

/// One background pump slot: idempotent start, stop-then-await, restartable.
/// Converges the five duplicated start/stop pairs (event, status, room,
/// projection, upstream flusher) into a single primitive.
///
/// Cancel-safety of [`PumpSlot::stop_and_await`], accepted as-is: if the
/// caller's future is dropped mid-await, the handle has already been taken
/// and its token cancelled, so the task exits on its own and the empty slot
/// accepts a fresh start; the only lost guarantee is that the completion was
/// observed. Same shape and strength as the code this converges.
struct PumpSlot {
    /// The binding class recorded at construction. Annotation only: nothing
    /// keys off it at runtime.
    #[allow(dead_code)]
    binding: PumpBinding,
    handle: Mutex<Option<PumpHandle>>,
}

impl PumpSlot {
    fn new(binding: PumpBinding) -> Self {
        Self {
            binding,
            handle: Mutex::new(None),
        }
    }

    /// Spawn the pump unless one is already live. Idempotent: returns
    /// `false` and spawns nothing when the slot is occupied. The token is
    /// created under the slot lock so a start racing a stop cannot orphan
    /// it; `make` receives it and builds the pump's own future (whose
    /// internal select stays the sole cancellation point).
    async fn get_or_spawn<F>(&self, make: impl FnOnce(CancellationToken) -> F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut slot = self.handle.lock().await;
        if slot.is_some() {
            return false;
        }
        let cancel = CancellationToken::new();
        let task = tokio::spawn(make(cancel.clone()));
        *slot = Some(PumpHandle { task, cancel });
        true
    }

    /// Stop and await the pump if it is running, clearing the slot so a
    /// later `get_or_spawn` can install a fresh one. The take happens under
    /// the lock; the cancel + await happen outside it.
    async fn stop_and_await(&self) {
        let handle = self.handle.lock().await.take();
        if let Some(handle) = handle {
            handle.cancel.cancel();
            let _ = handle.task.await;
        }
    }

    /// Whether a pump is currently installed in the slot. A test probe: the
    /// production code reads the slot only through start/stop.
    #[allow(dead_code)]
    async fn is_running(&self) -> bool {
        self.handle.lock().await.is_some()
    }
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
    /// The config entry name (stable slug, e.g. "main"): keys the relay's
    /// AppState slot and its joined-rooms slice, and stamps relay-scoped log
    /// lines so two concurrently connected archeions are distinguishable.
    name: String,
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
    pump: PumpSlot,
    /// The running status pump, if any. Bounded to the tunnel's lifetime
    /// (started on tunnel-up, stopped on tunnel-down), NOT the KEX epoch --
    /// status is plaintext and key-independent.
    status_pump: PumpSlot,
    /// The running room-membership poll pump, if any. Bounded to the tunnel
    /// session like the status pump: started on tunnel-up (an immediate first
    /// tick warms the joined-rooms cache after a reconnect), stopped on
    /// tunnel-down so a reconnect installs a fresh pump.
    room_pump: PumpSlot,
    /// The running projection push pump, if any. Bounded to the tunnel
    /// session like the other pumps (started on tunnel-up with an
    /// unconditional full first shot, stopped on tunnel-down), and
    /// additionally gated by the lesche's subscription hint so an unread
    /// projection costs nothing.
    projection_pump: PumpSlot,
    /// The upstream flusher: the single wire writer for the plaintext
    /// metadata uplink (status/projection/signal batches to the lesche's
    /// `/upstream` channel). Bounded to the tunnel session like the other
    /// pumps (the POST needs the KEX'd client); see relay/flusher.rs.
    upstream_flusher: PumpSlot,
    /// Whether the lesche currently has at least one projection subscriber
    /// (the projection face of `OwnerSub`; `false` until one arrives and
    /// after a `false`, the fail-toward-saving-resources default). Read by
    /// the pump before each push; written only from the tunnel dispatch.
    projection_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether the status face currently has a subscriber worth pushing
    /// to: the last per-face truth received for the status face (an
    /// `OwnerSub { face: Status }` frame, an `/upstream` piggyback
    /// count, or the tunnel-establishment re-learn). Defaults OPEN
    /// (an open default): an old lesche never signals the gate, so a closed
    /// default would silently gate status for the whole mixed-version
    /// window, while an erroneously-open gate self-corrects within one
    /// push's piggyback and a wrongly-closed gate has no carrier. The
    /// flag persists across tunnel reconnects (inner outlives the
    /// session); each establishment re-learns the truth, so a stale
    /// flag survives at most one session interval. Read by the flusher's
    /// batch build; written only from [`RelayHandle::handle_status_sub`].
    status_push_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Wake edge for the status gate: the flusher selects on this so a
    /// false -> true flip pushes the retained snapshot immediately
    /// instead of waiting out the cadence. Notified only on an open
    /// flip; closed flips need no wake (nothing is due).
    status_gate_notify: std::sync::Arc<tokio::sync::Notify>,
    /// Connection-lifetime monotonic push counter for the projection pump
    /// (never reset by a pump restart -- the lesche keys same-generation
    /// replay rejection on it; a fresh tunnel session is a new generation,
    /// which the lesche accepts unconditionally). Incremented per push.
    projection_push_seq: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Fallback tick cadence for the projection pump, in milliseconds
    /// (30000 in production; tests shorten it).
    projection_fallback_ms: std::sync::atomic::AtomicU64,
    /// Retry cadence for the upstream flusher's retained snapshot events,
    /// in milliseconds (2000 in production; tests shorten it).
    flusher_retry_ms: std::sync::atomic::AtomicU64,
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
        name: String,
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
                name,
                tagma_label,
                conversation_id,
                client,
                device,
                root_agent,
                crypto: Mutex::new(CryptoState::new()),
                pump: PumpSlot::new(PumpBinding::KexEpoch),
                status_pump: PumpSlot::new(PumpBinding::TunnelSession),
                room_pump: PumpSlot::new(PumpBinding::TunnelSession),
                projection_pump: PumpSlot::new(PumpBinding::TunnelSession),
                upstream_flusher: PumpSlot::new(PumpBinding::TunnelSession),
                projection_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                status_push_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                status_gate_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
                projection_push_seq: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                projection_fallback_ms: std::sync::atomic::AtomicU64::new(30_000),
                flusher_retry_ms: std::sync::atomic::AtomicU64::new(2_000),
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
    /// the plaintext straight to `/rooms/{room}/envelopes`) can reach the
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
