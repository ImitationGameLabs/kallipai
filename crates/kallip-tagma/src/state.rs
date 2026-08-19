use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
pub use kallip_common::agentid::AgentId;
use kallip_common::authtoken::TokenHash;
use kallip_common::policy::{ExecPolicy, PolicyPreset};
pub use kallip_common::protocol::AgentState;
pub use kallip_common::protocol::AgentSummary;
use kallip_common::protocol::ApiError;
use kallip_common::protocol::SseEvent;
use kallip_common::protocol::{ParkedReason, TransientRetryInfo};
use kallip_runtime::agent_task::RoundToken;
use kallip_runtime::approval::ApprovalStore;
use kallip_runtime::config::AgentConfig;
use kallip_runtime::context::ContextStore;
use kallip_runtime::profile::{ProfileConfig, ProfileRegistry};
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub type SharedState = Arc<AppState>;

/// Atomic-swap container for the full profile state: the serializable config
/// (for GET /profiles) and the assembled registry (for agent spawn + apply).
/// Swapped as a unit on PUT /profiles via [`ArcSwap`]; readers load a
/// consistent snapshot. Each running agent pins its own `Arc<ProfileRegistry>`
/// snapshot in its [`FailoverState`] — a swap does not disturb running agents
/// until an explicit apply.
pub struct ProfileBundle {
    /// The config as loaded (GET) or written (PUT) — serializable, no backends.
    pub config: ProfileConfig,
    /// The registry built from `config` — carries pre-built backends.
    pub registry: Arc<ProfileRegistry>,
}

/// Tagma-side cache of the rooms this tagma belongs to. Rooms are plaintext
/// server-readable (the lesche enforces member access), so the cache is pure
/// routing state: it tells the relay inbound fork and the agent's room
/// send/read/list routes whether a given conversation id is a room envelope
/// (vs the bilateral 1:1 conversation). Populated from the `list_my_rooms` poll
/// (the room-membership pump in `relay::room_poll`, whose immediate first tick
/// warms it on tunnel-up) and refreshed on each `Wake` nudge.
///
/// Best-effort: a cold or stale miss for a room routes the envelope to the
/// bilateral path, where it is dropped (the lesche member-gated read still
/// works) -- self-correcting on the next poll that warms the entry.
#[derive(Default)]
pub struct JoinedRooms {
    rooms: Mutex<HashSet<kallip_lesche_common::rooms::RoomId>>,
}

impl JoinedRooms {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `room` is a room this tagma belongs to.
    pub async fn is_joined(&self, room: &kallip_lesche_common::rooms::RoomId) -> bool {
        self.rooms.lock().await.contains(room)
    }

    /// Replace the room set from a fresh `list_my_rooms` snapshot.
    pub async fn set_joined_rooms(
        &self,
        rooms: impl IntoIterator<Item = kallip_lesche_common::rooms::RoomId>,
    ) {
        let mut g = self.rooms.lock().await;
        g.clear();
        g.extend(rooms);
    }

