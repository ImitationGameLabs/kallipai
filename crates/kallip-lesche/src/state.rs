//! The relay's shared state: a handle to the registry ([`ControlPlane`]) plus
//! the in-memory soft-state `Registry` (presence, conversations, app streams)
//! and the relay-only `pending_key_exchange` window.
//!
//! Everything here is soft-state, rebuilt on restart (presence from heralds
//! reconnecting, conversations create-on-demand). The durable identity /
//! credential / provisioning layer lives in the registry behind
//! [`ControlPlane`]; this crate never reads or writes it directly. The relay
//! keeps NO replay/dedup window: `sequence_n` is an end-to-end (app<->
//! herald) counter scoped to a crypto epoch the relay cannot see, so replay
//! protection lives entirely at the herald (per-epoch `seen_inbound` + AEAD
//! key rotation).
//!
//! # Lock-discipline invariants (authoritative)
//!
//! 1. **No `.await` under a lock.** Drop every `read()`/`write()`/
//!    `pending_key_exchange` guard before awaiting. `ControlPlane` calls (which
//!    await) happen outside any relay lock.
//! 2. **Never co-hold the registry lock with `pending_key_exchange`.** Register
//!    a KEX waiter only after the registry guard is dropped.
//! 3. **`app_streams` has a single creator: `me_events`** (`routes/events.rs`).
//!    Inserting elsewhere would violate the `OnDrop` cleanup assumption (every
//!    entry has a live subscriber).
//! 4. **`pending_key_exchange` cleanup is unconditional** via `KexGuard`
//!    (`routes/conversations.rs`).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use kallip_agora_common::control::KeyExchangeResponse;
use kallip_agora_common::control_plane::ControlPlane;
use kallip_agora_common::event::AgoraEvent;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TagmaId, UserId};
use kallip_common::protocol::ApiError;
use tokio::sync::{broadcast, oneshot};

pub type SharedConvState = Arc<ConversationsState>;

/// Capacity of the per-tagma / per-user broadcast channel feeding an SSE stream.
pub const BROADCAST_CAPACITY: usize = 128;

/// The relay state. The registry is reached only through `control`; the rest is
/// in-memory, per-incarnation.
pub struct ConversationsState {
    /// The registry (identity + tagma metadata + replay guard), DB-backed in
    /// production by `kallip-agora`, mockable in tests.
    pub control: Arc<dyn ControlPlane>,
    pub registry: RwLock<Registry>,
    /// Outstanding synchronous key exchanges, keyed by conversation. Bounded by
    /// in-flight KEX. Never held together with the registry lock.
    pub pending_key_exchange:
        std::sync::Mutex<HashMap<ConversationId, oneshot::Sender<KeyExchangeResponse>>>,
    /// Acceptable clock skew (both directions) on a tunnel reconnect proof's
    /// timestamp, in seconds.
    pub proof_skew_secs: i64,
    /// How long `key_exchange_init` waits for the herald's response before 504.
    pub key_exchange_timeout: Duration,
}

impl ConversationsState {
    /// Read-lock the registry, mapping poisoning into an HTTP 500.
    pub fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Registry>, ApiError> {
        self.registry
            .read()
            .map_err(|e| ApiError::internal(format_args!("registry lock poisoned: {e}")))
    }

    /// Write-lock the registry, mapping poisoning into an HTTP 500.
    pub fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Registry>, ApiError> {
        self.registry
            .write()
            .map_err(|e| ApiError::internal(format_args!("registry lock poisoned: {e}")))
    }
}

/// In-memory index of presence, conversations, and per-user app streams.
pub struct Registry {
    pub conversations: HashMap<ConversationId, ConversationRecord>,
    /// tagma_id -> the tagma's live herald tunnel. A tagma is "online" iff it
    /// has an entry. `owner` routes presence events; `id` is a per-connection
    /// identity token so a stale tunnel's cleanup cannot remove a freshly
    /// reconnected tunnel's presence.
    pub presence: HashMap<TagmaId, PresenceEntry>,
    /// user_id -> outbound broadcast to their multiplexed app SSE. The sole
    /// creator is `me_events`; it carries agent envelopes and presence events.
    /// Private: mutate only via [`Registry::open_app_stream`] /
    /// [`Registry::remove_app_stream_if_last`].
    app_streams: HashMap<UserId, broadcast::Sender<AgoraEvent>>,
}

/// One live herald tunnel: the outbound broadcast, the owning user (for presence
/// routing), and a per-connection identity token used to make presence removal
/// race-free across reconnects.
pub struct PresenceEntry {
    pub tx: broadcast::Sender<HeraldInbound>,
    pub owner: UserId,
    pub id: Arc<()>,
}

