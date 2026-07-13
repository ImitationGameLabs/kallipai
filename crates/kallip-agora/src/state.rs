//! Soft-state `AppState` + in-memory `Registry`. No persistence, no DB.
//!
//! Everything here is rebuilt on restart: presence from heralds reconnecting,
//! ownership/pinned-key from re-enrollment, tokens minted fresh.
//!
//! Known residual: the in-memory maps (`conversations`, `seq_seen`,
//! `app_streams`) have no eviction cap, so a long-lived incarnation that
//! accumulates many conversations grows without bound. This is bounded in
//! practice by restart cadence; an LRU/cap is phase-2 work.
//!
//! `Registry` lives behind a `std::sync::RwLock` (not tokio): every operation
//! under it is non-async (HashMap lookups, `broadcast::send`, `receiver_count`,
//! token-hash compare), and the synchronous guard is what lets the `OnDrop`
//! cleanup in `routes/herald.rs` + `routes/events.rs` run inline in `Drop::drop`,
//! provably race-free with no spawn-timing window. The replay/dedup window
//! `seq_seen` is a separate `std::sync::Mutex`, never held at the same time as
//! the registry lock.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control::KeyExchangeResponse;
use kallip_agora_common::event::AgoraEvent;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TeamId, UserId};
use kallip_common::agentid::AgentId;
use kallip_common::authtoken::TokenHash;
use kallip_common::protocol::ApiError;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub type SharedState = Arc<AppState>;

/// Capacity of the per-team / per-user broadcast channel feeding an SSE stream.
/// Shared by the herald tunnel and the app event stream.
pub const BROADCAST_CAPACITY: usize = 128;

/// In-memory control-plane state, rebuilt on every restart.
///
/// # Lock-discipline invariants (authoritative)
///
/// These are enforced by convention today; a future refactor should encapsulate
/// the mutation points as methods so the type system upholds them. Until then,
/// every handler MUST hold to:
///
/// 1. **No `.await` under a lock.** Drop every `read()`/`write()` /
///    `seq_seen`/`pending_key_exchange` guard before awaiting. The synchronous
///    guards are fine precisely because nothing under them is async.
/// 2. **Never co-hold the registry lock with `seq_seen` or
///    `pending_key_exchange`.** Reserve a sequence / register a KEX waiter only
///    after the registry guard is dropped. This is what keeps `post_envelope`'s
///    reserve-then-route and `key_exchange_init`'s register-then-await
///    deadlock-free.
/// 3. **`app_streams` has a single creator: `me_events`** (`routes/events.rs`).
///    Key exchange is synchronous and never reads this map. Inserting elsewhere
///    would violate the `OnDrop` cleanup assumption (every entry has a live
///    subscriber).
/// 4. **`pending_key_exchange` cleanup is unconditional** via `KexGuard`
///    (`routes/conversations.rs`): resolve, timeout, or request cancellation
///    all remove the entry, so the table is bounded by in-flight KEX.
pub struct AppState {
    pub shutdown: CancellationToken,
    pub limits: Limits,
    /// SHA-256 of the admin token; the single provisioning authority.
    pub admin_token_hash: TokenHash,
    pub registry: RwLock<Registry>,
    /// Per-conversation, per-sender highest `sequence_n` seen.
    ///
    /// Per-incarnation only: lost on agora restart, so a retried envelope after
    /// a restart is not deduped here (accepted residual, plan I5; the herald
    /// also runs its own receiver-side window).
    pub seq_seen: std::sync::Mutex<HashMap<ConversationId, HashMap<String, u64>>>,
    /// Outstanding synchronous key exchanges, keyed by conversation. The init
    /// handler inserts a oneshot sender BEFORE pushing the init to the herald,
    /// then awaits its receiver; the response handler resolves it. Cleanup is
    /// unconditional (see `KexGuard` in `routes/conversations.rs`): resolve,
    /// timeout, or app-request cancellation all remove the entry, so this is
    /// bounded by the number of in-flight KEX (<= one per conversation, <=
    /// per-user conversation cap). Never held together with the registry lock.
    pub pending_key_exchange:
        std::sync::Mutex<HashMap<ConversationId, oneshot::Sender<KeyExchangeResponse>>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_body_size_bytes: usize,
    /// How long a minted enrollment code remains redeemable.
    pub enrollment_code_ttl: Duration,
    /// Acceptable clock skew (both directions) on a tunnel reconnect proof's
    /// timestamp, in seconds.
    pub proof_skew_secs: i64,
    /// Per-user cap on live conversations, bounding the growth of `conversations`
    /// (and `seq_seen`) against an authenticated user driving unbounded creates.
    pub max_conversations_per_user: usize,
    /// How long `key_exchange_init` waits for the herald's response before
    /// failing with 504. Key exchange is cryptographically instant; this only
    /// bounds waiting on a herald that is offline, saturated, or dropped
    /// mid-turn.
    pub key_exchange_timeout: Duration,
}

