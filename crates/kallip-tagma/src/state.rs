use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crate::skill_promote::SkillPromoteStore;
pub use kallip_common::agentid::AgentId;
use kallip_common::authtoken::TokenHash;
use kallip_common::policy::{ExecPolicy, PolicyPreset};
pub use kallip_common::protocol::AgentState;
pub use kallip_common::protocol::AgentSummary;
use kallip_common::protocol::ApiError;
use kallip_common::protocol::SseEvent;
use kallip_runtime::agent_task::RoundToken;
use kallip_runtime::approval::ApprovalStore;
use kallip_runtime::config::AgentConfig;
use kallip_runtime::context::ContextStore;
use kallip_runtime::profile::ProfileRegistry;
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub type SharedState = Arc<AppState>;

pub struct AppState {
    /// Agent registry. **Lock order:** this RwLock must be acquired before
    /// any per-agent `exec_policy` std::sync::RwLock inside agent entries.
    pub registry: RwLock<AgentRegistry>,
    /// Tagma-global `bash_exec` classify preset, read once at startup from
    /// `KALLIP_POLICY_PRESET` and immutable for the tagma's lifetime. Every agent
    /// inherits this same preset (it is not per-agent).
    pub preset: PolicyPreset,
    pub skill_promote_store: Mutex<SkillPromoteStore>,
    /// Serializes writes to the shared skill directory during promote-request
    /// approval. Held across the consistency check and the actual write so
    /// concurrent approve operations cannot interleave.
    ///
    /// **Lock order:** always acquired *inside* the `skill_promote_store` Mutex
    /// (see `routes::skill_promote::handle_approve`). Never acquire in the
    /// reverse order — the store lock is the coarse-grained gate, this lock
    /// is the fine-grained skill-filesystem gate.
    pub skill_write_lock: Mutex<()>,
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
    pub profiles: Arc<ProfileRegistry>,
    /// Tagma-wide directory write-lock coordinator. Shared across all agents so
    /// one agent holding a dir's write-lock blocks another. The tagma build
    /// enforces locks via landlock on Linux (mandatory); advisory elsewhere.
    pub lock_manager: Arc<kallip_runtime::dirlock::DirLockManager>,
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
}

impl Agent {
    pub fn get_state(&self) -> AgentState {
        match self.state.load(Ordering::Relaxed) {
            AgentState::BUSY => AgentState::Busy,
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
        let (activity, faulted_reason) = match self {
            RegistryEntry::Live(e) => (e.activity_for_summary(), None),
            RegistryEntry::Faulted(e) => (String::new(), Some(e.reason.clone())),
        };
        AgentSummary {
            id: id.clone(),
            workspace_root: identity.config.workspace_root.display().to_string(),
            state: self.state_for_summary(),
            created_by: identity.config.created_by.clone(),
            role: identity.config.role.clone(),
            description: identity.config.description.clone(),
            activity,
            faulted_reason,
        }
    }
}

impl AppState {
    /// Test-only constructor with generous resource limits.
    #[cfg(test)]
    pub fn new(operator_token_hash: TokenHash, profiles: Arc<ProfileRegistry>) -> Self {
        Self::new_with_preset(operator_token_hash, profiles, PolicyPreset::Default)
    }

    /// Test-only constructor with a custom tagma-global preset.
    #[cfg(test)]
    pub fn new_with_preset(
        operator_token_hash: TokenHash,
        profiles: Arc<ProfileRegistry>,
        preset: PolicyPreset,
    ) -> Self {
        Self {
            registry: RwLock::new(AgentRegistry::new()),
            preset,
            skill_promote_store: Mutex::new(SkillPromoteStore::new()),
            skill_write_lock: Mutex::new(()),
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
        }
    }