#[derive(Debug, Clone)]
pub struct ConversationRecord {
    pub owner: UserId,
    pub tagma_id: TagmaId,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            conversations: HashMap::new(),
            presence: HashMap::new(),
            app_streams: HashMap::new(),
        }
    }

    /// The live app event-stream sender for `user`, if any. Read-only access for
    /// routing agent envelopes and presence events; creation is
    /// [`Self::open_app_stream`].
    pub fn app_stream(&self, user: &UserId) -> Option<&broadcast::Sender<AgoraEvent>> {
        self.app_streams.get(user)
    }

    /// Ensure an app event-stream channel exists for `user` and return a sender
    /// clone. Sole creator of `app_streams` entries.
    pub fn open_app_stream(&mut self, user: &UserId) -> broadcast::Sender<AgoraEvent> {
        self.app_streams
            .entry(user.clone())
            .or_insert_with(|| broadcast::channel::<AgoraEvent>(BROADCAST_CAPACITY).0)
            .clone()
    }

    /// Remove `user`'s app-stream channel iff `sender` is the last subscriber
    /// (`receiver_count() == 1`: the dying SSE stream itself).
    pub fn remove_app_stream_if_last(
        &mut self,
        user: &UserId,
        sender: &broadcast::Sender<AgoraEvent>,
    ) {
        if sender.receiver_count() == 1 {
            self.app_streams.remove(user);
        }
    }

    /// Ensure the soft-state conversation record exists for `tagma_id` owned by
    /// `owner`, and return its stable id. Idempotent (the id is the
    /// deterministic `ConversationId::for_tagma` derivation). Sole mutator of
    /// `conversations`.
    pub fn ensure_conversation(&mut self, owner: &UserId, tagma_id: &TagmaId) -> ConversationId {
        let conv_id = ConversationId::for_tagma(tagma_id);
        self.conversations
            .entry(conv_id.clone())
            .or_insert(ConversationRecord {
                owner: owner.clone(),
                tagma_id: tagma_id.clone(),
            });
        conv_id
    }

    /// Register a live herald tunnel for `tagma`, owned by `owner`, capturing the
    /// per-connection identity token `id` so a stale tunnel's cleanup cannot
    /// remove a fresh reconnect's presence.
    pub fn register_presence(
        &mut self,
        tagma: &TagmaId,
        owner: UserId,
        tx: broadcast::Sender<HeraldInbound>,
        id: Arc<()>,
    ) {
        self.presence
            .insert(tagma.clone(), PresenceEntry { tx, owner, id });
    }

    /// Remove `tagma`'s presence iff the live entry is still `id` (Arc pointer
    /// identity), returning whether it was removed. Race-free across reconnects.
    pub fn take_presence_if_owned(&mut self, tagma: &TagmaId, id: &Arc<()>) -> bool {
        let still_ours = self
            .presence
            .get(tagma)
            .map(|p| Arc::ptr_eq(&p.id, id))
            .unwrap_or(false);
        if still_ours {
            self.presence.remove(tagma);
        }
        still_ours
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_agora_common::event::AgoraEvent;
    use kallip_agora_common::ids::TagmaId;

    /// `register_presence` stores the owner so presence events can be routed to
    /// the owning user's app stream, and the snapshot iteration (what
    /// `me_events` emits on stream open) filters by owner.
    #[test]
    fn presence_records_owner_and_snapshot_filters_by_owner() {
        let mut reg = Registry::new();
        let alice = UserId::from("alice".to_string());
        let bob = UserId::from("bob".to_string());
        let a1 = TagmaId::from("a1".to_string());
        let a2 = TagmaId::from("a2".to_string());
        let b1 = TagmaId::from("b1".to_string());
        let (tx_a1, _) = broadcast::channel::<HeraldInbound>(8);
        let (tx_a2, _) = broadcast::channel::<HeraldInbound>(8);
        let (tx_b1, _) = broadcast::channel::<HeraldInbound>(8);
        reg.register_presence(&a1, alice.clone(), tx_a1, Arc::new(()));
        reg.register_presence(&a2, alice.clone(), tx_a2, Arc::new(()));
        reg.register_presence(&b1, bob.clone(), tx_b1, Arc::new(()));

        // Snapshot for alice = her two tagmas.
        let alice_online: Vec<TagmaId> = reg
            .presence
            .iter()
            .filter(|(_, e)| e.owner == alice)
            .map(|(t, _)| t.clone())
            .collect();
        assert_eq!(alice_online.len(), 2);
        assert!(alice_online.contains(&a1) && alice_online.contains(&a2));
    }

    /// The presence-push wiring: an open app stream for the owner receives a
    /// `TagmaOnline` sent to the owner's sender (the path `tunnel`/`me_events`
    /// take on connect / stream open).
    #[tokio::test]
    async fn app_stream_receives_presence_event() {
        let mut reg = Registry::new();
        let alice = UserId::from("alice".to_string());
        let tx = reg.open_app_stream(&alice);
        let mut rx = tx.subscribe();
        // Simulate the tunnel handler's online announcement.
        tx.send(AgoraEvent::TagmaOnline {
            tagma_id: TagmaId::from("a1".to_string()),
        })
        .expect("send");
        let ev = rx.recv().await.expect("receive");
        assert!(matches!(ev, AgoraEvent::TagmaOnline { .. }));
    }
}
