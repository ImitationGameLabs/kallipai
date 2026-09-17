//! The relay's shared state: a handle to the registry ([`ControlPlane`]) plus
//! the in-memory soft-state `Registry` (presence, conversations, app streams)
//! and the relay-only `pending_key_exchange` window.
//!
//! Everything here is soft-state, rebuilt on restart (presence from tagmata
//! reconnecting, conversations create-on-demand). The durable identity /
//! credential / provisioning layer lives in the registry behind
//! [`ControlPlane`]; this crate never reads or writes it directly. The relay
//! keeps NO replay/dedup window: `sequence_n` is an end-to-end (app<->
//! tagma) counter scoped to a crypto epoch the relay cannot see, so replay
//! protection lives entirely at the tagma (per-epoch `seen_inbound` + AEAD
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use kallip_archeion_common::control_plane::ControlPlane;
use kallip_archeion_common::ids::{ConversationId, ParticipantId, TagmaId, UserId};
use kallip_common::protocol::ApiError;
use kallip_lesche_common::control::KeyExchangeResponse;
use kallip_lesche_common::event::{LescheEvent, TagmaStatusPayload};
use kallip_lesche_common::projection::{ProjectionDirty, ProjectionSnapshot};
use kallip_lesche_common::rooms::MemberId;
use kallip_lesche_common::tunnel::{Face, TunnelInbound};
use tokio::sync::{broadcast, oneshot};

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
        let _order = self
            .send_lock
            .lock()
            .expect("app-stream send_lock poisoned");
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
        let _order = self
            .send_lock
            .lock()
            .expect("app-stream send_lock poisoned");
        (self.epoch, self.next_seq.load(Ordering::Relaxed))
    }
}

/// The relay state. The registry is reached only through `control`; the rest is
/// in-memory, per-incarnation.
pub struct ConversationsState {
    /// The registry (identity + tagma metadata + replay guard), DB-backed in
    /// production by `kallip-archeion`, mockable in tests.
    pub control: Arc<dyn ControlPlane>,
    pub registry: RwLock<Registry>,
    /// Outstanding synchronous key exchanges, keyed by conversation. Bounded by
    /// in-flight KEX. Never held together with the registry lock.
    pub pending_key_exchange:
        std::sync::Mutex<HashMap<ConversationId, oneshot::Sender<KeyExchangeResponse>>>,
    /// Acceptable clock skew (both directions) on a tunnel reconnect proof's
    /// timestamp, in seconds.
    pub proof_skew_secs: i64,
    /// How long `key_exchange_init` waits for the tagma's response before 504.
    pub key_exchange_timeout: Duration,
    /// The durable chat store (`Some` in production): room messages + the
    /// membership graph. Required for room delivery and management.
    pub db: Option<crate::db::Db>,
    /// Authoritative agent identity (label + owner display), keyed by the
    /// agent's `ParticipantId`, populated at tunnel-establish and stamped onto
    /// room envelopes so a tagma cannot self-declare its handle. See
    /// [`AgentProfileCache`].
    pub agent_profiles: AgentProfileCache,
}