/// In-memory index of users, teams, conversations, presence, and per-user app
/// streams. All token lookups are hash-based; variable lookup time over hashes
/// leaks nothing about a secret (an attacker cannot steer a SHA-256 output).
pub struct Registry {
    pub users: HashSet<UserId>,
    /// Access-token hash -> the user it authenticates.
    pub access_tokens: HashMap<TokenHash, UserId>,
    pub teams: HashMap<TeamId, TeamRecord>,
    /// Team-token hash -> the team it authenticates.
    pub team_tokens: HashMap<TokenHash, TeamId>,
    /// Enrollment-code hash -> its binding + lifecycle.
    pub enrollment_codes: HashMap<TokenHash, EnrollmentCode>,
    pub conversations: HashMap<ConversationId, ConversationRecord>,
    /// team_id -> the team's live herald tunnel. A team is "online" iff it has
    /// an entry. `id` is a per-connection identity token so a stale tunnel's
    /// cleanup cannot remove a freshly-reconnected tunnel's presence.
    pub presence: HashMap<TeamId, PresenceEntry>,
    /// team_id -> the highest accepted tunnel-proof unix_secs. Makes a captured
    /// proof single-use within the skew window: a replay (same or older
    /// timestamp) is rejected. Bounded to one entry per team by overwrite; the
    /// tunnel route GCs stale entries on each connect. Private: mutate only via
    /// [`Registry::consume_tunnel_proof`].
    seen_tunnel_proofs: HashMap<TeamId, i64>,
    /// user_id -> outbound broadcast to their multiplexed app SSE. The sole
    /// creator is `me_events` (`routes/events.rs`); it carries agent envelopes
    /// and lifecycle events only. Key exchange is synchronous and never touches
    /// this map. Private: mutate only via [`Registry::open_app_stream`] /
    /// [`Registry::remove_app_stream_if_last`].
    app_streams: HashMap<UserId, broadcast::Sender<AgoraEvent>>,
    /// user_id -> count of their live conversations, mirroring `conversations`
    /// ownership for an O(1) cap check at create time. Invariant: this count
    /// MUST stay equal to the number of live `conversations` entries owned by
    /// the user, so a future delete-conversation route MUST decrement it in
    /// lockstep. Conversations are not deletable today, so the count is
    /// increment-only; per-incarnation (soft-state, cleared on restart).
    /// Private: mutate only via [`Registry::create_conversation`].
    conversation_counts: HashMap<UserId, usize>,
}

/// One live herald tunnel: the outbound broadcast and a per-connection identity
/// token used to make presence removal race-free across reconnects.
pub struct PresenceEntry {
    pub tx: broadcast::Sender<HeraldInbound>,
    pub id: Arc<()>,
}

/// A registered agent team: owner + the herald's pinned device public key.
pub struct TeamRecord {
    pub owner: UserId,
    pub pinned_public_key: Ed25519PublicKey,
}

#[derive(Debug)]
pub struct EnrollmentCode {
    pub user: UserId,
    pub expires_at: Instant,
    pub consumed: bool,
}

#[derive(Debug, Clone)]
pub struct ConversationRecord {
    pub owner: UserId,
    pub team_id: TeamId,
    pub agent_id: AgentId,
}

