use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crate::skill_promote::SkillPromoteStore;
use crate::token::TokenHash;
pub use just_agent_common::agentid::AgentId;
use just_agent_common::policy::ToolPolicy;
pub use just_agent_common::protocol::AgentState;
pub use just_agent_common::protocol::AgentSummary;
use just_agent_common::protocol::ApiError;
use just_agent_common::protocol::SseEvent;
use just_agent_runtime::agent_task::RoundToken;
use just_agent_runtime::approval::ApprovalStore;
use just_agent_runtime::config::AgentConfig;
use just_agent_runtime::context::ContextStore;
use just_agent_runtime::profile::ProfileRegistry;
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub type SharedState = Arc<AppState>;

pub struct AppState {
    /// Agent registry. **Lock order:** this RwLock must be acquired before
    /// any `tool_policy` std::sync::RwLock inside agent entries.
    pub registry: RwLock<AgentRegistry>,
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
    /// Daemon-wide token budget shared by all agents.
    pub token_budget: just_agent_runtime::token_budget::TokenBudget,
    /// Profile registry loaded once at startup (config file or implicit env profile).
    /// Shared so the pre-built backends survive across agents.
    pub profiles: Arc<ProfileRegistry>,
}

/// Combined index: agent map + token-hash→id lookup + subagent reverse pointers.
/// All mutations go through methods that maintain invariants atomically.
pub struct AgentRegistry {
    agents: HashMap<AgentId, AgentEntry>,
    /// SHA-256 of each agent's auth token → its id. Keyed by hash so agent auth
    /// shares the operator's `TokenHash::of` → hash-compare path (consistency) — not
    /// for secret protection, since the plaintext still lives in [`Agent::env`] for
    /// shell injection.
    token_index: HashMap<TokenHash, AgentId>,
}

pub struct AgentEntry {
    pub agent: Agent,
    pub subagent_ids: Vec<AgentId>,
}

pub struct Agent {
    pub prompt_tx: mpsc::Sender<String>,
    pub events_tx: broadcast::Sender<SseEvent>,
    pub approvals: Arc<Mutex<ApprovalStore>>,
    pub config: AgentConfig,
    pub agent_handle: JoinHandle<()>,
    pub bridge_handle: JoinHandle<()>,
    pub store: Arc<Mutex<ContextStore>>,
    pub agent_dir: Option<PathBuf>,
    pub cancel: CancellationToken,
    /// The current round's cancellation token, reachable by `interrupt_agent`. `Some` only
    /// while a round is running; cancelling it aborts the round without terminating the
    /// task. Shared (same `Arc`) with the agent task's `AgentContext::round_cancel`.
    pub round_cancel: Arc<std::sync::Mutex<Option<RoundToken>>>,
    /// Wake signal triggered by external events (e.g. approval notifications).
    /// The agent task awaits this in the outer loop; callers signal via `notify_one()`.
    pub notify: Arc<Notify>,
    pub state: Arc<AtomicU8>,
    /// SHA-256 of the agent's auth token. The plaintext is injected into [`env`]
    /// (`JUST_AGENT_AUTH_TOKEN`) for shell injection; only this hash is retained for lookup.
    pub auth_token_hash: TokenHash,
    /// Environment variables injected into agent shell sessions (JUST_AGENT_ID, JUST_AGENT_AUTH_TOKEN, etc.).
    /// Preserved across reactivation so the agent retains its identity. This is the
    /// sole home of the auth-token plaintext.
    pub env: HashMap<String, String>,
    /// Shared tool policy. The daemon updates this via API; the runtime reads it in evaluate().
    pub tool_policy: Arc<std::sync::RwLock<ToolPolicy>>,
}

impl Agent {
    pub fn get_state(&self) -> AgentState {
        match self.state.load(Ordering::Relaxed) {
            AgentState::BUSY => AgentState::Busy,
            _ => AgentState::Idle,
        }
    }