    /// Production constructor with resource limits from CLI args.
    pub fn with_limits(
        operator_token_hash: TokenHash,
        max_agents: usize,
        max_subagents: usize,
        prompt_queue_size: usize,
        profiles: Arc<ProfileRegistry>,
        preset: PolicyPreset,
    ) -> Self {
        Self {
            registry: RwLock::new(AgentRegistry::new()),
            preset,
            skill_promote_store: Mutex::new(SkillPromoteStore::new()),
            skill_write_lock: Mutex::new(()),
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

    /// Caller must be the operator or a root agent (created_by is None).
    /// Used for promote-request review operations.
    pub fn require_root_or_operator(
        &self,
        identity: &crate::auth::Identity,
    ) -> Result<(), ApiError> {
        match identity {
            crate::auth::Identity::Operator => Ok(()),
            crate::auth::Identity::Agent { id } => {
                let entry = self
                    .get(id)
                    .ok_or_else(|| ApiError::forbidden("unknown agent"))?;
                if entry.identity().config.created_by.is_none() {
                    Ok(())
                } else {
                    Err(ApiError::forbidden(
                        "only root agents or operators can review promote requests",
                    ))
                }
            }
        }
    }

    /// Caller must be the operator or the agent identified by `target_id`.
    /// Used for self-only actions: promote-request submission, activity
    /// self-report. (A supervisor manages a subagent's `role`/`description` via
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

    /// Return the tagma's single root agent (created_by is None), live or
    /// faulted, or `None` during the startup window before one exists. Per the
    /// [`AgentRegistry`] invariant there is at most one root, so this is a
    /// singleton lookup, not a filter. Callers that need a running task (e.g.
    /// promote-request notification) must skip [`RegistryEntry::Faulted`].
    pub fn root_agent(&self) -> Option<(&AgentId, &RegistryEntry)> {
        self.agents
            .iter()
            .find(|(_, e)| e.identity().config.created_by.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Identity;
    use crate::test_helpers::*;
    use kallip_common::authtoken::TokenHash;

    // -- Agent::shutdown: bounded graceful task drain --

    #[tokio::test]
    async fn agent_shutdown_aborts_straggler_after_timeout() {
        use std::sync::atomic::AtomicBool;

        // An abortable straggler: `tokio::time::sleep` yields (so the timeout can
        // fire on a single-thread runtime) and respects cancellation. shutdown
        // must time out, return false, and abort the task before it sets the flag.
        let completed = Arc::new(AtomicBool::new(false));
        let flag = completed.clone();
        let mut entry = make_entry(None, "tok".into());
        entry.agent.agent_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            flag.store(true, Ordering::SeqCst);
        });
        assert!(!entry.agent.shutdown(Duration::from_millis(50)).await);
        // Aborted before the 60s sleep elapsed, so the completion flag stays unset.
        assert!(!completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn agent_shutdown_graceful_when_fast() {
        // `make_entry` spawns two instantly-completing tasks.
        let entry = make_entry(None, "tok".into());
        assert!(entry.agent.shutdown(Duration::from_secs(1)).await);
    }

    // -- interrupt: cancels the round token, never the lifecycle token --

    /// The core invariant: interrupt cancels only the current round token, so the
    /// agent task returns to its outer loop instead of terminating.
    #[tokio::test]
    async fn interrupt_cancels_round_not_lifecycle() {
        let entry = make_entry(None, "tok".into());
        let round = RoundToken::new(&entry.agent.cancel);
        // Simulate a round in flight: publish the round token into the slot.
        *entry.agent.round_cancel.lock().unwrap() = Some(round.clone());

        // Mirror `interrupt_agent`'s logic: cancel the slot's token, not the lifecycle.
        if let Some(rc) = entry.agent.round_cancel.lock().unwrap().clone() {
            rc.cancel();
        }

        assert!(
            round.handle().is_cancelled(),
            "round token cancelled by interrupt"
        );
        assert!(
            !entry.agent.cancel.is_cancelled(),
            "lifecycle token must NOT be cancelled by interrupt"
        );
    }

    /// With no round in flight the slot is `None`, so interrupt is a clean no-op.
    #[tokio::test]
    async fn interrupt_when_idle_is_noop() {
        let entry = make_entry(None, "tok".into());
        assert!(entry.agent.round_cancel.lock().unwrap().is_none());

        if let Some(rc) = entry.agent.round_cancel.lock().unwrap().clone() {
            rc.cancel();
        }

        assert!(!entry.agent.cancel.is_cancelled());
        assert!(entry.agent.round_cancel.lock().unwrap().is_none());
    }

    // -- Registry consistency: agents + token_index + subagent_ids stay in sync --

    #[tokio::test]
    async fn register_unregister_syncs_token_index() {
        let mut reg = AgentRegistry::new();
        let id = AgentId::random();
        // The registry indexes by the agent's token hash, derived inside make_entry.
        let token = "test-token";
        let hash = TokenHash::of(token);
        reg.register(
            id.clone(),
            RegistryEntry::Live(make_entry(None, token.into())),
        );
        assert!(reg.contains_key(&id));
        assert_eq!(reg.get_agent_id_by_token(&hash), Some(&id));

        let removed = reg.unregister(&id).unwrap();
        let removed_live = match removed {
            RegistryEntry::Live(l) => l,
            RegistryEntry::Faulted(_) => panic!("expected live entry"),
        };
        assert_eq!(removed_live.agent.auth_token_hash, hash);
        assert!(!reg.contains_key(&id));
        assert!(reg.get_agent_id_by_token(&hash).is_none());
    }

    #[tokio::test]
    async fn register_links_subagent_to_supervisor() {
        let mut reg = AgentRegistry::new();
        let sup = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &sup);
        add_sub(&mut reg, &child, &sup);
        assert_eq!(reg.get(&sup).unwrap().subagent_ids(), &vec![child]);
    }

    #[tokio::test]
    async fn unregister_removes_subagent_pointer() {
        let mut reg = AgentRegistry::new();
        let sup = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &sup);
        add_sub(&mut reg, &child, &sup);
        reg.unregister(&child).unwrap();
        assert!(reg.get(&sup).unwrap().subagent_ids().is_empty());
    }

    // -- Supervisor chain walking --

    #[tokio::test]
    async fn walk_chain_traverses_ancestors() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let b = AgentId::random();
        let c = AgentId::random();
        add_root(&mut reg, &a);
        add_sub(&mut reg, &b, &a);
        add_sub(&mut reg, &c, &b);
        let chain = reg.walk_supervisor_chain(&c).unwrap();
        assert_eq!(chain.len(), 3);
        assert!(chain[2].identity().config.created_by.is_none()); // root
    }

    #[tokio::test]
    async fn walk_chain_rejects_cycle() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let b = AgentId::random();
        reg.register(
            a.clone(),
            RegistryEntry::Live(make_entry(Some(b.clone()), "aa".into())),
        );
        reg.register(
            b,
            RegistryEntry::Live(make_entry(Some(a.clone()), "ab".into())),
        );
        match reg.walk_supervisor_chain(&a) {
            Err(e) => {
                assert_eq!(e.status, 403);
                assert!(e.message.contains("circular"));
            }
            Ok(_) => panic!("expected cycle error"),
        }
    }

