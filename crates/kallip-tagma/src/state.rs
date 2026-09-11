//! Tagma-wide shared state: one `Arc<AppState>` built by `main::run` and
//! cloned into every task. Four fields are `OnceLock` services installed by
//! `main::run` at startup: `hook_rules` right after construction, then the
//! `work_schedules` and `inboxes` stores, then `external` (the relay
//! projector) at root-relay activation; readers see `None` until set.
//! Mutex-kind conventions are stated once in the crate root (`main.rs`).
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
pub use kallip_common::agentid::AgentId;
use kallip_common::authtoken::TokenHash;
use kallip_common::policy::{ExecPolicy, PolicyPreset};
pub use kallip_common::protocol::AgentState;
pub use kallip_common::protocol::AgentSummary;
use kallip_common::protocol::ApiError;
use kallip_common::protocol::SseEvent;
use kallip_common::protocol::{LockState, ParkedReason, TransientRetryInfo};
use kallip_runtime::agent_task::RoundToken;
use kallip_runtime::approval::ApprovalStore;
use kallip_runtime::config::{AgentConfig, PermissionClass};
use kallip_runtime::context::ContextStore;
use kallip_runtime::profile::{ProfileConfig, ProfileRegistry};
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

/// Write the state byte and its transition timestamp together. The two
/// Relaxed stores are unordered against a concurrent summary read, so a
/// summary can momentarily pair the new state with the previous timestamp;
/// `state_since` feeds display and sorting only, so the transient mislabel
/// is harmless.
pub fn transition_state(state: &AtomicU8, state_since: &AtomicU64, new_state: u8) {
    state.store(new_state, Ordering::Relaxed);
    state_since.store(kallip_common::timefmt::now_epoch(), Ordering::Relaxed);
}
pub type SharedState = Arc<AppState>;

/// Atomic-swap container for the full profile state: the serializable config
/// (for GET /profiles) and the assembled registry (for agent spawn + apply).
/// Swapped as a unit on PUT /profiles via [`ArcSwap`]; readers load a
/// consistent snapshot. Each running agent pins its own `Arc<ProfileRegistry>`
/// snapshot in its [`FailoverState`](kallip_runtime::FailoverState) — a swap does not disturb running agents
/// until an explicit apply.
pub struct ProfileBundle {
    /// The config as loaded (GET) or written (PUT) — serializable, no backends.
    pub config: ProfileConfig,
    /// The registry built from `config` — carries pre-built backends.
    pub registry: Arc<ProfileRegistry>,
}

/// Tagma-side cache of the rooms this tagma belongs to, keyed by relay name:
/// each online relay's room-membership poll owns its slice, so two concurrently
/// connected archeions never overwrite each other. Rooms are plaintext
/// server-readable (the lesche enforces member access), so the cache is pure
/// routing state: it tells the relay inbound fork and the agent's room
/// send/read/list routes whether a given conversation id is a room envelope
/// (vs the bilateral 1:1 conversation), and which lesche owns a given room
/// (see [`JoinedRooms::owner_of`]). Populated from the `list_my_rooms` poll
/// (the room-membership pump in `relay::room_poll`, whose immediate first tick
/// warms it on tunnel-up) and refreshed on each `Wake` nudge.
///
/// Best-effort: a cold or stale miss for a room routes the envelope to the
/// bilateral path, where it is dropped (the lesche member-gated read still
/// works) -- self-correcting on the next poll that warms the entry.
#[derive(Default)]
pub struct JoinedRooms {
    rooms: Mutex<HashMap<String, HashSet<kallip_lesche_common::rooms::RoomId>>>,
}

impl JoinedRooms {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `room` is a room this tagma belongs to (union across relays).
    pub async fn is_joined(&self, room: &kallip_lesche_common::rooms::RoomId) -> bool {
        self.rooms
            .lock()
            .await
            .values()
            .any(|set| set.contains(room))
    }

    /// Replace the named relay's room set from a fresh `list_my_rooms`
    /// snapshot. Each relay's poll owns its slice; other relays' slices are
    /// untouched.
    pub async fn set_joined_rooms(
        &self,
        relay: &str,
        rooms: impl IntoIterator<Item = kallip_lesche_common::rooms::RoomId>,
    ) {
        let mut g = self.rooms.lock().await;
        g.insert(relay.to_owned(), rooms.into_iter().collect());
    }

    /// Snapshot of the room ids across all relays (for the agent's
    /// room-list route).
    pub async fn joined_rooms(&self) -> HashSet<kallip_lesche_common::rooms::RoomId> {
        let g = self.rooms.lock().await;
        g.values().flat_map(|s| s.iter().cloned()).collect()
    }

    /// The relay whose lesche owns `room`, if any poll has warmed the entry.
    /// Room ids are random and unique per lesche, so at most one relay holds
    /// a given id; a cold or stale miss is the caller's problem (the lesche
    /// routes return 503, mirroring the relay-not-online family).
    pub async fn owner_of(&self, room: &kallip_lesche_common::rooms::RoomId) -> Option<String> {
        self.rooms
            .lock()
            .await
            .iter()
            .find_map(|(relay, set)| set.contains(room).then(|| relay.clone()))
    }
}
/// Direct-session routing cache: which of this tagma's direct sessions exist
/// on which relay's lesche. Populated by the same poll pump as [`JoinedRooms`]
/// (one tick, two calls: `list_my_rooms` then `list_direct_sessions`); keyed
/// by relay entry name like the room cache, refreshed by full replace per
/// tick. Best-effort in the same way: a cold or stale miss routes the send/
/// read to the first installed relay or surfaces unavailable, and the next
/// poll self-corrects -- the lesche member-gates every direct route, so a
/// stale entry never leaks a session.
#[derive(Default)]
pub struct DirectSessions {
    sessions: Mutex<HashMap<String, HashSet<kallip_lesche_common::direct::DirectSessionId>>>,
}

