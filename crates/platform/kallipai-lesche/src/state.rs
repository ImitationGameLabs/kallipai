//! The relay's shared state: a handle to the registry ([`ControlPlane`]) plus
//! the in-memory soft-state `Registry` (app streams, agent streams).
//!
//! Everything here is soft-state, rebuilt on restart from clients
//! reconnecting. The durable identity / credential / provisioning layer
//! lives in the registry behind [`ControlPlane`]; this crate never reads or
//! writes it directly. The relay keeps NO replay/dedup window: `sequence_n`
//! is an end-to-end (app<->tagma) counter scoped to a crypto epoch the relay
//! cannot see, so replay protection lives entirely at the tagma (per-epoch
//! `seen_inbound` + AEAD key rotation).
//!
//! # Lock-discipline invariants (authoritative)
//!
//! 1. **No `.await` under a lock.** Drop every `read()`/`write()` guard
//!    before awaiting. `ControlPlane` calls (which await) happen outside any
//!    relay lock.
//! 2. **`app_streams` has a single creator: `me_events`** (`events.rs`).
//!    Inserting elsewhere would violate the `OnDrop` cleanup assumption (every
//!    entry has a live subscriber).
//!
//! # Domain layout
//!
//! The crate is organized into two domains along the line the
//! physical-split blueprint draws: `chat` (rooms, direct sessions,
//! the durable store) and `link` (the agent SSE carrier -- every surface
//! whose peer is a tagma). This registry deliberately spans both and stays
//! ONE lock: `agent_streams` is link-domain state and `app_streams` is the
//! shared events face, and the fan paths walk both maps under one read
//! lock, so a room fan never takes a second lock. Cross-domain calls go
//! only through the seams the two domain roots export (frame delivery
//! to an agent stream, presence fans); the one shared cache is
//! `agent_profiles` (filled lazily by the room/direct send paths on a
//! cache miss).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use kallipai_archeion_common::control_plane::ControlPlane;
use kallipai_archeion_common::ids::{ParticipantId, TagmaId, UserId};
use kallipai_common::protocol::ApiError;
use kallipai_lesche_common::event::{LescheAgentEvent, LescheEvent};
use kallipai_lesche_common::rooms::MemberId;
use tokio::sync::broadcast;

pub type SharedConvState = Arc<ConversationsState>;

/// Capacity of the per-tagma / per-user broadcast channel feeding an SSE stream.
pub const BROADCAST_CAPACITY: usize = 128;
/// One frame in flight on an app stream: the transport cursor (`epoch`, `seq`)
/// beside the untouched domain event. The SSE wire serializes `event` only —
/// the cursor rides the SSE `id:` field — so payload bytes are identical to
/// the earlier stream (additive framing; clients that ignore `id:` keep
/// working).
#[derive(Clone, Debug)]
pub struct SequencedAppEvent {
    /// The app-stream generation this frame was delivered on.
    pub epoch: u32,
    /// Per-stream delivery sequence. Allocated at send time under
    /// `send_lock`, so channel arrival order equals allocation order.
    pub seq: u64,
    /// The domain event; the only part that reaches the wire payload.
    pub event: LescheEvent,
}

/// One user's multiplexed app event stream: the broadcast channel plus the
/// transport cursor layers onto it. The registry holds it behind an `Arc`
/// so fan sites clone the handle under the registry lock and deliver outside
/// it (or under it — `send_lock` is a leaf lock, see below).
pub struct AppStream {
    /// Outbound broadcast ring (`BROADCAST_CAPACITY`), shared by every tab of
    /// the user (one stream per user), so all tabs observe
    /// identical `(epoch, seq)` per frame.
    pub tx: broadcast::Sender<SequencedAppEvent>,
    /// Stream generation. Changes when the channel is recreated (last tab
    /// left, then a reconnect) and across process restarts; on the wire an
    /// epoch change means "full resync", the same value means "same stream".
    pub epoch: u32,
    /// Next seq to allocate; monotonic within one channel incarnation.
    next_seq: AtomicU64,
    /// Serializes seq allocation with the send that publishes it — channel
    /// order equals seq order. Load-bearing, not insurance: envelope /
    /// presence / status fans run concurrently today, and lock-free
    /// allocation would let arrival order diverge from seq order, so clients
    /// would report false gaps (same rationale as the tagma bus ordering
    /// lock). Leaf lock: never acquires the registry lock, so a fan site may
    /// deliver while holding a registry guard with no inversion.
    send_lock: Mutex<()>,
}