    /// Await both background tasks, bounded by `timeout`; force-abort on overrun.
    ///
    /// The caller must have already signalled cancellation (`cancel.cancel()` or
    /// the daemon-wide `shutdown` token). Returns `true` if both tasks finished
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

impl AppState {
    /// Test-only constructor with generous resource limits.
    #[cfg(test)]
    pub fn new(operator_token_hash: TokenHash, profiles: Arc<ProfileRegistry>) -> Self {
        Self {
            registry: RwLock::new(AgentRegistry::new()),
            skill_promote_store: Mutex::new(SkillPromoteStore::new()),
            skill_write_lock: Mutex::new(()),
            shutdown: CancellationToken::new(),
            operator_token_hash,
            max_agents: crate::args::MAX_AGENTS_LIMIT,
            max_subagents: crate::args::MAX_SUBAGENTS_LIMIT,
            prompt_queue_size: 5,
            token_budget: just_agent_runtime::token_budget::TokenBudget::new(
                just_agent_common::protocol::DEFAULT_TOKEN_BUDGET,
                0,
            ),
            profiles,
        }
    }

    /// Production constructor with resource limits from CLI args.
    pub fn with_limits(
        operator_token_hash: TokenHash,
        max_agents: usize,
        max_subagents: usize,
        prompt_queue_size: usize,
        profiles: Arc<ProfileRegistry>,
    ) -> Self {
        Self {
            registry: RwLock::new(AgentRegistry::new()),
            skill_promote_store: Mutex::new(SkillPromoteStore::new()),
            skill_write_lock: Mutex::new(()),
            shutdown: CancellationToken::new(),
            operator_token_hash,
            max_agents,
            max_subagents,
            prompt_queue_size,
            token_budget: just_agent_runtime::token_budget::TokenBudget::new(
                just_agent_common::protocol::DEFAULT_TOKEN_BUDGET,
                0,
            ),
            profiles,
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

    pub fn get(&self, id: &AgentId) -> Option<&AgentEntry> {
        self.agents.get(id)
    }

    pub fn get_mut(&mut self, id: &AgentId) -> Option<&mut AgentEntry> {
        self.agents.get_mut(id)
    }

    pub fn contains_key(&self, id: &AgentId) -> bool {
        self.agents.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&AgentId, &AgentEntry)> {
        self.agents.iter()
    }

    pub fn get_agent_id_by_token(&self, hash: &TokenHash) -> Option<&AgentId> {
        self.token_index.get(hash)
    }

    // -- write helpers --

    /// Insert agent, index its token hash, update supervisor's subagent_ids.
    pub fn register(&mut self, id: AgentId, entry: AgentEntry) {
        // Eagerly link: if the supervisor is already registered, update its
        // subagent_ids now. This always succeeds in the create path
        // (supervisor is validated before we get here) and in the restore
        // path (top-down BFS guarantees supervisor is registered first).
        if let Some(ref supervisor_id) = entry.agent.config.created_by
            && let Some(supervisor) = self.agents.get_mut(supervisor_id)
        {
            supervisor.subagent_ids.push(id.clone());
        }
        self.token_index
            .insert(entry.agent.auth_token_hash.clone(), id.clone());
        self.agents.insert(id, entry);
    }

    /// Like [`Self::register`], but skips the subagent_ids push.
    /// Used by `create_agent` which pre-reserves the slot before spawning.
    pub fn register_no_subagent_push(&mut self, id: AgentId, entry: AgentEntry) {
        self.token_index
            .insert(entry.agent.auth_token_hash.clone(), id.clone());
        self.agents.insert(id, entry);
    }

    /// Remove agent, unregister its token hash, update supervisor's subagent_ids.
    pub fn unregister(&mut self, id: &AgentId) -> Option<AgentEntry> {
        let entry = self.agents.remove(id)?;
        self.token_index.remove(&entry.agent.auth_token_hash);
        if let Some(ref supervisor_id) = entry.agent.config.created_by
            && let Some(supervisor) = self.agents.get_mut(supervisor_id)
        {
            supervisor.subagent_ids.retain(|sid| sid != id);
        }
        Some(entry)
    }

    /// Remove and return every entry, clearing the token index.
    ///
    /// Used at daemon shutdown to take ownership of all agents so their task
    /// handles can be awaited without holding the registry lock.
    pub fn drain(&mut self) -> Vec<(AgentId, AgentEntry)> {
        self.token_index.clear();
        self.agents.drain().collect()
    }

    // -- authorization helpers --

    /// Walk the `created_by` chain from `start_id` upward with cycle detection.
    pub fn walk_supervisor_chain(&self, start_id: &AgentId) -> Result<Vec<&AgentEntry>, ApiError> {
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
            match &entry.agent.config.created_by {
                Some(supervisor_id) => current_id = supervisor_id.clone(),
                None => break,
            }
        }
        Ok(chain)
    }

    /// Caller must be the operator or the direct supervisor of the subagent being created.
    /// Returns the supervisor's `AgentEntry` for delegation checks.
    pub fn require_supervisor(
        &self,
        identity: &crate::auth::Identity,
        supervisor_id: &AgentId,
    ) -> Result<&AgentEntry, ApiError> {
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
                    .any(|e| e.agent.config.created_by.as_ref() == Some(caller_id))
                {
                    return Ok(());
                }
            }
        }
        Err(ApiError::forbidden("not authorized to manage this agent"))
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
                if entry.agent.config.created_by.is_none() {
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
    /// Used for promote-request submission (agents submit on their own behalf).
    pub fn require_self_or_operator(
        &self,
        identity: &crate::auth::Identity,
        target_id: &AgentId,
    ) -> Result<(), ApiError> {
        match identity {
            crate::auth::Identity::Operator => Ok(()),
            crate::auth::Identity::Agent { id } if id == target_id => Ok(()),
            _ => Err(ApiError::forbidden(
                "only the agent itself or operator can submit promote requests",
            )),
        }
    }

    /// Return all root agents (created_by is None).
    pub fn root_agents(&self) -> Vec<(&AgentId, &AgentEntry)> {
        self.agents
            .iter()
            .filter(|(_, e)| e.agent.config.created_by.is_none())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Identity;
    use crate::test_helpers::*;
    use crate::token::TokenHash;

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
        reg.register(id.clone(), make_entry(None, token.into()));
        assert!(reg.contains_key(&id));
        assert_eq!(reg.get_agent_id_by_token(&hash), Some(&id));

        let removed = reg.unregister(&id).unwrap();
        assert_eq!(removed.agent.auth_token_hash, hash);
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
        assert_eq!(reg.get(&sup).unwrap().subagent_ids, vec![child]);
    }

    #[tokio::test]
    async fn unregister_removes_subagent_pointer() {
        let mut reg = AgentRegistry::new();
        let sup = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &sup);
        add_sub(&mut reg, &child, &sup);
        reg.unregister(&child).unwrap();
        assert!(reg.get(&sup).unwrap().subagent_ids.is_empty());
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
        assert!(chain[2].agent.config.created_by.is_none()); // root
    }

    #[tokio::test]
    async fn walk_chain_rejects_cycle() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let b = AgentId::random();
        reg.register(a.clone(), make_entry(Some(b.clone()), "aa".into()));
        reg.register(b, make_entry(Some(a.clone()), "ab".into()));
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
        reg.register(a.clone(), make_entry(Some(ghost), "a".into()));
        match reg.walk_supervisor_chain(&a) {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected broken chain error"),
        }
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

    // -- root_agents --

    #[tokio::test]
    async fn root_agents_returns_only_roots() {
        let mut reg = AgentRegistry::new();
        let root1 = AgentId::random();
        let root2 = AgentId::random();
        let child = AgentId::random();
        add_root(&mut reg, &root1);
        add_root(&mut reg, &root2);
        add_sub(&mut reg, &child, &root1);
        let roots: Vec<_> = reg.root_agents().into_iter().map(|(id, _)| id).collect();
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&&root1));
        assert!(roots.contains(&&root2));
    }

    // -- Resource limits in AppState --

    #[test]
    fn with_limits_sets_max_agents() {
        let state = AppState::with_limits(TokenHash::of("tok"), 50, 20, 5, make_profile_registry());
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