impl DirectSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `session` is a direct session this tagma belongs to (union
    /// across relays). The relay inbound fork dispatches on this before the
    /// bilateral path, mirroring the joined-rooms check.
    pub async fn is_session(
        &self,
        session: &kallip_lesche_common::direct::DirectSessionId,
    ) -> bool {
        self.sessions
            .lock()
            .await
            .values()
            .any(|set| set.contains(session))
    }

    /// Replace the named relay's direct-session set from a fresh
    /// `list_direct_sessions` snapshot. Each relay's poll owns its slice;
    /// other relays' slices are untouched.
    pub async fn set_direct_sessions(
        &self,
        relay: &str,
        sessions: impl IntoIterator<Item = kallip_lesche_common::direct::DirectSessionId>,
    ) {
        let mut g = self.sessions.lock().await;
        g.insert(relay.to_owned(), sessions.into_iter().collect());
    }

    /// The relay whose lesche holds `session`, if any poll has warmed the
    /// entry. Session ids are derived (not lesche-assigned), so the SAME id
    /// can exist on several lesches; the first warmed owner wins (a
    /// single-relay deployment -- today's only kind -- has exactly one).
    pub async fn owner_of(
        &self,
        session: &kallip_lesche_common::direct::DirectSessionId,
    ) -> Option<String> {
        self.sessions
            .lock()
            .await
            .iter()
            .find_map(|(relay, set)| set.contains(session).then(|| relay.clone()))
    }
}

pub struct AppState {
    /// Agent registry. **Lock order:** this RwLock must be acquired before
    /// any per-agent `exec_policy` std::sync::RwLock inside agent entries.
    pub registry: RwLock<AgentRegistry>,
    /// Tagma-global `bash_exec` classify preset, read once at startup from
    /// `KALLIP_POLICY_PRESET` and immutable for the tagma's lifetime. Every agent
    /// inherits this same preset (it is not per-agent).
    pub preset: PolicyPreset,
    /// Exec-hook rules (builtin preset + `exec_hooks.toml` overrides,
    /// tagma-wide), loaded once at startup — same trust boundary and
    /// lifetime as [`AppState::preset`].
    /// Installed after construction (the `work_schedules` pattern): unset
    /// means no rules, and every spawned agent clones the same set.
    pub hook_rules: std::sync::OnceLock<Arc<Vec<kallip_runtime::policy::HookRule>>>,
    pub shutdown: CancellationToken,
    /// SHA-256 of the operator token. The plaintext is printed once at startup and
    /// never retained; this hash is what incoming bearer tokens are compared against.
    pub operator_token_hash: TokenHash,
    /// Maximum number of concurrent agents.
    pub max_agents: usize,
    /// Maximum number of direct subagents per agent.
    pub max_subagents: usize,
    /// Message channel capacity per agent.
    pub prompt_queue_size: usize,
    /// Tagma-wide token budget shared by all agents.
    pub token_budget: kallip_runtime::token_budget::TokenBudget,
    /// Profile registry loaded once at startup (config file or implicit env profile).
    /// Shared so the pre-built backends survive across agents.
    pub profiles: Arc<ArcSwap<ProfileBundle>>,
    /// Shared HTTP client for tagma-side outbound calls (the files-service
    /// media fetch): one client reuses its connection pool across requests,
    /// and clones are cheap (an internal `Arc`).
    pub files_http: reqwest::Client,
    /// Tagma-wide directory write-lock coordinator. Shared across all agents so
    /// one agent holding a dir's write-lock blocks another. The tagma build
    /// enforces locks via landlock on Linux (mandatory); advisory elsewhere.
    pub lock_manager: Arc<kallip_runtime::dirlock::DirLockManager>,
    /// Online-mode relay connectors, keyed by entry name (one per configured
    /// archeion), each with its long-running tunnel task's `JoinHandle` so
    /// graceful shutdown can drain it. Empty in pure-local deployments and
    /// holds only the successfully-activated subset otherwise (a failed entry
    /// degrades to local-only for that entry alone).
    ///
    /// Interior-mutable (not set via `Arc::get_mut`) because the root agent's
    /// bridge/agent tasks already hold `Arc<AppState>` clones by the time the
    /// relays are installed. Read under the mutex (e.g. by the lesche message
    /// routes); guards never span an await.
    pub relays:
        std::sync::Mutex<HashMap<String, (crate::relay::RelayHandle, tokio::task::JoinHandle<()>)>>,
    /// The in-process typed topic bus (see [`crate::bus`]): the fixed registry
    /// of chat topics every pump and serving path publishes/subscribes
    /// through. Built once here at construction — the single registration
    /// site.
    pub bus: crate::bus::EventBus,
    /// Invalidation source for the snapshot pumps (conduwuit-style watch): the
    /// discrete registry mutation classes — roster changes ([`AgentRegistry`]),
    /// duty flips ([`crate::duty::DutyStore`]), budget-limit writes, work-schedule edits —
    /// bump the generation and wake every snapshot pump instantly; the pumps'
    /// fallback tickers stay as the staleness lower bound.
    /// Continuous consumption (`TokenBudget::record_usage`) is deliberately
    /// NOT notified: turn-lifecycle signals already cover it, and the ticker
    /// bounds the residual drift.
    pub invalidations: tokio::sync::watch::Sender<u64>,
    /// The single external projector: the SOLE writer of chat content. Owns the
    /// unified `chat_history` store + conversation id, subscribes to the root
    /// broadcast, persists each authored/inbound row once, and publishes the
    /// stamped frame onto a bus both serving paths (direct SSE + relay envelope)
    /// forward. Installed once at startup after the `Arc<AppState>` exists.
    pub external: std::sync::OnceLock<crate::external::ExternalProjector>,
    /// Room-routing cache: which of this tagma's rooms exist (all plaintext).
    /// Populated by the room-membership pump; independent of the relay, so it
    /// stays usable even when the relay is not online. See [`JoinedRooms`].
    pub joined_rooms: Arc<JoinedRooms>,
    /// Direct-session routing cache (the direct counterpart of
    /// [`Self::joined_rooms`]). See [`DirectSessions`].
    pub direct_sessions: Arc<DirectSessions>,
    /// Per-agent message inboxes (SQLite-backed). Installed at startup.
    /// The off-duty gate buffers messages here; the phase
    /// executor flushes them on wake-up.
    pub inboxes: std::sync::OnceLock<crate::inbox::InboxStore>,
    /// Per-agent duty status. The off-duty gate checks this before
    /// delivering external messages; off-duty agents buffer to inbox.
    pub duty: Arc<crate::duty::DutyStore>,
    /// SQLite-backed work-schedule store. Opened at startup.
    pub work_schedules: std::sync::OnceLock<crate::work_schedule::WorkScheduleStore>,
    /// SQLite-backed task coordination store. Opened at startup —
    /// `TaskStore::open` runs the migration chain, so the boot brings
    /// the schema to head. The SOLE writer of tasks.sqlite: CLI
    /// processes never touch the file, they go through the task API.
    pub tasks: std::sync::OnceLock<std::sync::Arc<kallip_task::TaskStore>>,
    /// Content-addressed blob root for closed-task dossiers, handed to
    /// close/extract alongside the store. Installed at startup.
    pub task_blobs: std::sync::OnceLock<std::sync::Arc<dyn kallip_task::BlobStore>>,
    /// The single agent spawn entry: agent create, boot restore, and
    /// delivery's reactivation all route through here. An indirection so
    /// tests can observe/stub the spawn without spinning a real runtime;
    /// production default is `lifecycle::spawn_agent_boxed`.
    pub spawn_fn: crate::lifecycle::SpawnFn,
    /// Converge mutual exclusion: at most one `POST /team/converge`
    /// run at a time. A second request is refused with `409` rather
    /// than queued — converge is an explicit operator action, and a
    /// collision means the operator should look at the current state
    /// before retrying, not wait behind a stale plan. The guard is
    /// held for the whole plan/preflight/execute pipeline via
    /// `try_lock`, so a crashed run cannot wedge the field.
    pub converge: tokio::sync::Mutex<()>,
}