/// `deliver` failed: the stream has no live receiver, so the frame — and its
/// just-consumed seq — landed nowhere. Deliberately unit-sized: the discarded
/// `SendError` would carry the whole 192-byte frame for no reader.
#[derive(Debug)]
pub struct DeliverError;

impl std::fmt::Display for DeliverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no live receiver on the app stream")
    }
}

impl std::error::Error for DeliverError {}

impl AppStream {
    fn new(epoch: u32) -> Self {
        Self {
            tx: broadcast::channel(BROADCAST_CAPACITY).0,
            epoch,
            next_seq: AtomicU64::new(0),
            send_lock: Mutex::new(()),
        }
    }

    /// Subscribe to this stream. Keeps the pre-`AppStream` call shape
    /// `open_app_stream(..).subscribe()` working for subscribers.
    pub fn subscribe(&self) -> broadcast::Receiver<SequencedAppEvent> {
        self.tx.subscribe()
    }

    /// Deliver one event: allocate its seq and publish the frame as one step
    /// under `send_lock`. `Err` means no live receiver; the seq is consumed
    /// anyway (benign, mirroring the tagma bus: an undelivered frame has no
    /// observer, so no cursor can observe the hole).
    pub fn deliver(&self, event: LescheEvent) -> Result<u64, DeliverError> {
        let _order = self.send_lock.lock().unwrap_or_else(|e| e.into_inner());
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.tx
            .send(SequencedAppEvent {
                epoch: self.epoch,
                seq,
                event,
            })
            .map_err(|_| DeliverError)?;
        Ok(seq)
    }

    /// Capture the open-stream marker values `(epoch, next_seq)`. Invariant:
    /// `next_seq` is the seq of the first frame allocated after this capture
    /// (the next value `deliver` hands out — with `fetch_add` front-return
    /// semantics that is exactly the current load). Reading under `send_lock`
    /// is what makes the marker gap-free: a frame allocated before the
    /// capture carries a seq strictly below `next_seq` (clients treat it as
    /// pre-adoption), a frame allocated after it continues at `next_seq`; a
    /// lock-free read could interleave with a concurrent allocation and
    /// manufacture a false gap.
    pub fn capture_marker(&self) -> (u32, u64) {
        let _order = self.send_lock.lock().unwrap_or_else(|e| e.into_inner());
        (self.epoch, self.next_seq.load(Ordering::Relaxed))
    }
}

/// The relay state. The registry is reached only through `control`; the rest is
/// in-memory, per-incarnation.
pub struct ConversationsState {
    /// The registry (identity + tagma metadata + replay guard), DB-backed in
    /// production by `kallipai-archeion`, mockable in tests.
    pub control: Arc<dyn ControlPlane>,
    pub registry: RwLock<Registry>,
    /// The durable chat store (`Some` in production): room messages + the
    /// membership graph. Required for room delivery and management.
    pub db: Option<crate::chat::db::Db>,
    /// Authoritative agent identity (label + owner display), keyed by the
    /// agent's `ParticipantId`, populated on first lookup and stamped onto
    /// room envelopes so a tagma cannot self-declare its handle. See
    /// [`AgentProfileCache`].
    pub agent_profiles: AgentProfileCache,
}

/// An agent's authoritative display identity, resolved from the registry and
/// stamped onto room envelopes/rows by the relay. Mirrors the display fields of
/// [`kallipai_archeion_common::control_plane::TagmaProfile`] minus the raw usability
/// facts + pinned key (which only the keryx proof check and the
/// local predicates need).
#[derive(Debug, Clone)]
pub struct AgentProfile {
    /// Read by the roster path (a follow-up); the message-stamp path builds the
    /// stable handle from `owner_username` only.
    #[allow(dead_code)]
    pub label: Option<String>,
    pub owner_username: String,
    /// Read by the roster/history path (a follow-up); the message-stamp path
    /// uses only `owner_username`.
    #[allow(dead_code)]
    pub owner_display_name: Option<String>,
}

/// Per-incarnation cache of resolved agent profiles, keyed by the agent's
/// `ParticipantId`. The send path's first lookup resolves a live
/// `tagma_profile` (one registry RPC) and caches it; later sends stamp
/// the sender handle without a per-message RPC. Stale on owner rename
/// for the cache's lifetime (acceptable: ownership is non-transferable
/// and the unforgeable id-prefix still disambiguates).
#[derive(Debug, Default)]
pub struct AgentProfileCache(std::sync::Mutex<HashMap<ParticipantId, AgentProfile>>);