    /// Snapshot of the room ids (for the agent's room-list route).
    pub async fn joined_rooms(&self) -> HashSet<kallip_lesche_common::rooms::RoomId> {
        self.rooms.lock().await.clone()
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
    /// Tagma-wide directory write-lock coordinator. Shared across all agents so
    /// one agent holding a dir's write-lock blocks another. The tagma build
    /// enforces locks via landlock on Linux (mandatory); advisory elsewhere.
    pub lock_manager: Arc<kallip_runtime::dirlock::DirLockManager>,
    /// Optional online-mode relay connector (set once at startup when
    /// `KALLIP_TAGMA_RELAY_AGORA_URL` is configured), plus its long-running tunnel
    /// task's `JoinHandle` so graceful shutdown can drain it. `None` in
    /// pure-local deployments and during the degrade-to-local-only path.
    ///
    /// Interior-mutable (not set via `Arc::get_mut`) because the root agent's
    /// bridge/agent tasks already hold `Arc<AppState>` clones by the time the
    /// relay is installed. Read under the mutex (e.g. by the lesche message route).
    pub relay: std::sync::Mutex<Option<(crate::relay::RelayHandle, tokio::task::JoinHandle<()>)>>,
    /// The direct (local, non-relay) serving path, always present. Installed
    /// once at startup via [`Self::set_direct`] after the `Arc<AppState>` exists
    /// (its pumps hold a `Weak<AppState>` and a child of `shutdown`). Serves the
    /// external event vocabulary over a plain SSE to any local frontend client.
    pub direct: std::sync::OnceLock<crate::direct::DirectServing>,
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
    /// Per-agent message inboxes (SQLite-backed). Installed at startup.
    /// The off-duty gate buffers messages here; the phase
    /// executor flushes them on wake-up.
    pub inboxes: std::sync::OnceLock<crate::inbox::InboxStore>,
    /// Per-agent duty status. The off-duty gate checks this before
    /// delivering external messages; off-duty agents buffer to inbox.
    pub duty: Arc<crate::duty::DutyStore>,
    /// SQLite-backed work-schedule store. Opened at startup.
    pub work_schedules: std::sync::OnceLock<crate::work_schedule::WorkScheduleStore>,
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
}

/// Durable identity shared by live and faulted registry entries: the config
/// (created_by, role, description, workspace_root, permissions_class, agent_id)
/// and the on-disk directory. Everything a supervisor needs to list, authorize
/// against, relabel, or archive an agent -- independent of whether it currently
/// has a running task.
pub struct AgentIdentity {
    pub config: AgentConfig,
    pub agent_dir: Option<PathBuf>,
}

/// The registry value: a live running agent, or a faulted placeholder that
/// could not be brought up (e.g. restore failure). The enum makes "is there a
/// live task?" a type-level question, forcing every runtime-field access to
/// consciously handle the faulted case.
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
}

/// Bridge-written parked snapshot: why the agent parked and when (the `when`
/// backs the kick turn's "parked N ago" text and is NOT persisted — a
/// restart degrades Parked to Idle per the design's restore semantics).
#[derive(Debug, Clone)]
pub struct ParkedSnapshot {
    pub reason: ParkedReason,
    pub at: std::time::Instant,
}

pub struct Agent {
    pub prompt_tx: mpsc::Sender<String>,
    pub events_tx: broadcast::Sender<SseEvent>,
    pub approvals: Arc<Mutex<ApprovalStore>>,
    pub agent_handle: JoinHandle<()>,
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
    /// task; read by the wake route (kick turn text) and the status surfaces.
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
    /// The handles are awaited by reference: when the timeout fires the inner
    /// *non-move* async block is dropped and the field borrows are released,
    /// leaving `self` owning the handles so we can call `.abort()`. (A `JoinSet`
    /// would not work here — aborting its wrapper tasks only drops the
    /// `JoinHandle`s, which does not abort the underlying tasks.)
    pub(crate) async fn shutdown(mut self, timeout: Duration) -> bool {
        let graceful = tokio::time::timeout(timeout, async {
            let _ = tokio::join!(&mut self.agent_handle, &mut self.bridge_handle);
        })
        .await
        .is_ok();
        if !graceful {
            self.agent_handle.abort();
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
        let (activity, faulted_reason, parked_reason, retrying) = match self {
            RegistryEntry::Live(e) => {
                let agent = &e.agent;
                (
                    e.activity_for_summary(),
                    None,
                    agent.parked_reason_snapshot(),
                    agent.retrying_snapshot(),
                )
            }
            RegistryEntry::Faulted(e) => (String::new(), Some(e.reason.clone()), None, None),
        };
        AgentSummary {
            id: id.clone(),
            workspace_root: identity.config.workspace_root.display().to_string(),
            state: self.state_for_summary(),
            created_by: identity.config.created_by.clone(),
            role: identity.config.role.clone(),
            description: identity.config.description.clone(),
            activity,
            duty: Default::default(),
            parked_reason,
            retrying,
            faulted_reason,
            // Populated only by `get_root_agent` (the sole external-conversation
            // surface); absent on list/metadata summaries.
            conversation_id: None,
        }
    }
}

impl AppState {
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
        Self {
            registry: RwLock::new(AgentRegistry::new()),
            preset,
            hook_rules: std::sync::OnceLock::new(),
            shutdown: CancellationToken::new(),
            operator_token_hash,
            max_agents: crate::args::MAX_AGENTS_LIMIT,
            max_subagents: crate::args::MAX_SUBAGENTS_LIMIT,
            prompt_queue_size: 5,
            token_budget: kallip_runtime::token_budget::TokenBudget::new(
                kallip_common::protocol::DEFAULT_TOKEN_BUDGET,
                0,
            ),
            profiles,
            lock_manager: Arc::new(kallip_runtime::dirlock::DirLockManager::new()),
            relay: std::sync::Mutex::new(None),
            direct: std::sync::OnceLock::new(),
            external: std::sync::OnceLock::new(),
            joined_rooms: Arc::new(JoinedRooms::new()),
            inboxes: std::sync::OnceLock::new(),
            duty: Arc::new(crate::duty::DutyStore::new()),
            work_schedules: std::sync::OnceLock::new(),
        }
    }