    #[tokio::test]
    async fn walk_chain_rejects_broken_link() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let ghost = AgentId::random();
        reg.register(
            a.clone(),
            RegistryEntry::Live(make_entry(Some(ghost), "a".into())),
        );
        match reg.walk_supervisor_chain(&a) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected broken chain error"),
        }
    }

    // -- Faulted entries: chain integrity, registration, authorization --

    /// The headline fix: a supervisor chain walks cleanly through faulted
    /// nodes, so a superior can authorize against a faulted descendant. Today
    /// the whole subtree vanishes and the walk 403s ("broken supervisor chain").
    #[tokio::test]
    async fn walk_chain_traverses_faulted_nodes() {
        let mut reg = AgentRegistry::new();
        let root = AgentId::random();
        let mid = AgentId::random();
        let leaf = AgentId::random();
        add_root(&mut reg, &root);
        add_faulted_sub(&mut reg, &mid, &root, "mid restore failed");
        add_faulted_sub(&mut reg, &leaf, &mid, "leaf restore failed");
        let chain = reg.walk_supervisor_chain(&leaf).expect("chain is intact");
        assert_eq!(chain.len(), 3);
        // The faulted nodes are present and report their state for summaries.
        assert_eq!(chain[0].state_for_summary(), AgentState::Faulted);
        assert_eq!(chain[1].state_for_summary(), AgentState::Faulted);
    }

    /// A faulted entry links to a live supervisor via the eager subagent-push,
    /// so `subagent list` on the supervisor includes the faulted child.
    #[tokio::test]
    async fn register_faulted_links_to_live_supervisor() {
        let mut reg = AgentRegistry::new();
        let sup = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &sup);
        add_faulted_sub(&mut reg, &child, &sup, "boom");
        assert!(reg.get(&sup).unwrap().subagent_ids().contains(&child));
    }

    /// A faulted child links to a faulted supervisor too (subtree stays connected).
    #[tokio::test]
    async fn register_faulted_links_to_faulted_supervisor() {
        let mut reg = AgentRegistry::new();
        let sup = AgentId::random();
        let child = AgentId::random();
        add_faulted_root(&mut reg, &sup, "sup restore failed");
        add_faulted_sub(&mut reg, &child, &sup, "child restore failed");
        assert!(reg.get(&sup).unwrap().subagent_ids().contains(&child));
    }

    /// A faulted entry is never inserted into the token index: it has no auth
    /// token (the token is minted fresh on each restore and never persisted), so
    /// it must not be authenticatable.
    #[tokio::test]
    async fn register_faulted_not_in_token_index() {
        let mut reg = AgentRegistry::new();
        let id = AgentId::random();
        add_faulted_root(&mut reg, &id, "missing workspace");
        // Any hash lookup misses -- a faulted agent cannot authenticate.
        assert!(
            reg.get_agent_id_by_token(&TokenHash::of("anything"))
                .is_none()
        );
    }

    /// Operator and a live ancestor both pass `require_superior` against a
    /// faulted descendant -- the chain is walkable, so management works.
    #[tokio::test]
    async fn require_superior_succeeds_through_faulted() {
        let mut reg = AgentRegistry::new();
        let root = AgentId::random();
        let faulted_child = AgentId::random();
        add_root(&mut reg, &root);
        add_faulted_sub(&mut reg, &faulted_child, &root, "restore failed");
        assert!(
            reg.require_superior(&Identity::Operator, &faulted_child)
                .is_ok()
        );
        assert!(
            reg.require_superior(&Identity::Agent { id: root.clone() }, &faulted_child)
                .is_ok()
        );
    }

    /// `drain` returns both live and faulted entries so the shutdown caller can
    /// await live tasks and drop faulted ones.
    #[tokio::test]
    async fn drain_returns_both_variants() {
        let mut reg = AgentRegistry::new();
        let live = AgentId::random();
        let faulted = AgentId::random();
        add_root(&mut reg, &live);
        add_faulted_root(&mut reg, &faulted, "broken");
        let drained = reg.drain();
        assert_eq!(drained.len(), 2);
        assert!(
            drained
                .iter()
                .any(|(_, e)| matches!(e, RegistryEntry::Live(_)))
        );
        assert!(
            drained
                .iter()
                .any(|(_, e)| matches!(e, RegistryEntry::Faulted(_)))
        );
    }

    // -- relation_of --

    #[tokio::test]
    async fn relation_of_operator_is_operator() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        add_root(&mut reg, &a);
        assert_eq!(
            reg.relation_of(None, &a),
            crate::messaging::SenderRelation::Operator
        );
    }

    #[tokio::test]
    async fn relation_of_self_is_same() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        add_root(&mut reg, &a);
        assert_eq!(
            reg.relation_of(Some(&a), &a),
            crate::messaging::SenderRelation::Same
        );
    }

    #[tokio::test]
    async fn relation_of_direct_and_transitive_parent_is_superior() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let b = AgentId::random();
        let c = AgentId::random();
        add_root(&mut reg, &a);
        add_sub(&mut reg, &b, &a);
        add_sub(&mut reg, &c, &b);
        let superior = crate::messaging::SenderRelation::Superior;
        // a is grandparent of c, b is parent of c.
        assert_eq!(reg.relation_of(Some(&a), &c), superior);
        assert_eq!(reg.relation_of(Some(&b), &c), superior);
    }

    #[tokio::test]
    async fn relation_of_child_is_subordinate() {
        let mut reg = AgentRegistry::new();
        let parent = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &parent);
        add_sub(&mut reg, &child, &parent);
        // Child messaging parent: child is subordinate (receiver is its ancestor).
        assert_eq!(
            reg.relation_of(Some(&child), &parent),
            crate::messaging::SenderRelation::Subordinate
        );
    }

    #[tokio::test]
    async fn relation_of_sibling_and_unrelated_are_peers() {
        let mut reg = AgentRegistry::new();
        let parent = AgentId::random();
        let sib1 = AgentId::random();
        let sib2 = AgentId::random();
        add_root(&mut reg, &parent);
        add_sub(&mut reg, &sib1, &parent);
        add_sub(&mut reg, &sib2, &parent);
        let peer = crate::messaging::SenderRelation::Peer;
        assert_eq!(reg.relation_of(Some(&sib1), &sib2), peer);
        // Unrelated roots are also peers.
        let other = AgentId::random();
        add_root(&mut reg, &other);
        assert_eq!(reg.relation_of(Some(&other), &parent), peer);
    }

    #[tokio::test]
    async fn relation_of_broken_chain_is_unknown() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let ghost = AgentId::random();
        reg.register(
            a.clone(),
            RegistryEntry::Live(make_entry(Some(ghost), "a".into())),
        );
        // Self short-circuits before any walk, so a broken chain is irrelevant.
        assert_eq!(
            reg.relation_of(Some(&a), &a),
            crate::messaging::SenderRelation::Same
        );
        let b = AgentId::random();
        add_root(&mut reg, &b);
        // a's chain is broken; relation to the unrelated root b is unknowable.
        let unknown = crate::messaging::SenderRelation::Unknown;
        assert_eq!(reg.relation_of(Some(&a), &b), unknown);
        assert_eq!(reg.relation_of(Some(&b), &a), unknown);
    }

    // -- Authorization: require_superior --

    #[tokio::test]
    async fn superior_operator_bypasses_all() {
        let mut reg = AgentRegistry::new();
        let target = AgentId::random();
        add_root(&mut reg, &target);
        reg.require_superior(&Identity::Operator, &target).unwrap();
    }

    #[tokio::test]
    async fn superior_ancestor_accepted() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let b = AgentId::random();
        let c = AgentId::random();
        add_root(&mut reg, &a);
        add_sub(&mut reg, &b, &a);
        add_sub(&mut reg, &c, &b);
        // a is grand-supervisor of c.
        reg.require_superior(&Identity::Agent { id: a.clone() }, &c)
            .unwrap();
    }

    #[tokio::test]
    async fn superior_rejects_unrelated() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let other = AgentId::random();
        add_root(&mut reg, &a);
        add_root(&mut reg, &other);
        match reg.require_superior(&Identity::Agent { id: other }, &a) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    #[tokio::test]
    async fn superior_rejects_child_accessing_parent() {
        let mut reg = AgentRegistry::new();
        let parent = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &parent);
        add_sub(&mut reg, &child, &parent);
        match reg.require_superior(&Identity::Agent { id: child }, &parent) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    // -- Authorization: require_direct_supervisor (PUT /metadata) --

    #[tokio::test]
    async fn direct_supervisor_operator_bypasses() {
        let mut reg = AgentRegistry::new();
        let target = AgentId::random();
        add_root(&mut reg, &target);
        reg.require_direct_supervisor(&Identity::Operator, &target)
            .unwrap();
    }

    #[tokio::test]
    async fn direct_supervisor_accepts_direct_parent() {
        let mut reg = AgentRegistry::new();
        let parent = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &parent);
        add_sub(&mut reg, &child, &parent);
        reg.require_direct_supervisor(&Identity::Agent { id: parent }, &child)
            .unwrap();
    }

    #[tokio::test]
    async fn direct_supervisor_rejects_grandparent() {
        // require_superior allows ancestors; require_direct_supervisor does not.
        let mut reg = AgentRegistry::new();
        let grandparent = AgentId::random();
        let parent = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &grandparent);
        add_sub(&mut reg, &parent, &grandparent);
        add_sub(&mut reg, &child, &parent);
        match reg.require_direct_supervisor(&Identity::Agent { id: grandparent }, &child) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    #[tokio::test]
    async fn direct_supervisor_rejects_unrelated() {
        let mut reg = AgentRegistry::new();
        let parent = AgentId::random();
        let child = AgentId::random();
        let other = AgentId::random();
        add_root(&mut reg, &parent);
        add_sub(&mut reg, &child, &parent);
        add_root(&mut reg, &other);
        match reg.require_direct_supervisor(&Identity::Agent { id: other }, &child) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    #[tokio::test]
    async fn direct_supervisor_root_target_only_operator() {
        // A root agent (created_by None) has no supervisor → only the operator.
        let mut reg = AgentRegistry::new();
        let root = AgentId::random();
        let other = AgentId::random();
        add_root(&mut reg, &root);
        add_root(&mut reg, &other);
        match reg.require_direct_supervisor(&Identity::Agent { id: other }, &root) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    #[tokio::test]
    async fn direct_supervisor_missing_target_is_not_found() {
        let reg = AgentRegistry::new();
        let ghost = AgentId::random();
        match reg.require_direct_supervisor(&Identity::Operator, &ghost) {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("expected NOT_FOUND"),
        }
    }

    // -- Authorization: require_supervisor --

    #[tokio::test]
    async fn supervisor_allows_operator_and_self() {
        let mut reg = AgentRegistry::new();
        let id = AgentId::random();
        add_root(&mut reg, &id);
        reg.require_supervisor(&Identity::Operator, &id).unwrap();
        reg.require_supervisor(&Identity::Agent { id: id.clone() }, &id)
            .unwrap();
    }

    #[tokio::test]
    async fn supervisor_rejects_wrong_identity() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let other = AgentId::random();
        add_root(&mut reg, &a);
        add_root(&mut reg, &other);
        match reg.require_supervisor(&Identity::Agent { id: other }, &a) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    #[tokio::test]
    async fn supervisor_returns_not_found_for_missing() {
        let reg = AgentRegistry::new();
        let ghost = AgentId::random();
        match reg.require_supervisor(&Identity::Operator, &ghost) {
            Err(e) => assert_eq!(e.status, 404),
            Ok(_) => panic!("expected NOT_FOUND"),
        }
    }

    // -- Authorization: require_root_or_operator --

    #[tokio::test]
    async fn root_or_operator_allows_operator() {
        let reg = AgentRegistry::new();
        reg.require_root_or_operator(&Identity::Operator).unwrap();
    }

    #[tokio::test]
    async fn root_or_operator_allows_root_agent() {
        let mut reg = AgentRegistry::new();
        let root = AgentId::random();
        add_root(&mut reg, &root);
        reg.require_root_or_operator(&Identity::Agent { id: root })
            .unwrap();
    }

    #[tokio::test]
    async fn root_or_operator_rejects_subagent() {
        let mut reg = AgentRegistry::new();
        let root = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &root);
        add_sub(&mut reg, &child, &root);
        match reg.require_root_or_operator(&Identity::Agent { id: child }) {
            Err(e) => {
                assert_eq!(e.status, 403);
                assert!(e.message.contains("root agents"));
            }
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    // -- Authorization: require_self_or_operator --

    #[tokio::test]
    async fn self_or_operator_allows_operator() {
        let mut reg = AgentRegistry::new();
        let id = AgentId::random();
        add_root(&mut reg, &id);
        reg.require_self_or_operator(&Identity::Operator, &id)
            .unwrap();
    }

    #[tokio::test]
    async fn self_or_operator_allows_self() {
        let mut reg = AgentRegistry::new();
        let id = AgentId::random();
        add_root(&mut reg, &id);
        reg.require_self_or_operator(&Identity::Agent { id: id.clone() }, &id)
            .unwrap();
    }

    #[tokio::test]
    async fn self_or_operator_rejects_other_agent() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let b = AgentId::random();
        add_root(&mut reg, &a);
        add_root(&mut reg, &b);
        match reg.require_self_or_operator(&Identity::Agent { id: b }, &a) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    #[tokio::test]
    async fn self_or_operator_rejects_supervisor_of_target() {
        // Pins the self-only invariant for `PUT /agents/{id}/activity`: a parent
        // (supervisor) must NOT write a subagent's activity — only the agent
        // itself. Guards against a future swap to `require_direct_supervisor`.
        let mut reg = AgentRegistry::new();
        let parent = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &parent);
        add_sub(&mut reg, &child, &parent);
        match reg.require_self_or_operator(&Identity::Agent { id: parent }, &child) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    // -- root_agent (singleton) --

    #[tokio::test]
    async fn root_agent_returns_the_single_root() {
        let mut reg = AgentRegistry::new();
        assert!(reg.root_agent().is_none());

        let root = AgentId::random();
        let child = AgentId::random();
        reg.register_root(
            root.clone(),
            RegistryEntry::Live(make_entry(None, format!("agent-{root}"))),
        )
        .unwrap();
        add_sub(&mut reg, &child, &root);

        let (found_id, _) = reg.root_agent().expect("root present");
        assert_eq!(found_id, &root);
    }

    #[tokio::test]
    async fn register_root_rejects_a_second_root() {
        let mut reg = AgentRegistry::new();
        let root = AgentId::random();
        reg.register_root(
            root.clone(),
            RegistryEntry::Live(make_entry(None, format!("agent-{root}"))),
        )
        .unwrap();
        // A second root violates the singleton invariant.
        let dup = AgentId::random();
        let err = reg
            .register_root(
                dup.clone(),
                RegistryEntry::Live(make_entry(None, format!("agent-{dup}"))),
            )
            .unwrap_err();
        assert_eq!(err.status, 409);
        // The original root is unaffected.
        assert_eq!(reg.root_agent().unwrap().0, &root);
    }

    // -- Resource limits in AppState --

    #[test]
    fn with_limits_sets_max_agents() {
        let state = AppState::with_limits(
            TokenHash::of("tok"),
            50,
            20,
            5,
            make_profile_registry(),
            PolicyPreset::Default,
        );
        assert_eq!(state.max_agents, 50);
        assert_eq!(state.max_subagents, 20);
        assert_eq!(state.prompt_queue_size, 5);
    }

    #[test]
    fn new_has_generous_limits() {
        let state = AppState::new(TokenHash::of("tok"), make_profile_registry());
        assert_eq!(state.max_agents, crate::args::MAX_AGENTS_LIMIT);
        assert_eq!(state.max_subagents, crate::args::MAX_SUBAGENTS_LIMIT);
    }
}