impl AgentProfileCache {
    pub fn get(&self, pid: &ParticipantId) -> Option<AgentProfile> {
        self.0.lock().ok()?.get(pid).cloned()
    }
    pub fn set(&self, pid: ParticipantId, profile: AgentProfile) {
        if let Ok(mut g) = self.0.lock() {
            g.insert(pid, profile);
        }
    }
}

impl ConversationsState {
    /// Read-lock the registry, mapping poisoning into an HTTP 500.
    pub fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Registry>, ApiError> {
        self.registry
            .read()
            .map_err(|e| ApiError::internal(format_args!("registry lock poisoned: {e}")))
    }

    /// The durable store, or a 500. Room management mutates
    /// the durable chat graph, so it cannot degrade to in-memory mock mode
    /// the way delivery does (which skips persistence when no store is wired).
    pub fn require_db(&self) -> Result<&crate::chat::db::Db, ApiError> {
        self.db
            .as_ref()
            .ok_or_else(|| ApiError::internal(format_args!("chat store required")))
    }

    /// Write-lock the registry, mapping poisoning into an HTTP 500.
    pub fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Registry>, ApiError> {
        self.registry
            .write()
            .map_err(|e| ApiError::internal(format_args!("registry lock poisoned: {e}")))
    }
}

/// In-memory index of per-user app streams and per-tagma agent streams.
///
/// Both maps are keyed by [`ParticipantId`] (the opaque room-layer
/// identity): a user's app stream and a tagma's agent stream are both looked
/// up by their derived participant id, so the room fan-out routes every
/// member uniformly regardless of kind. The `TagmaId`/`UserId`-taking
/// helpers derive the participant id internally, so the app-stream paths
/// (`me_events`) and the agent-stream paths keep their existing call
/// shapes; the room fan-out uses the `_by_member` lookups.
pub struct Registry {
    /// participant_id -> the user's multiplexed app SSE stream (Human
    /// members). The sole creator is `me_events`; it carries agent envelopes,
    /// presence, status and room events. Private: mutate only via
    /// [`Registry::open_app_stream`] / [`Registry::remove_app_stream_if_last`].
    /// The `Arc` lets fan sites clone the handle under the registry lock and
    /// deliver outside it.
    app_streams: HashMap<ParticipantId, Arc<AppStream>>,
    /// Next epoch handed to a newly created app stream. Seeded from
    /// wall-clock nanos (see `Registry::new`) and bumped only on channel
    /// creation, so an epoch on the wire identifies one channel incarnation:
    /// same value = same stream, changed value = recreated stream or
    /// restarted server.
    next_app_stream_epoch: u32,
    /// Per-tagma live agent SSE streams (`GET /tagmata/{id}/events`): the
    /// sole delivery carrier for tagma-bound lesche events, and the
    /// online source the room roster reads (a member renders online
    /// while its agent stream is registered; global liveness reads the
    /// keryx tunnel edge, not this table). Private: mutated only via
    /// the agent-stream methods below.
    agent_streams: HashMap<ParticipantId, AgentStreamEntry>,
}