    /// Production constructor with resource limits from CLI args.
    pub fn with_limits(
        operator_token_hash: TokenHash,
        max_agents: usize,
        max_subagents: usize,
        prompt_queue_size: usize,
        profiles: Arc<ArcSwap<ProfileBundle>>,
        preset: PolicyPreset,
    ) -> Self {
        Self {
            registry: RwLock::new(AgentRegistry::new()),
            preset,
            hook_rules: std::sync::OnceLock::new(),
            shutdown: CancellationToken::new(),
            operator_token_hash,
            max_agents,
            max_subagents,
            prompt_queue_size,
            token_budget: kallip_runtime::token_budget::TokenBudget::new(
                kallip_common::protocol::DEFAULT_TOKEN_BUDGET,
                0,
            ),
            profiles,
            lock_manager: Arc::new(kallip_runtime::dirlock::DirLockManager::new()),
            relay: std::sync::Mutex::new(None),
            direct: std::sync::OnceLock::new(),
            external: std::sync::OnceLock::new(),
            joined_rooms: Arc::new(JoinedRooms::new()),
            inboxes: std::sync::OnceLock::new(),
            duty: Arc::new(crate::duty::DutyStore::new()),
            work_schedules: std::sync::OnceLock::new(),
        }
    }
}

impl AppState {
    /// Install the relay connector + its run-task handle, once, at startup.
    /// Called from `main` after `ensure_root_agent`. Must not use `Arc::get_mut`
    /// — the root agent already holds `Arc<AppState>` clones by this point.
    pub fn set_relay(&self, handle: crate::relay::RelayHandle, join: tokio::task::JoinHandle<()>) {
        let mut slot = self.relay.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some((handle, join));
    }

    /// Take the relay connector + its run-task handle out so graceful shutdown
    /// can drain the task. Returns `None` in local-only deployments. The slot is
    /// left `None`; the lesche message route is not served during the shutdown drain.
    pub fn take_relay(&self) -> Option<(crate::relay::RelayHandle, tokio::task::JoinHandle<()>)> {
        let mut slot = self.relay.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    }

    /// Install the direct serving handle, once, at startup. Called from `main`
    /// after the `Arc<AppState>` exists (the direct pumps hold a `Weak` to it).
    /// No `take`: direct runs for the tagma's lifetime and is drained by its
    /// pumps' `shutdown` child token, not by dropping the handle.
    pub fn set_direct(&self, serving: crate::direct::DirectServing) {
        // OnceLock: a second install is a logic bug — surface it loudly.
        if self.direct.set(serving).is_err() {
            panic!("direct serving must be installed once at startup");
        }
    }
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
            token_index: HashMap::new(),
        }
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
        self.agents.drain().collect()
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