/// Combined index: agent map + token-hash→id lookup + subagent reverse pointers.
/// All mutations go through methods that maintain invariants atomically.
///
/// **INVARIANT: at most one root entry.** A root is an entry whose
/// `config.created_by == None`. The tagma owns exactly one tagma-global root
/// agent, eagerly created at startup (see `routes::agent::ensure_root_agent`).
/// Production code inserts a root only through [`Self::register_root`], which
/// rejects a second; [`Self::register`] is reserved for subagents and for tests
/// that deliberately construct otherwise-invalid states.
pub struct AgentRegistry {
    agents: HashMap<AgentId, RegistryEntry>,
    /// SHA-256 of each **live** agent's auth token → its id. Faulted entries are
    /// never indexed: their token is minted fresh on each restore and never
    /// persisted, so a faulted entry (which never spawned) has no real hash and
    /// cannot authenticate. Keyed by hash so agent auth shares the operator's
    /// `TokenHash::of` → hash-compare path (consistency) — not for secret
    /// protection, since the plaintext still lives in [`Agent::env`] for shell
    /// injection.
    token_index: HashMap<TokenHash, AgentId>,
    /// Roster-mutation invalidation source: the mutators bump the generation so
    /// snapshot pumps wake instantly (see [`AppState::invalidations`]). Owns a
    /// private channel when built bare (the test-only `new`) — bumps land nowhere.
    invalidations: tokio::sync::watch::Sender<u64>,
}

/// Durable identity shared by live and faulted registry entries: the config
/// (created_by, role, description, workspace_root, permissions_class, agent_id)
/// and the on-disk directory. Everything a supervisor needs to list, authorize
/// against, relabel, or archive an agent -- independent of whether it currently
/// has a running task.
#[derive(Clone)]
pub struct AgentIdentity {
    pub config: AgentConfig,
    pub agent_dir: Option<PathBuf>,
}

/// The registry value: a live running agent, or a faulted placeholder that
/// could not be brought up (e.g. restore failure). The enum makes "is there a
/// live task?" a type-level question, forcing every runtime-field access to
/// consciously handle the faulted case.
#[allow(clippy::large_enum_variant)] // Live carries the full runtime handle set by design; Faulted is a placeholder
pub enum RegistryEntry {
    /// A live, running agent: durable identity + runtime handle + known children.
    Live(AgentEntry),
    /// Registered for visibility/management only -- no task, no channels. The
    /// supervisor chain still runs through it (chain walkers read `identity`).
    Faulted(FaultedEntry),
}

/// A live agent entry: durable identity, the running [`Agent`] handle, and the
/// ids of direct subagents this agent has spawned.
pub struct AgentEntry {
    pub identity: AgentIdentity,
    pub agent: Agent,
    pub subagent_ids: Vec<AgentId>,
}

/// A faulted agent entry: durable identity and known children, plus the reason
/// it could not be brought up. Surfaced via [`AgentSummary::faulted_reason`].
pub struct FaultedEntry {
    pub identity: AgentIdentity,
    pub subagent_ids: Vec<AgentId>,
    pub reason: String,
    /// Unix seconds when the fault was recorded (entry creation).
    pub at: u64,
}

/// Bridge-written parked snapshot: why the agent parked and when (the `when`
/// backs the kick turn's "parked N ago" text and is NOT persisted — a
/// restart degrades Parked to Idle per the restore semantics).
#[derive(Debug, Clone)]
pub struct ParkedSnapshot {
    pub reason: ParkedReason,
    pub at: std::time::Instant,
}

