//! `AppState`: the durable handle + the in-memory soft-state `Registry`.
//!
//! **Durable** (Postgres, via [`crate::db`]): users, passkeys, invite codes, enrollment
//! tokens, tagmata, tagma tokens, sessions — identity / credentials / provisioning.
//! **Soft-state** (the `Registry` here): presence, conversations, app streams,
//! tunnel-proof replay guard, per-user conversation counts. Everything in the
//! `Registry` is rebuilt on restart: presence from heralds reconnecting,
//! conversations are create-on-demand, conversations' history lives on the host.
//!
//! Known residual: the in-memory maps (`conversations`, `seq_seen`,
//! `app_streams`) have no eviction cap, so a long-lived incarnation that
//! accumulates many conversations grows without bound. This is bounded in
//! practice by restart cadence; an LRU/cap is owed before any scale deploy.
//!
//! `Registry` lives behind a `std::sync::RwLock` (not tokio): every operation
//! under it is non-async (HashMap lookups, `broadcast::send`, `receiver_count`),
//! and the synchronous guard is what lets the `OnDrop` cleanup in
//! `routes/herald.rs` + `routes/events.rs` run inline in `Drop::drop`,
//! provably race-free with no spawn-timing window. The replay/dedup window
//! `seq_seen` is a separate `std::sync::Mutex`, never held at the same time as
//! the registry lock.
//!
//! DB access never happens under the registry lock: handlers read the `Db`
//! (Clone, async) before or after taking a registry guard, never both at once.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::db::Db;
use crate::ratelimit::IpRateLimiter;
use crate::session::SessionCfg;
use kallip_agora_common::control::KeyExchangeResponse;
use kallip_agora_common::event::AgoraEvent;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TagmaId, UserId};
use kallip_common::agentid::AgentId;
use kallip_common::authtoken::TokenHash;
use kallip_common::protocol::ApiError;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use webauthn_rs::Webauthn;

pub type SharedState = Arc<AppState>;

/// Capacity of the per-tagma / per-user broadcast channel feeding an SSE stream.
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
///    guards are fine precisely because nothing under them is async. DB
///    (`.await`-bearing) lookups happen outside the registry lock entirely.
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
    /// Durable store handle (sea-orm `DatabaseConnection`, cheap to clone).
    pub db: Db,
    /// Configured WebAuthn relying party (register/login ceremonies).
    pub webauthn: Arc<Webauthn>,
    /// Session-cookie attrs + TTL.
    pub session_cfg: SessionCfg,
    /// Per-IP token bucket guarding `/v1/auth/*`.
    pub auth_rate_limiter: IpRateLimiter,
    /// CIDRs whose direct connections are trusted to have set
    /// `X-Forwarded-For`. The rate limiter honors XFF only for a peer in one of
    /// these nets (see [`crate::clientip::real_client_ip`]). Empty means XFF is
    /// never trusted.
    pub trusted_proxies: Vec<ipnet::IpNet>,
    pub registry: RwLock<Registry>,
    /// Per-conversation, per-sender highest `sequence_n` seen.
    ///
    /// Per-incarnation only: lost on agora restart, so a retried envelope after
    /// a restart is not deduped here (accepted residual; the herald
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
    /// How long a minted enrollment token remains redeemable.
    pub enrollment_code_ttl: Duration,
    /// Default lifetime for an admin-minted invite code when none is given.
    pub invite_default_ttl_secs: u64,
    /// Acceptable clock skew (both directions) on a tunnel reconnect proof's
    /// timestamp, in seconds.
    pub proof_skew_secs: i64,
    /// Per-user cap on live conversations, bounding the growth of
    /// `conversations` (and `seq_seen`) against an authenticated user driving
    /// unbounded creates.
    pub max_conversations_per_user: usize,
    /// How long `key_exchange_init` waits for the herald's response before
    /// failing with 504. Key exchange is cryptographically instant; this only
    /// bounds waiting on a herald that is offline, saturated, or dropped
    /// mid-turn.
    pub key_exchange_timeout: Duration,
}

/// In-memory index of conversations, presence, and per-user app streams. The
/// identity / credential / provisioning layer (users, passkeys, invite codes,
/// enrollment tokens, tagmata, tagma tokens, sessions) lives in the durable
/// store ([`crate::db`]); only the data-plane soft state that is rebuilt on
/// every restart lives here.
pub struct Registry {
    pub conversations: HashMap<ConversationId, ConversationRecord>,
    /// tagma_id -> the tagma's live herald tunnel. A tagma is "online" iff it
    /// has an entry. `id` is a per-connection identity token so a stale tunnel's
    /// cleanup cannot remove a freshly-reconnected tunnel's presence.
    pub presence: HashMap<TagmaId, PresenceEntry>,
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
/// token used to make presence removal race-free across reconnects. A planned
/// agent roster will hang off this entry.
pub struct PresenceEntry {
    pub tx: broadcast::Sender<HeraldInbound>,
    pub id: Arc<()>,
}

#[derive(Debug, Clone)]
pub struct ConversationRecord {
    pub owner: UserId,
    pub tagma_id: TagmaId,
    pub agent_id: AgentId,
}

impl AppState {
    pub fn new(
        admin_token_hash: TokenHash,
        limits: Limits,
        db: Db,
        webauthn: Arc<Webauthn>,
        session_cfg: SessionCfg,
        auth_rate_limiter: IpRateLimiter,
        trusted_proxies: Vec<ipnet::IpNet>,
    ) -> Self {
        Self {
            shutdown: CancellationToken::new(),
            limits,
            admin_token_hash,
            db,
            webauthn,
            session_cfg,
            auth_rate_limiter,
            trusted_proxies,
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
            conversations: HashMap::new(),
            presence: HashMap::new(),
            app_streams: HashMap::new(),
            conversation_counts: HashMap::new(),
        }
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

    /// Create a conversation owned by `owner` bound to `(tagma_id, agent_id)`,
    /// enforcing the per-user cap and keeping `conversation_counts` in lockstep
    /// with `conversations`. Sole mutator of both. Returns the new id. Tagma
    /// ownership is validated by the caller against the DB before this call.
    pub fn create_conversation(
        &mut self,
        owner: &UserId,
        tagma_id: TagmaId,
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
                tagma_id,
                agent_id,
            },
        );
        *self.conversation_counts.entry(owner.clone()).or_insert(0) = count + 1;
        Ok(conv_id)
    }

    /// Register a live herald tunnel for `tagma`, capturing the per-connection
    /// identity token `id` so a stale tunnel's cleanup cannot remove a fresh
    /// reconnect's presence.
    pub fn register_presence(
        &mut self,
        tagma: &TagmaId,
        tx: broadcast::Sender<HeraldInbound>,
        id: Arc<()>,
    ) {
        self.presence
            .insert(tagma.clone(), PresenceEntry { tx, id });
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