impl AppState {
    pub fn new(admin_token_hash: TokenHash, limits: Limits) -> Self {
        Self {
            shutdown: CancellationToken::new(),
            limits,
            admin_token_hash,
            registry: RwLock::new(Registry::new()),
            seq_seen: std::sync::Mutex::new(HashMap::new()),
            pending_key_exchange: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Read-lock the registry, mapping poisoning (a prior panic under the lock)
    /// into an HTTP 500.
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

impl Registry {
    pub fn new() -> Self {
        Self {
            users: HashSet::new(),
            access_tokens: HashMap::new(),
            teams: HashMap::new(),
            team_tokens: HashMap::new(),
            enrollment_codes: HashMap::new(),
            conversations: HashMap::new(),
            presence: HashMap::new(),
            seen_tunnel_proofs: HashMap::new(),
            app_streams: HashMap::new(),
            conversation_counts: HashMap::new(),
        }
    }

    /// Drop consumed/expired enrollment codes. Call opportunistically at create
    /// and redeem so `enrollment_codes` growth is bounded by the TTL window x
    /// creation rate rather than accumulating forever.
    pub fn gc_enrollment_codes(&mut self, now: Instant) {
        self.enrollment_codes.retain(|_, c| !c.is_dead(now));
    }

    /// The live app event-stream sender for `user`, if any. Read-only access for
    /// routing agent envelopes; creation is [`Self::open_app_stream`].
    pub fn app_stream(&self, user: &UserId) -> Option<&broadcast::Sender<AgoraEvent>> {
        self.app_streams.get(user)
    }

    /// Ensure an app event-stream channel exists for `user` and return a sender
    /// clone. Sole creator of `app_streams` entries: only `me_events` may call
    /// this, upholding the single-creator invariant the `OnDrop` cleanup relies
    /// on (every entry has a live subscriber).
    pub fn open_app_stream(&mut self, user: &UserId) -> broadcast::Sender<AgoraEvent> {
        self.app_streams
            .entry(user.clone())
            .or_insert_with(|| broadcast::channel::<AgoraEvent>(BROADCAST_CAPACITY).0)
            .clone()
    }

    /// Remove `user`'s app-stream channel iff `sender` is the last subscriber
    /// (`receiver_count() == 1`: the dying SSE stream itself, still alive during
    /// the `OnDrop` closure). Mirrors the `me_events` cleanup.
    pub fn remove_app_stream_if_last(
        &mut self,
        user: &UserId,
        sender: &broadcast::Sender<AgoraEvent>,
    ) {
        if sender.receiver_count() == 1 {
            self.app_streams.remove(user);
        }
    }

    /// Create a conversation owned by `owner` bound to `(team_id, agent_id)`,
    /// enforcing the per-user cap and keeping `conversation_counts` in lockstep
    /// with `conversations`. Sole mutator of both. Returns the new id.
    pub fn create_conversation(
        &mut self,
        owner: &UserId,
        team_id: TeamId,
        agent_id: AgentId,
        cap: usize,
    ) -> Result<ConversationId, ApiError> {
        let count = self.conversation_counts.get(owner).copied().unwrap_or(0);
        if count >= cap {
            return Err(ApiError::conflict("per-user conversation limit reached"));
        }
        let conv_id = ConversationId::random();
        self.conversations.insert(
            conv_id.clone(),
            ConversationRecord {
                owner: owner.clone(),
                team_id,
                agent_id,
            },
        );
        *self.conversation_counts.entry(owner.clone()).or_insert(0) = count + 1;
        Ok(conv_id)
    }

    /// Atomic tunnel-proof consume: reject a replay (same or older timestamp),
    /// GC entries outside the skew window, then record `ts`. Sole mutator of
    /// `seen_tunnel_proofs`. `now`/`skew` are unix seconds.
    pub fn consume_tunnel_proof(
        &mut self,
        team: &TeamId,
        ts: i64,
        skew: i64,
        now: i64,
    ) -> Result<(), ApiError> {
        if self.seen_tunnel_proofs.get(team).copied() >= Some(ts) {
            return Err(ApiError::unauthorized("replayed or stale device proof"));
        }
        let floor = now - skew;
        self.seen_tunnel_proofs.retain(|_, prev| *prev > floor);
        self.seen_tunnel_proofs.insert(team.clone(), ts);
        Ok(())
    }

    /// Register a live herald tunnel for `team`, capturing the per-connection
    /// identity token `id` so a stale tunnel's cleanup cannot remove a fresh
    /// reconnect's presence.
    pub fn register_presence(
        &mut self,
        team: &TeamId,
        tx: broadcast::Sender<HeraldInbound>,
        id: Arc<()>,
    ) {
        self.presence.insert(team.clone(), PresenceEntry { tx, id });
    }

    /// Remove `team`'s presence iff the live entry is still `id` (Arc pointer
    /// identity), returning whether it was removed. Race-free across reconnects.
    pub fn take_presence_if_owned(&mut self, team: &TeamId, id: &Arc<()>) -> bool {
        let still_ours = self
            .presence
            .get(team)
            .map(|p| Arc::ptr_eq(&p.id, id))
            .unwrap_or(false);
        if still_ours {
            self.presence.remove(team);
        }
        still_ours
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl EnrollmentCode {
    /// Has this code expired or already been consumed?
    pub fn is_dead(&self, now: Instant) -> bool {
        self.consumed || now >= self.expires_at
    }
}