pub struct Agent {
    pub prompt_tx: mpsc::Sender<String>,
    pub events_tx: broadcast::Sender<SseEvent>,
    pub approvals: Arc<Mutex<ApprovalStore>>,
    /// Abort handle for the agent task. The task's `JoinHandle` is consumed by
    /// the panic watcher spawned next to it (see `agent_watch`), so this handle
    /// is the only way to stop the task from the outside.
    pub agent_abort: AbortHandle,
    /// The panic watcher: awaits the agent task and, if it died from a panic,
    /// swaps the registry entry from `Live` to `Faulted` in place (preserving
    /// the supervisor chain and map slot) so a panicked agent is visible and
    /// manageable instead of a silently dead `Live` entry. Clean exits and
    /// aborts leave the entry untouched.
    pub agent_watch: JoinHandle<()>,
    pub bridge_handle: JoinHandle<()>,
    pub store: Arc<Mutex<ContextStore>>,
    pub cancel: CancellationToken,
    /// The current round's cancellation token, reachable by `interrupt_agent`. `Some` only
    /// while a round is running; cancelling it aborts the round without terminating the
    /// task. Shared (same `Arc`) with the agent task's `AgentContext::round_cancel`.
    pub round_cancel: Arc<std::sync::Mutex<Option<RoundToken>>>,
    /// Wake signal triggered by external events (e.g. approval notifications).
    /// The agent task awaits this in the outer loop; callers signal via `notify_one()`.
    pub notify: Arc<Notify>,
    pub state: Arc<AtomicU8>,
    /// Unix seconds of the most recent `state` transition (creation sets the
    /// baseline). Written only beside a `state` store so the pair reads
    /// consistently; feeds `AgentSummary::state_since` for the fleet views.
    pub state_since: Arc<AtomicU64>,
    /// Ephemeral, agent-self-reported current activity ("reading docs/x.md").
    /// Written by `PUT /agents/{id}/activity` (the agent reports its own, via the
    /// `kallip activity` CLI), cleared by the bridge on terminal events, read
    /// by `list_agents`/`agent_status`. Not persisted — `AgentMeta` holds only the
    /// durable identity fields (`role`/`description`).
    pub activity: Arc<std::sync::Mutex<String>>,
    /// SHA-256 of the agent's auth token. The plaintext is injected into [`env`]
    /// (`KALLIP_AUTH_TOKEN`) for shell injection; only this hash is retained for lookup.
    pub auth_token_hash: TokenHash,
    /// Environment variables injected into agent shell sessions (KALLIP_ID, KALLIP_AUTH_TOKEN, etc.).
    /// Preserved across reactivation so the agent retains its identity. This is the
    /// sole home of the auth-token plaintext.
    pub env: HashMap<String, String>,
    /// The tagma-global `bash_exec` classify preset snapshot this agent was
    /// spawned under. Immutable for the agent's lifetime (the tagma's preset is
    /// fixed at startup); read by the runtime policy in `evaluate()`.
    pub preset: PolicyPreset,
    /// Shared `bash_exec` command-policy overrides. The tagma updates this via
    /// API (`PUT /exec-policy`); the runtime reads it in `evaluate()` for
    /// `bash_exec`. The only per-agent runtime-mutable policy knob.
    pub exec_policy: Arc<std::sync::RwLock<ExecPolicy>>,
    /// Per-agent execution gate coordinating this agent's shell forks (READ on
    /// the backend) with workspace carve-outs (WRITE here when a subagent is
    /// spawned under this agent). Stored on the agent so the carve-out paths
    /// (`Materialize::run`, `restore_one`) reach it via `AgentEntry`.
    pub exec_gate: Arc<kallip_runtime::ExecGate>,
    /// Pending profile-reset cell shared with the agent task's `AgentContext`.
    /// The apply route writes here; the agent drains it on its next wake-up.
    pub pending_profile_reset: Arc<std::sync::Mutex<Option<kallip_runtime::ProfileReset>>>,
    /// Parked snapshot, written by the bridge at a parking terminal event and
    /// cleared on any non-parked terminal. Shared (same `Arc`) with the bridge
    /// task; read by the delivery gate's auto-wake (kick turn text) and the status surfaces.
    pub parked: Arc<std::sync::Mutex<Option<ParkedSnapshot>>>,
    /// Armed chain-transient retry info, written by the bridge at an
    /// FCE-with-retry terminal event; cleared on any other terminal. Shared
    /// with the bridge; read by the status surfaces as the `retrying` field.
    pub retrying: Arc<std::sync::Mutex<Option<TransientRetryInfo>>>,
    /// Active-profile snapshot, shared (same `Arc`) with the runtime's
    /// [`kallip_runtime::FailoverState`]: the runtime's active-profile writers
    /// (spawn, failover advance, profile apply) keep it current; the tagma only
    /// reads it, for the status surfaces.
    pub profile_snapshot: Arc<std::sync::Mutex<kallip_runtime::ProfileSnapshot>>,
}

impl Agent {
    pub fn get_state(&self) -> AgentState {
        match self.state.load(Ordering::Relaxed) {
            AgentState::BUSY => AgentState::Busy,
            AgentState::WAITING => AgentState::Waiting,
            AgentState::PARKED => AgentState::Parked,
            AgentState::RETRYING => AgentState::Retrying,
            _ => AgentState::Idle,
        }
    }

    /// Snapshot the ephemeral activity string. Poison-tolerant (`into_inner`)
    /// so a prior panic in any cell holder cannot brick `list_agents` /
    /// `agent_status` for this agent — matches the `exec_policy` pattern.
    pub fn activity_snapshot(&self) -> String {
        self.activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Snapshot the parked reason for wire surfaces (`None` unless parked).
    pub fn parked_reason_snapshot(&self) -> Option<ParkedReason> {
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| p.reason.clone())
    }