/// An agent's authoritative display identity, resolved from the registry and
/// stamped onto room envelopes/rows by the relay. Mirrors the display fields of
/// [`kallip_archeion_common::control_plane::TagmaProfile`] minus the raw usability
/// facts + pinned key (which only the tunnel-proof / policy paths need).
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
/// `ParticipantId`. Populated at tunnel-establish (one registry RPC per
/// connect); the rooms send path reads it to stamp the sender handle without a
/// per-message RPC. A cache miss at send falls back to a live `tagma_profile`
/// call and caches the result. Stale on owner rename until tunnel reconnect
/// (acceptable: ownership is non-transferable and the unforgeable id-prefix
/// still disambiguates).
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
    pub fn require_db(&self) -> Result<&crate::db::Db, ApiError> {
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

/// In-memory index of presence, conversations, and per-user app streams.
///
/// The presence / app-stream maps are keyed by
/// [`ParticipantId`] (the opaque room-layer identity): a tagma's tunnel and a
/// user's app stream are both looked up by their derived participant id, so the
/// room fan-out routes every member uniformly regardless of kind. The
/// `TagmaId`/`UserId`-taking helpers derive the participant id internally, so
/// the tagma-lifecycle paths (tunnel/status/signal) and the app-stream paths
/// (`me_events`) keep their existing call shapes; the room fan-out uses the
/// `_by_member` lookups.
pub struct Registry {
    pub conversations: HashMap<ConversationId, ConversationRecord>,
    /// participant_id -> the member's live tunnel (Agent members only today -- a
    /// platform-native tagma). A participant is "online" iff it has an entry.
    /// `owner` routes presence events to the owning user; `id` is a
    /// per-connection identity token so a stale tunnel's cleanup cannot remove a
    /// freshly reconnected tunnel's presence.
    pub presence: HashMap<ParticipantId, PresenceEntry>,
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
    /// Per-tagma projection cache (registry bypass) and
    /// per-(user, tagma) projection SSE channels. Private: mutated only via
    /// the projection methods below. The projection table outlives the
    /// tagma's presence: offline tags keep serving stale reads.
    projections: HashMap<ParticipantId, ProjectionEntry>,
    projection_streams: HashMap<(ParticipantId, ParticipantId), broadcast::Sender<ProjectionDirty>>,
    /// Teardown lag for projection SSE unsubscribes, in milliseconds (30s
    /// in production; tests shorten it). Read by the SSE OnDrop cleanup.
    projection_unsub_lag_ms: std::sync::atomic::AtomicU64,
}

/// One live tunnel: the outbound broadcast, the owning user (for presence
/// routing), the tagma id (carried for the tagma-lifecycle event layer, which
/// still speaks `TagmaId` -- the participant-id key is one-way derived and
/// cannot be reversed for `TagmaOnline`/`TagmaStatus` payloads), and a
/// per-connection identity token used to make presence removal race-free across
/// reconnects.
pub struct PresenceEntry {
    pub tx: broadcast::Sender<TunnelInbound>,
    pub owner: UserId,
    pub tagma_id: TagmaId,
    pub id: Arc<()>,
    /// Latest aggregate status snapshot relayed over this tunnel; written unconditionally on every status POST (with or without live subscribers). Tunnel-scoped: a reconnect starts the cache empty until the next pump tick (bounded self-heal, <= one fallback tick).
    pub latest_status: Option<TagmaStatusPayload>,
    /// The last payload this tunnel's status fan actually broadcast to
    /// the owner's app stream (the meaningful-transition base). Also
    /// tunnel-scoped: a reconnect resets the throttle, and the next
    /// snapshot broadcasts unconditionally.
    pub last_broadcast_status: Option<TagmaStatusPayload>,
}
/// One tagma's stored projection: the latest accepted push plus the
/// bookkeeping the accept/replay logic needs. `generation` records which
/// tunnel connection the snapshot came in on (an `Arc::ptr_eq` against the
/// live [`PresenceEntry::id`]): a push from a different generation is a
/// reconnect whose push counter restarted, so it is accepted
/// unconditionally; a same-generation push must carry a strictly
/// newer `push_seq` or it is an out-of-order replay.
#[derive(Clone)]
pub struct ProjectionEntry {
    /// Store-side seq: increments once per accepted push.
    pub seq: u64,
    /// The owning user, captured at accept time from the tagma's presence:
    /// offline tags keep serving stale reads, and this is what makes
    /// the owner check possible without a live presence entry.
    pub owner: UserId,
    /// The tagma-side push counter at accept time.
    pub last_push_seq: u64,
    /// Wall-clock unix seconds when the push was accepted.
    pub updated_at: i64,
    pub generation: Arc<()>,
    pub snapshot: ProjectionSnapshot,
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
            projections: HashMap::new(),
            projection_streams: HashMap::new(),
            projection_unsub_lag_ms: std::sync::atomic::AtomicU64::new(30_000),
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
        // The 0 -> 1 subscription edge for the status face: the
        // create IS the edge (this is the sole creator), so
        // the open hint fans to every live tagma of the owner here, inside
        // the caller's write lock, exactly like the projection hint fires
        // inside `open_projection_stream`.
        self.fan_owner_sub(user, Face::Status, true);
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
            // last departure, so the close hint fans here — a
            // subscriber arriving inside the lag window keeps the stream
            // and the gate alive, mirroring the projection hint's window.
            self.fan_owner_sub(user, Face::Status, false);
            true
        } else {
            false
        }
    }

    /// Fan an `OwnerSub` frame to every live tagma of `owner`: the status
    /// face is owner-scoped, unlike the (user, tagma)-keyed projection
    /// stream, so one edge touches all of the owner's tagmata. Sync
    /// broadcast sends under the caller's registry write lock (never
    /// awaits); presence-gated like [`Self::send_owner_sub`], so a
    /// test-seeded stream with no live tunnel is a no-op.
    fn fan_owner_sub(&self, owner: &UserId, face: Face, active: bool) {
        for entry in self.presence.values() {
            if entry.owner == *owner {
                let _ = entry.tx.send(TunnelInbound::OwnerSub { face, active });
            }
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

    // --- Projection store & streams ---

    /// Accept a projection push from `tagma` (whose live presence
    /// connection token is `generation`). Returns the new store seq, or
    /// `None` when the push is a same-generation replay (its `push_seq` is
    /// not newer than the accepted one) -- such a push is dropped without
    /// touching the store or bumping the seq.
    pub fn accept_projection(
        &mut self,
        tagma: &TagmaId,
        generation: &Arc<()>,
        push_seq: u64,
        owner: UserId,
        snapshot: ProjectionSnapshot,
    ) -> Option<u64> {
        let pid = ParticipantId::for_tagma(tagma);
        let next_seq = match self.projections.get(&pid) {
            Some(stored) => {
                let same = Arc::ptr_eq(&stored.generation, generation);
                if same && push_seq <= stored.last_push_seq {
                    return None; // out-of-order replay within a generation
                }
                // Store seq keeps climbing across generations: clients use
                // it to detect a reset, so only the tagma push_seq
                // expectation is voided by a new generation.
                stored.seq + 1
            }
            None => 1,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.projections.insert(
            pid,
            ProjectionEntry {
                owner,
                seq: next_seq,
                last_push_seq: push_seq,
                updated_at: now,
                generation: generation.clone(),
                snapshot,
            },
        );
        Some(next_seq)
    }

    /// The stored projection for `tagma`, if any push was ever accepted.
    /// Outlives the tagma's presence: offline tags keep serving
    /// stale reads from this.
    pub fn projection(&self, tagma: &TagmaId) -> Option<&ProjectionEntry> {
        self.projections.get(&ParticipantId::for_tagma(tagma))
    }

    /// Open (or join) the (user, tagma) projection SSE channel. Returns the
    /// receiver plus whether this subscribe is the stream's 0 -> 1 edge (the
    /// caller fans `OwnerSub { face: Projection, active: true }` on that edge only).
    pub fn open_projection_stream(
        &mut self,
        user: &UserId,
        tagma: &TagmaId,
    ) -> (
        broadcast::Sender<ProjectionDirty>,
        broadcast::Receiver<ProjectionDirty>,
        bool,
    ) {
        let key = (
            ParticipantId::for_user(user),
            ParticipantId::for_tagma(tagma),
        );
        let sender = self
            .projection_streams
            .entry(key)
            .or_insert_with(|| broadcast::channel::<ProjectionDirty>(BROADCAST_CAPACITY).0)
            .clone();
        let was_first = sender.receiver_count() == 0;
        let rx = sender.subscribe();
        if was_first {
            self.send_owner_sub(tagma, Face::Projection, true);
        }
        (sender, rx, was_first)
    }

    /// Fan `OwnerSub { face, active }` down one tagma's tunnel. Best-effort:
    /// an offline tagma misses it; the tunnel handler re-sends the current
    /// per-face truth on every reconnect so a gate can never go stale for
    /// long.
    pub fn send_owner_sub(&self, tagma: &TagmaId, face: Face, active: bool) {
        if let Some(entry) = self.presence.get(&ParticipantId::for_tagma(tagma)) {
            let _ = entry.tx.send(TunnelInbound::OwnerSub { face, active });
        }
    }

    /// Remove the (user, tagma) projection channel when `sender` is its last
    /// subscriber, mirroring [`Self::remove_app_stream_if_last`]. Returns
    /// whether the entry was removed (the 1 -> 0 edge; the caller fans
    /// `OwnerSub { face: Projection, active: false }` after the lag window).
    pub fn remove_projection_stream_if_last(
        &mut self,
        user: &UserId,
        tagma: &TagmaId,
        sender: &broadcast::Sender<ProjectionDirty>,
    ) -> bool {
        let key = (
            ParticipantId::for_user(user),
            ParticipantId::for_tagma(tagma),
        );
        // By the time the OnDrop cleanup runs, the dying stream's own rx is
        // already gone, so `receiver_count() == 0` means "nobody left"; a
        // new subscriber that arrived inside the lag window keeps the
        // channel (and the hint) alive.
        if sender.receiver_count() == 0 {
            self.projection_streams.remove(&key);
            self.send_owner_sub(tagma, Face::Projection, false);
            true
        } else {
            false
        }
    }
    /// The projection SSE teardown lag (ms). Production is 30s; tests
    /// shorten it so the hint-flap window is observable.
    pub fn projection_unsub_lag_ms(&self) -> u64 {
        self.projection_unsub_lag_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(test)] // consumed by the SSE OnDrop teardown test only
    /// Shorten the teardown lag (test injection only).
    pub fn set_projection_unsub_lag_ms(&self, ms: u64) {
        self.projection_unsub_lag_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether any client currently holds a live projection stream for
    /// `tagma`. Consulted on tunnel reconnect so the lesche re-sends the
    /// current hint (the tagma may have missed a flip while offline).
    pub fn projection_stream_live_for_tagma(&self, tagma: &TagmaId) -> bool {
        let tp = ParticipantId::for_tagma(tagma);
        self.projection_streams
            .iter()
            .any(|((_, t), s)| *t == tp && s.receiver_count() > 0)
    }

    /// Fan a `ProjectionDirty` frame to `(user, tagma)`'s projection stream,
    /// if any client is subscribed. Best-effort: no subscribers is fine (the
    /// hint suppression means this is the normal unread case).
    pub fn fan_projection_dirty(&self, user: &UserId, tagma: &TagmaId, dirty: ProjectionDirty) {
        if let Some(sender) = self.projection_streams.get(&(
            ParticipantId::for_user(user),
            ParticipantId::for_tagma(tagma),
        )) {
            let _ = sender.send(dirty);
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

    /// Register a live tagma tunnel for `tagma`, owned by `owner`, capturing the
    /// per-connection identity token `id` so a stale tunnel's cleanup cannot
    /// remove a fresh reconnect's presence. Keyed by the tagma's derived
    /// participant id.
    pub fn register_presence(
        &mut self,
        tagma: &TagmaId,
        owner: UserId,
        tx: broadcast::Sender<TunnelInbound>,
        id: Arc<()>,
    ) {
        self.presence.insert(
            ParticipantId::for_tagma(tagma),
            PresenceEntry {
                tx,
                owner,
                tagma_id: tagma.clone(),
                id,
                latest_status: None,
                last_broadcast_status: None,
            },
        );
    }

    /// The live tunnel entry for a room member id (the room fan-out's Agent-member
    /// lookup). See [`app_stream_by_member`](Self::app_stream_by_member) for the
    /// keying rationale and the `MemberId`-typed guard.
    pub fn presence_by_member(&self, mid: &MemberId) -> Option<&PresenceEntry> {
        self.presence.get(mid.as_ref())
    }

    /// The live tunnel entry for a tagma (the tagma-lifecycle paths: tunnel /
    /// status / signal). Derives the participant id internally so those paths
    /// keep their `TagmaId` call shape.
    pub fn presence_by_tagma(&self, tagma: &TagmaId) -> Option<&PresenceEntry> {
        self.presence.get(&ParticipantId::for_tagma(tagma))
    }

    /// Mutable lookup for the status relay's cache write (the only writer of latest_status). See presence_by_tagma.
    pub fn presence_by_tagma_mut(&mut self, tagma: &TagmaId) -> Option<&mut PresenceEntry> {
        self.presence.get_mut(&ParticipantId::for_tagma(tagma))
    }
    /// Remove `tagma`'s presence iff the live entry is still `id` (Arc pointer
    /// identity), returning whether it was removed. Race-free across reconnects.
    pub fn take_presence_if_owned(&mut self, tagma: &TagmaId, id: &Arc<()>) -> bool {
        let pid = ParticipantId::for_tagma(tagma);
        let still_ours = self
            .presence
            .get(&pid)
            .map(|p| Arc::ptr_eq(&p.id, id))
            .unwrap_or(false);
        if still_ours {
            self.presence.remove(&pid);
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
    use kallip_archeion_common::ids::TagmaId;
    use kallip_lesche_common::event::LescheEvent;

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
        let (tx_a1, _) = broadcast::channel::<TunnelInbound>(8);
        let (tx_a2, _) = broadcast::channel::<TunnelInbound>(8);
        let (tx_b1, _) = broadcast::channel::<TunnelInbound>(8);
        reg.register_presence(&a1, alice.clone(), tx_a1, Arc::new(()));
        reg.register_presence(&a2, alice.clone(), tx_a2, Arc::new(()));
        reg.register_presence(&b1, bob.clone(), tx_b1, Arc::new(()));

        // Snapshot for alice = her two tagmas.
        let alice_online: Vec<TagmaId> = reg
            .presence
            .values()
            .filter(|e| e.owner == alice)
            .map(|e| e.tagma_id.clone())
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
        let stream = reg.open_app_stream(&alice);
        let mut rx = stream.subscribe();
        // Simulate the tunnel handler's online announcement.
        stream
            .deliver(LescheEvent::TagmaOnline {
                tagma_id: TagmaId::from("a1".to_string()),
            })
            .expect("delivered");
        let frame = rx.recv().await.expect("receive");
        assert!(matches!(frame.event, LescheEvent::TagmaOnline { .. }));
    }

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