/// One live agent SSE stream: the outbound broadcast plus the
/// per-connection identity token that makes slot removal race-free
/// across reconnects.
pub struct AgentStreamEntry {
    pub tx: broadcast::Sender<LescheAgentEvent>,
    pub id: Arc<()>,
}
impl Registry {
    pub fn new() -> Self {
        Self {
            app_streams: HashMap::new(),
            agent_streams: HashMap::new(),
            // Seeded from wall-clock nanos so a restart cannot reuse an epoch a
            // surviving client cursor still holds. The u32 truncation wraps every
            // ~4.295 s (2^32 ns) — not a whole number of milliseconds, so two
            // process starts cannot land exactly one wrap apart even on a
            // millisecond-quantized clock, and real restart phases are random.
            // Do not "simplify" to a counter from 1: an epoch collision makes a
            // post-restart reconnect look seamless and every frame is then
            // silently ignored (seq below expected) — the exact loss the
            // cursor makes visible. Epoch 0 carries no sentinel meaning today.
            next_app_stream_epoch: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u32)
                // Clock before UNIX_EPOCH (broken clock): any non-zero constant
                // keeps construction infallible; the bump-per-creation counter
                // still separates streams within this process.
                .unwrap_or(0x9E37_79B9),
        }
    }

    /// The live app-stream handle for `user`, if any. Read-only access for
    /// routing events to the user's stream; creation is
    /// [`Self::open_app_stream`]. Keyed by the user's derived participant id.
    pub fn app_stream(&self, user: &UserId) -> Option<Arc<AppStream>> {
        self.app_streams
            .get(&ParticipantId::for_user(user))
            .cloned()
    }

    /// The live app-stream handle for a room member id (the room fan-out's
    /// Human-member lookup). The map keys on the underlying `ParticipantId`; a
    /// `MemberId` shares that UUID, so the lookup goes by the borrowed string.
    /// Takes the room-domain `MemberId` (not the raw `UserId`) so a caller
    /// cannot accidentally look up by an account id whose string is not the
    /// derived key.
    pub fn app_stream_by_member(&self, mid: &MemberId) -> Option<Arc<AppStream>> {
        self.app_streams.get(mid.as_ref()).cloned()
    }

    /// Ensure an app event stream exists for `user` and return its handle.
    /// Sole creator of `app_streams` entries; only a fresh
    /// channel consumes an epoch — an existing stream keeps its own, so a
    /// second tab joining mid-stream observes no epoch change.
    pub fn open_app_stream(&mut self, user: &UserId) -> Arc<AppStream> {
        if let Some(existing) = self.app_streams.get(&ParticipantId::for_user(user)) {
            return existing.clone();
        }
        self.next_app_stream_epoch = self.next_app_stream_epoch.wrapping_add(1);
        let stream = Arc::new(AppStream::new(self.next_app_stream_epoch));
        self.app_streams
            .insert(ParticipantId::for_user(user), stream.clone());
        stream
    }

    /// Remove `user`'s app-stream entry iff `stream` is down to its last
    /// subscriber (`receiver_count() == 1`: the dying SSE stream itself).
    /// Returns whether the entry was removed (the user has no remaining app
    /// stream) so the caller can fan a presence-offline transition only on
    /// the 1 -> 0 edge. The next `open_app_stream` creates a fresh channel
    /// with a fresh epoch — the wire signal for clients to full-resync.
    pub fn remove_app_stream_if_last(&mut self, user: &UserId, stream: &AppStream) -> bool {
        if stream.tx.receiver_count() == 1 {
            self.app_streams.remove(&ParticipantId::for_user(user));
            true
        } else {
            false
        }
    }

    /// Whether `user` currently holds an app stream. Named for intent at the
    /// 0 -> 1 edge detection in `me_events` (announce presence only on the first
    /// stream); semantically equivalent to `self.app_stream(user).is_some()`.
    /// Only meaningful under the registry write lock the caller already holds.
    pub fn has_app_stream(&self, user: &UserId) -> bool {
        self.app_streams
            .contains_key(&ParticipantId::for_user(user))
    }

    /// Register `tagma`'s live agent SSE stream. Returns `false` (slot left
    /// untouched) when a live stream already holds the slot; a slot whose
    /// channel has no receivers left is dead (its `OnDrop` cleanup never
    /// ran), so it is reaped and replaced here -- the fallback cleanup that
    /// keeps a leaked slot from 409-ing the tagma forever.
    pub fn register_agent_stream(
        &mut self,
        tagma: &TagmaId,
        tx: broadcast::Sender<LescheAgentEvent>,
        id: Arc<()>,
    ) -> bool {
        let pid = ParticipantId::for_tagma(tagma);
        if self
            .agent_streams
            .get(&pid)
            .is_some_and(|entry| entry.tx.receiver_count() > 0)
        {
            return false;
        }
        self.agent_streams.insert(pid, AgentStreamEntry { tx, id });
        true
    }

    /// The live agent-stream entry for a tagma. Test-facing probe: the
    /// runtime delivery lookups go through the `_by_member` form.
    #[cfg(test)]
    pub fn agent_stream_by_tagma(&self, tagma: &TagmaId) -> Option<&AgentStreamEntry> {
        self.agent_streams.get(&ParticipantId::for_tagma(tagma))
    }

    /// The live agent-stream entry for a room member id (the room fan-out
    /// and roster delivery lookup). Keyed by the member id string.
    pub fn agent_stream_by_member(&self, mid: &MemberId) -> Option<&AgentStreamEntry> {
        self.agent_streams.get(mid.as_ref())
    }

    /// Remove `tagma`'s agent stream iff the live entry is still `id` (Arc
    /// pointer identity), returning whether it was removed. Race-free
    /// across reconnects.
    pub fn take_agent_stream_if_owned(&mut self, tagma: &TagmaId, id: &Arc<()>) -> bool {
        let pid = ParticipantId::for_tagma(tagma);
        let still_ours = self
            .agent_streams
            .get(&pid)
            .map(|e| Arc::ptr_eq(&e.id, id))
            .unwrap_or(false);
        if still_ours {
            self.agent_streams.remove(&pid);
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
    use kallipai_archeion_common::ids::TagmaId;
    use kallipai_lesche_common::event::LescheEvent;

    /// Marker invariant: `capture_marker` returns the seq of the first frame
    /// allocated after the capture, and allocation continues densely there.
    #[tokio::test]
    async fn marker_next_seq_is_the_first_seq_allocated_after_the_capture() {
        let stream = AppStream::new(42);
        // Pre-capture frame with no receiver yet: the send errs and the seq is
        // consumed benignly (no observer exists to see the hole).
        let _ = stream.deliver(LescheEvent::TagmaOnline {
            tagma_id: TagmaId::from("a".to_string()),
        });
        let (epoch, next_seq) = stream.capture_marker();
        assert_eq!(epoch, 42);
        assert_eq!(next_seq, 1, "one frame was allocated before the capture");
        let mut rx = stream.subscribe();
        stream
            .deliver(LescheEvent::TagmaOffline {
                tagma_id: TagmaId::from("a".to_string()),
            })
            .expect("delivered");
        stream
            .deliver(LescheEvent::TagmaOnline {
                tagma_id: TagmaId::from("a".to_string()),
            })
            .expect("delivered");
        assert_eq!(rx.recv().await.expect("frame").seq, 1);
        assert_eq!(rx.recv().await.expect("frame").seq, 2);
    }

    /// No live receiver: `deliver` errs and the seq is consumed anyway —
    /// benign (no observer exists to see the hole), mirroring the tagma bus.
    #[test]
    fn deliver_without_a_receiver_consumes_the_seq_benignly() {
        let stream = AppStream::new(7);
        assert!(
            stream
                .deliver(LescheEvent::TagmaOnline {
                    tagma_id: TagmaId::from("a".to_string()),
                })
                .is_err(),
            "no receivers yet"
        );
        let _rx = stream.subscribe();
        let seq = stream
            .deliver(LescheEvent::TagmaOnline {
                tagma_id: TagmaId::from("a".to_string()),
            })
            .expect("delivered");
        assert_eq!(seq, 1, "the unobserved frame consumed seq 0");
    }

    /// An existing stream keeps its epoch (a second tab joining mid-stream
    /// observes no epoch change); only a recreated channel gets a fresh one.
    #[test]
    fn reopening_keeps_the_epoch_until_the_channel_is_recreated() {
        let mut reg = Registry::new();
        let alice = UserId::from("alice".to_string());
        let s1 = reg.open_app_stream(&alice);
        let e1 = s1.epoch;
        let s1b = reg.open_app_stream(&alice);
        assert!(Arc::ptr_eq(&s1, &s1b), "the second tab shares the stream");
        assert_eq!(s1b.epoch, e1, "an existing stream keeps its epoch");
        let _rx = s1.subscribe();
        assert!(
            reg.remove_app_stream_if_last(&alice, &s1),
            "the last subscriber leaves: the 1 -> 0 edge"
        );
        let s2 = reg.open_app_stream(&alice);
        assert_ne!(s2.epoch, e1, "a recreated channel gets a fresh epoch");
    }

    /// A lagged subscriber's dropped frames are visible as a seq jump: the
    /// ring keeps no history, so after the lag error the first frame the slow
    /// receiver gets carries a seq above anything it observed before (the id
    /// jump the client's resync policy keys on).
    #[tokio::test]
    async fn lagged_receiver_observes_the_seq_jump() {
        let stream = AppStream::new(3);
        let mut slow = stream.subscribe();
        stream
            .deliver(LescheEvent::TagmaOnline {
                tagma_id: TagmaId::from("a".to_string()),
            })
            .expect("delivered");
        let first = slow.recv().await.expect("frame");
        assert_eq!(first.seq, 0);

        // Overflow the ring (capacity 128): the slow receiver stalls at seq 0.
        for _ in 0..200 {
            stream
                .deliver(LescheEvent::TagmaOnline {
                    tagma_id: TagmaId::from("a".to_string()),
                })
                .expect("delivered");
        }
        use tokio::sync::broadcast::error::TryRecvError;
        let mut jumped = None;
        while jumped.is_none() {
            match slow.try_recv() {
                Ok(frame) => jumped = Some(frame.seq),
                Err(TryRecvError::Lagged(n)) => assert_eq!(n, 72, "200 sent, 128 retained"),
                Err(TryRecvError::Empty) => panic!("frames should be retained"),
                Err(TryRecvError::Closed) => panic!("the stream is alive"),
            }
        }
        assert_eq!(
            jumped,
            Some(73),
            "the first retained seq jumps from 0 to 73"
        );
    }
}