    /// Snapshot the armed retry info for wire surfaces (`None` unless
    /// chain-transient backoff is armed).
    pub fn retrying_snapshot(&self) -> Option<TransientRetryInfo> {
        *self.retrying.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Snapshot the runtime-active profile for wire surfaces.
    pub fn active_profile_snapshot(&self) -> kallip_runtime::ProfileSnapshot {
        self.profile_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Await both background tasks, bounded by `timeout`; force-abort on overrun.
    ///
    /// The caller must have already signalled cancellation (`cancel.cancel()` or
    /// the tagma-wide `shutdown` token). Returns `true` if both tasks finished
    /// gracefully within the bound; otherwise force-aborts both and returns
    /// `false`. Consumes `self`, so all owned resources (store, channels, config)
    /// drop together once the tasks are done.
    ///
    /// The watcher is joined instead of the agent task itself: it resolves
    /// once the agent task has terminated (and any panic swap has been
    /// applied), so shutdown still covers the agent task end-to-end. The
    /// handles are awaited by reference: when the timeout fires the inner
    /// *non-move* async block is dropped and the field borrows are released,
    /// leaving `self` owning the handles so we can call `.abort()`.
    pub(crate) async fn shutdown(mut self, timeout: Duration) -> bool {
        let graceful = tokio::time::timeout(timeout, async {
            let _ = tokio::join!(&mut self.agent_watch, &mut self.bridge_handle);
        })
        .await
        .is_ok();
        if !graceful {
            self.agent_abort.abort();
            self.agent_watch.abort();
            self.bridge_handle.abort();
        }
        graceful
    }
}

impl AgentEntry {
    /// Ephemeral activity snapshot for the live-only [`AgentSummary`] activity
    /// field. Faulted entries report an empty activity.
    fn activity_for_summary(&self) -> String {
        self.agent.activity_snapshot()
    }
}

impl RegistryEntry {
    /// Durable identity (config + on-disk dir) -- available on both variants,
    /// so chain walkers, list, and metadata routes read uniformly.
    pub fn identity(&self) -> &AgentIdentity {
        match self {
            RegistryEntry::Live(e) => &e.identity,
            RegistryEntry::Faulted(e) => &e.identity,
        }
    }

    /// Mutable durable identity, for relabel writes (`update_metadata`).
    pub fn identity_mut(&mut self) -> &mut AgentIdentity {
        match self {
            RegistryEntry::Live(e) => &mut e.identity,
            RegistryEntry::Faulted(e) => &mut e.identity,
        }
    }

    /// Direct children of this entry -- maintained on both variants so a
    /// faulted parent still tracks the subagents it spawned before faulting
    /// (or that were restored under it).
    pub fn subagent_ids(&self) -> &Vec<AgentId> {
        match self {
            RegistryEntry::Live(e) => &e.subagent_ids,
            RegistryEntry::Faulted(e) => &e.subagent_ids,
        }
    }

    pub fn subagent_ids_mut(&mut self) -> &mut Vec<AgentId> {
        match self {
            RegistryEntry::Live(e) => &mut e.subagent_ids,
            RegistryEntry::Faulted(e) => &mut e.subagent_ids,
        }
    }

    /// The live agent handle, or `None` for a faulted entry. Callers that need
    /// runtime resources (channels, policies, task handles) branch on this and
    /// reject/skip faulted entries.
    pub fn as_live(&self) -> Option<&AgentEntry> {
        match self {
            RegistryEntry::Live(e) => Some(e),
            RegistryEntry::Faulted(_) => None,
        }
    }

    pub fn as_live_mut(&mut self) -> Option<&mut AgentEntry> {
        match self {
            RegistryEntry::Live(e) => Some(e),
            RegistryEntry::Faulted(_) => None,
        }
    }

    /// Lifecycle state this entry reports. Live entries read the bridge-owned
    /// atomic; faulted entries are always [`AgentState::Faulted`] (a
    /// wire/display state that is never stored atomically -- see
    /// [`AgentState`]). Used by [`Self::summary`] and directly where a caller
    /// needs just the state.
    pub fn state_for_summary(&self) -> AgentState {
        match self {
            RegistryEntry::Live(e) => e.agent.get_state(),
            RegistryEntry::Faulted(_) => AgentState::Faulted,
        }
    }

    /// Build the wire [`AgentSummary`] for either variant. The single
    /// construction site for list / metadata responses.
    pub fn summary(&self, id: &AgentId) -> AgentSummary {
        let identity = self.identity();
        let (activity, faulted_reason, parked_reason, retrying, state_since) = match self {
            RegistryEntry::Live(e) => {
                let agent = &e.agent;
                (
                    e.activity_for_summary(),
                    None,
                    agent.parked_reason_snapshot(),
                    agent.retrying_snapshot(),
                    Some(agent.state_since.load(Ordering::Relaxed)),
                )
            }
            RegistryEntry::Faulted(e) => (
                String::new(),
                Some(e.reason.clone()),
                None,
                None,
                Some(e.at),
            ),
        };
        AgentSummary {
            id: id.clone(),
            workspace_root: identity.config.workspace_root.display().to_string(),
            state: self.state_for_summary(),
            created_by: identity.config.created_by.clone(),
            role: identity.config.role.clone(),
            description: identity.config.description.clone(),
            profile_set: identity.config.profile_set.clone(),
            activity,
            duty: Default::default(),
            lock: None,
            parked_reason,
            retrying,
            faulted_reason,
            state_since,
            // Populated only by `get_root_agent` (the sole external-conversation
            // surface); absent on list/metadata summaries.
            conversation_id: None,
        }
    }
}

impl AppState {
    /// Build the wire summary for `entry` with the runtime joins applied:
    /// the duty-board reading and the workspace lock-visibility probe. The
    /// single summary-exit shape for every route returning an
    /// [`AgentSummary`] — keeping both joins in one place means no outlet
    /// can forget one half when a new field ships.
    pub fn summarize(&self, id: &AgentId, entry: &RegistryEntry) -> AgentSummary {
        let mut summary = entry.summary(id);
        summary.duty = self.duty.get(id);
        summary.lock = self.lock_visibility(id, entry);
        summary
    }

    /// Probe whether a live Normal-class agent holds the write-lock on its
    /// workspace root. Guests never lock and faulted agents hold nothing,
    /// so both report `None` (absence is normal there); any other live
    /// agent probing `missing` is the lock-evaporation red flag.
    fn lock_visibility(&self, id: &AgentId, entry: &RegistryEntry) -> Option<LockState> {
        if entry.state_for_summary() == AgentState::Faulted
            || entry.identity().config.permissions_class != PermissionClass::Normal
        {
            return None;
        }
        let ws = &entry.identity().config.workspace_root;
        let held = self.lock_manager.holds_exact(id, ws).unwrap_or(false);
        Some(if held {
            LockState::Held
        } else {
            LockState::Missing
        })
    }

    /// Test-only constructor with generous resource limits.
    #[cfg(test)]
    pub fn new(operator_token_hash: TokenHash, profiles: Arc<ArcSwap<ProfileBundle>>) -> Self {
        Self::new_with_preset(operator_token_hash, profiles, PolicyPreset::Default)
    }

    /// Test-only constructor with a custom tagma-global preset.
    #[cfg(test)]
    pub fn new_with_preset(
        operator_token_hash: TokenHash,
        profiles: Arc<ArcSwap<ProfileBundle>>,
        preset: PolicyPreset,
    ) -> Self {
        Self::with_limits(
            operator_token_hash,
            crate::args::MAX_AGENTS_LIMIT,
            crate::args::MAX_SUBAGENTS_LIMIT,
            5,
            profiles,
            preset,
            kallip_runtime::token_budget::TokenBudget::unlimited(),
        )
    }

    /// Production constructor with resource limits from CLI args.
    pub fn with_limits(
        operator_token_hash: TokenHash,
        max_agents: usize,
        max_subagents: usize,
        prompt_queue_size: usize,
        profiles: Arc<ArcSwap<ProfileBundle>>,
        preset: PolicyPreset,
        token_budget: kallip_runtime::token_budget::TokenBudget,
    ) -> Self {
        let (invalidations, _) = tokio::sync::watch::channel(0u64);
        Self {
            spawn_fn: crate::lifecycle::spawn_agent_boxed(),
            registry: RwLock::new(AgentRegistry::with_invalidation(invalidations.clone())),
            preset,
            hook_rules: std::sync::OnceLock::new(),
            shutdown: CancellationToken::new(),
            operator_token_hash,
            max_agents,
            max_subagents,
            prompt_queue_size,
            token_budget,
            profiles,
            files_http: reqwest::Client::new(),
            lock_manager: Arc::new(kallip_runtime::dirlock::DirLockManager::new()),
            relays: std::sync::Mutex::new(HashMap::new()),
            bus: crate::bus::tagma_bus().expect("static topic registry is conflict-free"),
            external: std::sync::OnceLock::new(),
            joined_rooms: Arc::new(JoinedRooms::new()),
            direct_sessions: Arc::new(DirectSessions::new()),
            inboxes: std::sync::OnceLock::new(),
            duty: Arc::new(crate::duty::DutyStore::with_invalidation(
                invalidations.clone(),
            )),
            work_schedules: std::sync::OnceLock::new(),
            tasks: std::sync::OnceLock::new(),
            task_blobs: std::sync::OnceLock::new(),
            converge: tokio::sync::Mutex::new(()),
            invalidations,
        }
    }

    /// Startup budget resolution for the tagma binary: `KALLIP_TOKEN_BUDGET`
    /// sets a finite limit (pure number or K/M/G suffix, the CLI amount
    /// grammar); unset means unlimited. A set-but-invalid value, **including
    /// the empty string**, is a boot error rather than a silent fallback to
    /// unlimited — falling back would defeat an operator cap that failed to
    /// parse (e.g. a typo'd value with a trailing space). `Some("0")` boots
    /// paused — the same semantics as `budget set 0`, valid but unusual.
    ///
    /// The env read and the `?`-propagated error live in the binary's
    /// startup (anyhow context), not here: this is a pure parse with no
    /// process state.
    pub(crate) fn startup_token_budget(
        raw: Option<String>,
    ) -> anyhow::Result<kallip_runtime::token_budget::TokenBudget> {
        Ok(match raw {
            None => kallip_runtime::token_budget::TokenBudget::unlimited(),
            Some(raw) => {
                let value = kallip_common::tokens::parse_token_amount(&raw)
                    .map_err(|err| anyhow::anyhow!("invalid value {raw:?}: {err}"))?;
                kallip_runtime::token_budget::TokenBudget::new(value, 0)
            }
        })
    }
}

impl AppState {
    /// Bump the invalidation generation: every subscribed snapshot pump wakes
    /// on its next poll and re-snapshots (the pump's differential gate
    /// absorbs no-op wakes). Best-effort: no receivers is fine.
    pub fn invalidate(&self) {
        let generation = *self.invalidations.borrow();
        let _ = self.invalidations.send(generation + 1);
    }

    /// Subscribe a snapshot pump to the invalidation source. The watch
    /// semantics give the conduwuit-style behavior for free: mutations that
    /// land while the pump is busy coalesce into one wake.
    pub fn subscribe_invalidations(&self) -> tokio::sync::watch::Receiver<u64> {
        self.invalidations.subscribe()
    }

    /// Install the named relay connector + its run-task handle, at startup.
    /// Called from `main` after `ensure_root_agent` (once per successfully
    /// activated entry). Must not use `Arc::get_mut` — the root agent already
    /// holds `Arc<AppState>` clones by this point.
    pub fn set_relay(
        &self,
        name: &str,
        handle: crate::relay::RelayHandle,
        join: tokio::task::JoinHandle<()>,
    ) {
        let mut slots = self.relays.lock().unwrap_or_else(|e| e.into_inner());
        slots.insert(name.to_owned(), (handle, join));
    }

    /// Clone the named relay connector out for a route (guard dropped before
    /// the caller awaits). `None` when that entry is not online.
    pub fn relay(&self, name: &str) -> Option<crate::relay::RelayHandle> {
        let slots = self.relays.lock().unwrap_or_else(|e| e.into_inner());
        slots.get(name).map(|(handle, _)| handle.clone())
    }

    /// Clone every installed relay connector out (order: map iteration --
    /// nondeterministic; a caller needing one specific relay uses
    /// [`Self::relay`] with a cache-owner name instead). Guards dropped
    /// before the caller awaits.
    pub fn relay_handles(&self) -> Vec<crate::relay::RelayHandle> {
        let slots = self.relays.lock().unwrap_or_else(|e| e.into_inner());
        slots.values().map(|(handle, _)| handle.clone()).collect()
    }

    /// Take every relay connector + run-task handle out so graceful shutdown
    /// can drain them all. The map is left empty; the lesche message routes
    /// are not served during the shutdown drain.
    pub fn take_relays(
        &self,
    ) -> Vec<(
        String,
        crate::relay::RelayHandle,
        tokio::task::JoinHandle<()>,
    )> {
        let mut slots = self.relays.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .drain()
            .map(|(name, (handle, join))| (name, handle, join))
            .collect()
    }
}

impl AgentRegistry {
    /// Bare constructor: bumps land on a private channel (test-friendly — no
    /// receiver, no observable effect). The AppState constructors install the
    /// shared channel via [`Self::with_invalidation`].
    #[cfg(test)]
    pub fn new() -> Self {
        let (invalidations, _) = tokio::sync::watch::channel(0u64);
        Self {
            agents: HashMap::new(),
            token_index: HashMap::new(),
            invalidations,
        }
    }

    /// Registry wired to the tagma's invalidation channel: roster mutations
    /// wake every subscribed snapshot pump.
    pub fn with_invalidation(invalidations: tokio::sync::watch::Sender<u64>) -> Self {
        Self {
            agents: HashMap::new(),
            token_index: HashMap::new(),
            invalidations,
        }
    }

    /// Bump the invalidation generation (roster changed). Best-effort: no
    /// receivers is fine.
    fn notify_invalidation(&self) {
        let generation = *self.invalidations.borrow();
        let _ = self.invalidations.send(generation + 1);
    }

    // -- read helpers --

    pub fn get(&self, id: &AgentId) -> Option<&RegistryEntry> {
        self.agents.get(id)
    }

    pub fn get_mut(&mut self, id: &AgentId) -> Option<&mut RegistryEntry> {
        self.agents.get_mut(id)
    }

    pub fn contains_key(&self, id: &AgentId) -> bool {
        self.agents.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&AgentId, &RegistryEntry)> {
        self.agents.iter()
    }

    pub fn get_agent_id_by_token(&self, hash: &TokenHash) -> Option<&AgentId> {
        self.token_index.get(hash)
    }

    // -- write helpers --

    /// Insert an entry, update the supervisor's `subagent_ids`, and -- for live
    /// entries only -- index the auth-token hash. Faulted entries are never
    /// token-indexed (see [`AgentRegistry`] doc).
    ///
    /// Eagerly links the entry under its supervisor if the supervisor is already
    /// registered. This always succeeds in the create path (supervisor is
    /// validated first) and in the restore path (top-down BFS guarantees the
    /// supervisor is registered first). If the supervisor isn't registered
    /// (e.g. an orphaned faulted entry whose supervisor's data is gone), the
    /// push is silently skipped -- safe, the link just isn't established.
    pub fn register(&mut self, id: AgentId, entry: RegistryEntry) {
        if let Some(ref supervisor_id) = entry.identity().config.created_by
            && let Some(supervisor) = self.agents.get_mut(supervisor_id)
        {
            supervisor.subagent_ids_mut().push(id.clone());
        }
        if let RegistryEntry::Live(live) = &entry {
            self.token_index
                .insert(live.agent.auth_token_hash.clone(), id.clone());
        }
        self.agents.insert(id, entry);
        self.notify_invalidation();
    }

    /// Insert the tagma's single root agent. This is the **only** production
    /// path that registers a root; it rejects a second root to uphold the
    /// singleton invariant documented on [`AgentRegistry`]. Equivalent to
    /// [`Self::register`] for a `created_by == None` entry, plus the uniqueness
    /// check. Callers already own the `id` (passed in), so nothing is returned
    /// on success.
    pub fn register_root(&mut self, id: AgentId, entry: RegistryEntry) -> Result<(), ApiError> {
        if entry.identity().config.created_by.is_some() {
            return Err(ApiError::internal(
                "register_root: entry is not a root (created_by is set)",
            ));
        }
        if self.root_agent().is_some() {
            return Err(ApiError::conflict(
                "a root agent already exists; the tagma owns exactly one root",
            ));
        }
        // Delegate to the raw inserter; the root has no supervisor so the
        // subagent-push branch is a no-op.
        self.register(id, entry);
        Ok(())
    }

    /// Like [`Self::register`], but skips the `subagent_ids` push.
    /// Used by `create_agent` which pre-reserves the slot before spawning.
    pub fn register_no_subagent_push(&mut self, id: AgentId, entry: RegistryEntry) {
        if let RegistryEntry::Live(live) = &entry {
            self.token_index
                .insert(live.agent.auth_token_hash.clone(), id.clone());
        }
        self.agents.insert(id, entry);
        self.notify_invalidation();
    }

    /// Remove an entry, unregister its token hash (live only), and drop it from
    /// the supervisor's `subagent_ids`.
    pub fn unregister(&mut self, id: &AgentId) -> Option<RegistryEntry> {
        let entry = self.agents.remove(id)?;
        if let RegistryEntry::Live(live) = &entry {
            self.token_index.remove(&live.agent.auth_token_hash);
        }
        if let Some(ref supervisor_id) = entry.identity().config.created_by
            && let Some(supervisor) = self.agents.get_mut(supervisor_id)
        {
            supervisor.subagent_ids_mut().retain(|sid| sid != id);
        }
        self.notify_invalidation();
        Some(entry)
    }

    /// Remove and return every entry, clearing the token index.
    ///
    /// Used at tagma shutdown to take ownership of all entries so live task
    /// handles can be awaited without holding the registry lock. Faulted
    /// entries are returned too; the shutdown caller simply has no task to
    /// await for them.
    pub fn drain(&mut self) -> Vec<(AgentId, RegistryEntry)> {
        self.token_index.clear();
        let drained = self.agents.drain().collect::<Vec<_>>();
        self.notify_invalidation();
        drained
    }

    // -- authorization helpers --

    /// Walk the `created_by` chain from `start_id` upward with cycle detection.
    pub fn walk_supervisor_chain(
        &self,
        start_id: &AgentId,
    ) -> Result<Vec<&RegistryEntry>, ApiError> {
        let mut visited = HashSet::new();
        let mut current_id = start_id.clone();
        let mut chain = Vec::new();
        loop {
            if !visited.insert(current_id.clone()) {
                return Err(ApiError::forbidden("circular supervisor chain"));
            }
            let entry = self
                .get(&current_id)
                .ok_or_else(|| ApiError::forbidden("broken supervisor chain"))?;
            chain.push(entry);
            match &entry.identity().config.created_by {
                Some(supervisor_id) => current_id = supervisor_id.clone(),
                None => break,
            }
        }
        Ok(chain)
    }

    /// The strict delegation ancestors of an agent whose supervisor is
    /// `start_supervisor_id` — i.e. the `created_by` chain `[start_supervisor_id,
    /// …, root]` as owned [`AgentId`]s. Passed into
    /// [`DirLockManager::acquire`](kallip_runtime::dirlock::DirLockManager::acquire)
    /// so a nested lock held under an ancestor is treated as delegation rather
    /// than conflict. Mirrors [`Self::walk_supervisor_chain`]'s cycle detection;
    /// returns owned ids so the caller may drop the registry read guard before
    /// calling the (sync) lock manager.
    pub fn supervisor_chain_ids(
        &self,
        start_supervisor_id: &AgentId,
    ) -> Result<Vec<AgentId>, ApiError> {
        let mut visited = HashSet::new();
        let mut current_id = start_supervisor_id.clone();
        let mut ids = Vec::new();
        loop {
            if !visited.insert(current_id.clone()) {
                return Err(ApiError::forbidden("circular supervisor chain"));
            }
            let entry = self
                .get(&current_id)
                .ok_or_else(|| ApiError::forbidden("broken supervisor chain"))?;
            ids.push(current_id.clone());
            match &entry.identity().config.created_by {
                Some(supervisor_id) => current_id = supervisor_id.clone(),
                None => break,
            }
        }
        Ok(ids)
    }

    /// Relation of `sender_id` to `receiver`, where `sender_id == None` denotes
    /// the operator. Informational only -- it never gates authorization. Returns
    /// [`SenderRelation::Unknown`](crate::messaging::SenderRelation::Unknown)
    /// only when neither a superior nor subordinate relation can be established
    /// *and* at least one chain walk failed; an intact hierarchy always resolves
    /// to one of the other variants.
    ///
    /// Reuses [`Self::supervisor_chain_ids`] (which already detects cycles and
    /// broken links); strict ancestors are the chain entries after index 0.
    pub fn relation_of(
        &self,
        sender_id: Option<&AgentId>,
        receiver: &AgentId,
    ) -> crate::messaging::SenderRelation {
        use crate::messaging::SenderRelation;

        let Some(id) = sender_id else {
            return SenderRelation::Operator;
        };
        if id == receiver {
            return SenderRelation::Same;
        }

        // `supervisor_chain_ids` returns `[start, ..., root]` (owned ids) and
        // `Err` on a broken/cyclic chain. `skip(1)` drops the start node so only
        // strict ancestors count. Each chain is walked at most once: a Superior
        // match returns after the first walk, and the failed-walk flag is reused
        // for the Unknown fallback (no re-walk).
        let receiver_chain = self.supervisor_chain_ids(receiver);
        if matches!(&receiver_chain, Ok(chain) if chain.iter().skip(1).any(|a| a == id)) {
            return SenderRelation::Superior; // sender outranks receiver
        }
        let sender_chain = self.supervisor_chain_ids(id);
        if matches!(&sender_chain, Ok(chain) if chain.iter().skip(1).any(|a| a == receiver)) {
            return SenderRelation::Subordinate; // receiver outranks sender
        }
        // Neither ancestor relation matched. If either walk failed, the chain is
        // corrupt enough that we cannot confidently call it a peer.
        if receiver_chain.is_err() || sender_chain.is_err() {
            SenderRelation::Unknown
        } else {
            SenderRelation::Peer
        }
    }

    /// Caller must be the operator or the direct supervisor of the subagent being created.
    /// Returns the supervisor's entry for delegation checks.
    pub fn require_supervisor(
        &self,
        identity: &crate::auth::Identity,
        supervisor_id: &AgentId,
    ) -> Result<&RegistryEntry, ApiError> {
        let supervisor = self.get(supervisor_id).ok_or_else(|| {
            ApiError::not_found(format!("supervisor agent {supervisor_id} not found"))
        })?;
        match identity {
            crate::auth::Identity::Operator => Ok(supervisor),
            crate::auth::Identity::Agent { id } if id == supervisor_id => Ok(supervisor),
            _ => Err(ApiError::forbidden(
                "invalid auth token for supervisor agent",
            )),
        }
    }

    /// Caller must be the operator or a superior of the target agent.
    pub fn require_superior(
        &self,
        identity: &crate::auth::Identity,
        target_id: &AgentId,
    ) -> Result<(), ApiError> {
        match identity {
            crate::auth::Identity::Operator => return Ok(()),
            crate::auth::Identity::Agent { id: caller_id } => {
                let chain = self.walk_supervisor_chain(target_id)?;
                if chain
                    .iter()
                    .any(|e| e.identity().config.created_by.as_ref() == Some(caller_id))
                {
                    return Ok(());
                }
            }
        }
        Err(ApiError::forbidden("not authorized to manage this agent"))
    }

    /// Caller must be the operator or the **direct** supervisor of the target
    /// (`target.created_by == Some(caller)`). Stricter than [`Self::require_superior`]
    /// — grandparents may not relabel a grandchild without going through the parent.
    /// Used by `PUT /agents/{id}/metadata`: the entity that assigned the role at
    /// spawn is the entity that may change it. A root target (`created_by = None`)
    /// has no supervisor, so only the operator may relabel it.
    pub fn require_direct_supervisor(
        &self,
        identity: &crate::auth::Identity,
        target_id: &AgentId,
    ) -> Result<(), ApiError> {
        let target = self
            .get(target_id)
            .ok_or_else(|| ApiError::not_found(format!("agent {target_id} not found")))?;
        match identity {
            crate::auth::Identity::Operator => Ok(()),
            crate::auth::Identity::Agent { id: caller_id } => {
                match &target.identity().config.created_by {
                    Some(parent) if parent == caller_id => Ok(()),
                    _ => Err(ApiError::forbidden(
                        "only the direct supervisor may change this agent's metadata",
                    )),
                }
            }
        }
    }

    /// Caller must be the operator or the agent identified by `target_id`.
    /// Used for self-only actions (e.g. activity self-report). (A supervisor
    /// manages a subagent's `role`/`description` via
    /// [`Self::require_direct_supervisor`]; this is the complementary self-write.)
    pub fn require_self_or_operator(
        &self,
        identity: &crate::auth::Identity,
        target_id: &AgentId,
    ) -> Result<(), ApiError> {
        match identity {
            crate::auth::Identity::Operator => Ok(()),
            crate::auth::Identity::Agent { id } if id == target_id => Ok(()),
            _ => Err(ApiError::forbidden(
                "only the agent itself or operator is authorized for this action",
            )),
        }
    }

    /// Caller must be exactly the agent identified by `target_id` — strictly
    /// self-only, with **no operator override**. Used for actions that speak
    /// *as* the agent (today: `kallip lesche send`, which delivers a chat
    /// message the end user attributes to the agent). Letting the operator in
    /// here would let it forge an agent's voice to the user; an operator
    /// announcement, if ever needed, is a separate route with its own sender
    /// identity, not this one. Compare [`Self::require_self_or_operator`], which
    /// permits the operator for self-write actions that do not impersonate the
    /// agent.
    pub fn require_self(
        &self,
        identity: &crate::auth::Identity,
        target_id: &AgentId,
    ) -> Result<(), ApiError> {
        match identity {
            crate::auth::Identity::Agent { id } if id == target_id => Ok(()),
            _ => Err(ApiError::forbidden(
                "only the agent itself may send as that agent",
            )),
        }
    }

    /// Return the tagma's single root agent (`created_by` is `None`), live or
    /// faulted, or `None` during the startup window before one exists. Per the
    /// [`AgentRegistry`] invariant there is at most one root, so this is a
    /// singleton lookup, not a filter. Callers that need a running task must
    /// skip [`RegistryEntry::Faulted`].
    pub fn root_agent(&self) -> Option<(&AgentId, &RegistryEntry)> {
        self.agents
            .iter()
            .find(|(_, e)| e.identity().config.is_root())
    }
}

#[cfg(test)]
mod tests;
