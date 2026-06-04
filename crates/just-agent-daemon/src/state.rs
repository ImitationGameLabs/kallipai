use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::skill_promote::SkillPromoteStore;
use axum::http::StatusCode;
pub use just_agent_common::agentid::AgentId;
use just_agent_common::command::UserInput;
use just_agent_common::policy::ToolPolicy;
pub use just_agent_common::protocol::AgentState;
pub use just_agent_common::protocol::AgentSummary;
use just_agent_common::protocol::SseEvent;
use just_agent_runtime::approval::ApprovalStore;
use just_agent_runtime::config::AgentConfig;
use just_agent_runtime::context::ContextStore;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub type SharedState = Arc<AppState>;

pub struct AppState {
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
    pub operator_token: String,
}

/// Combined index: agent map + token→id lookup + subagent reverse pointers.
/// All mutations go through methods that maintain invariants atomically.
pub struct AgentRegistry {
    agents: HashMap<AgentId, AgentEntry>,
    token_index: HashMap<String, AgentId>,
}

pub struct AgentEntry {
    pub agent: Agent,
    pub subagent_ids: Vec<AgentId>,
}

pub struct Agent {
    pub prompt_tx: mpsc::Sender<UserInput>,
    pub events_tx: broadcast::Sender<SseEvent>,
    pub approvals: Arc<Mutex<ApprovalStore>>,
    pub config: AgentConfig,
    pub agent_handle: JoinHandle<()>,
    pub bridge_handle: JoinHandle<()>,
    pub store: Arc<Mutex<ContextStore>>,
    pub session_dir: Option<PathBuf>,
    pub cancel: CancellationToken,
    pub state: Arc<AtomicU8>,
    pub auth_token: String,
    /// Environment variables injected into PTY sessions (JUST_AGENT_ID, JUST_AGENT_AUTH_TOKEN, etc.).
    /// Preserved across reactivation so the agent retains its identity.
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
}

impl AppState {
    pub fn new(operator_token: String) -> Self {
        Self {
            registry: RwLock::new(AgentRegistry::new()),
            skill_promote_store: Mutex::new(SkillPromoteStore::new()),
            skill_write_lock: Mutex::new(()),
            shutdown: CancellationToken::new(),
            operator_token,
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

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&AgentId, &AgentEntry)> {
        self.agents.iter()
    }

    pub fn values(&self) -> impl Iterator<Item = &AgentEntry> {
        self.agents.values()
    }

    pub fn get_agent_id_by_token(&self, token: &str) -> Option<&AgentId> {
        self.token_index.get(token)
    }

    // -- write helpers --

    /// Insert agent, register token, update supervisor's subagent_ids.
    pub fn register(&mut self, id: AgentId, auth_token: String, entry: AgentEntry) {
        // Eagerly link: if the supervisor is already registered, update its
        // subagent_ids now. This always succeeds in the create path
        // (supervisor is validated before we get here) and in the restore
        // path (top-down BFS guarantees supervisor is registered first).
        if let Some(ref supervisor_id) = entry.agent.config.created_by
            && let Some(supervisor) = self.agents.get_mut(supervisor_id)
        {
            supervisor.subagent_ids.push(id.clone());
        }
        self.token_index.insert(auth_token, id.clone());
        self.agents.insert(id, entry);
    }

    /// Remove agent, unregister token, update supervisor's subagent_ids.
    pub fn unregister(&mut self, id: &AgentId) -> Option<AgentEntry> {
        let entry = self.agents.remove(id)?;
        self.token_index.remove(&entry.agent.auth_token);
        if let Some(ref supervisor_id) = entry.agent.config.created_by
            && let Some(supervisor) = self.agents.get_mut(supervisor_id)
        {
            supervisor.subagent_ids.retain(|sid| sid != id);
        }
        Some(entry)
    }

    // -- authorization helpers --

    /// Walk the `created_by` chain from `start_id` upward with cycle detection.
    pub fn walk_supervisor_chain(
        &self,
        start_id: &AgentId,
    ) -> Result<Vec<&AgentEntry>, (StatusCode, String)> {
        let mut visited = HashSet::new();
        let mut current_id = start_id.clone();
        let mut chain = Vec::new();
        loop {
            if !visited.insert(current_id.clone()) {
                return Err((StatusCode::FORBIDDEN, "circular supervisor chain".into()));
            }
            let entry = self
                .get(&current_id)
                .ok_or((StatusCode::FORBIDDEN, "broken supervisor chain".into()))?;
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
    ) -> Result<&AgentEntry, (StatusCode, String)> {
        let supervisor = self.get(supervisor_id).ok_or((
            StatusCode::NOT_FOUND,
            format!("supervisor agent {supervisor_id} not found"),
        ))?;
        match identity {
            crate::auth::Identity::Operator => Ok(supervisor),
            crate::auth::Identity::Agent { id } if id == supervisor_id => Ok(supervisor),
            _ => Err((
                StatusCode::FORBIDDEN,
                "invalid auth token for supervisor agent".into(),
            )),
        }
    }

    /// Caller must be the operator or a superior of the target agent.
    pub fn require_superior(
        &self,
        identity: &crate::auth::Identity,
        target_id: &AgentId,
    ) -> Result<(), (StatusCode, String)> {
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
        Err((
            StatusCode::FORBIDDEN,
            "not authorized to manage this agent".into(),
        ))
    }

    /// Caller must be the operator or a root agent (created_by is None).
    /// Used for promote-request review operations.
    pub fn require_root_or_operator(
        &self,
        identity: &crate::auth::Identity,
    ) -> Result<(), (StatusCode, String)> {
        match identity {
            crate::auth::Identity::Operator => Ok(()),
            crate::auth::Identity::Agent { id } => {
                let entry = self
                    .get(id)
                    .ok_or((StatusCode::FORBIDDEN, "unknown agent".into()))?;
                if entry.agent.config.created_by.is_none() {
                    Ok(())
                } else {
                    Err((
                        StatusCode::FORBIDDEN,
                        "only root agents or operators can review promote requests".into(),
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
    ) -> Result<(), (StatusCode, String)> {
        match identity {
            crate::auth::Identity::Operator => Ok(()),
            crate::auth::Identity::Agent { id } if id == target_id => Ok(()),
            _ => Err((
                StatusCode::FORBIDDEN,
                "only the agent itself or operator can submit promote requests".into(),
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
    use just_agent_runtime::config::{PermissionProfile, default_tool_policy};
    use just_agent_runtime::retry::RetryPolicy;

    fn make_entry(created_by: Option<AgentId>, auth_token: String) -> AgentEntry {
        let (prompt_tx, _) = mpsc::channel(1);
        let (events_tx, _) = broadcast::channel(1);
        let config = AgentConfig {
            prompt: None,
            system_prompt: String::new(),
            max_tool_rounds: 1,
            workspace_root: PathBuf::from("/tmp"),
            context_window_tokens: 128_000,
            output_reserve_tokens: 8_192,
            summary_max_tokens: 1_200,
            tool_timeout_secs: 120,
            skills: vec![],
            retry_policy: RetryPolicy::default(),
            pinned_budget_ratio: 0.25,
            context_thresholds: vec![50, 80],
            agent_id: None,
            created_by,
            permissions: PermissionProfile::new(PathBuf::from("/tmp")),
        };
        AgentEntry {
            agent: Agent {
                prompt_tx,
                events_tx,
                approvals: Arc::new(Mutex::new(ApprovalStore::new())),
                config,
                agent_handle: tokio::spawn(async {}),
                bridge_handle: tokio::spawn(async {}),
                store: Arc::new(Mutex::new(ContextStore::new())),
                session_dir: None,
                cancel: CancellationToken::new(),
                state: Arc::new(AtomicU8::new(AgentState::IDLE)),
                auth_token,
                env: HashMap::new(),
                tool_policy: Arc::new(std::sync::RwLock::new(default_tool_policy())),
            },
            subagent_ids: vec![],
        }
    }

    fn add_root(registry: &mut AgentRegistry, id: &AgentId) {
        let token = format!("tok-{id}");
        registry.register(id.clone(), token, make_entry(None, format!("agent-{id}")));
    }

    fn add_sub(registry: &mut AgentRegistry, id: &AgentId, supervisor: &AgentId) {
        let token = format!("tok-{id}");
        registry.register(
            id.clone(),
            token,
            make_entry(Some(supervisor.clone()), format!("agent-{id}")),
        );
    }

    // -- Registry consistency: agents + token_index + subagent_ids stay in sync --

    #[tokio::test]
    async fn register_unregister_syncs_token_index() {
        let mut reg = AgentRegistry::new();
        let id = AgentId::random();
        // In production, the registry token and agent.auth_token are always identical.
        let token = "test-token";
        reg.register(id.clone(), token.into(), make_entry(None, token.into()));
        assert!(reg.contains_key(&id));
        assert_eq!(reg.get_agent_id_by_token(token), Some(&id));

        let removed = reg.unregister(&id).unwrap();
        assert_eq!(removed.agent.auth_token, token);
        assert!(!reg.contains_key(&id));
        assert!(reg.get_agent_id_by_token(token).is_none());
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
        reg.register(
            a.clone(),
            "ta".into(),
            make_entry(Some(b.clone()), "aa".into()),
        );
        reg.register(b, "tb".into(), make_entry(Some(a.clone()), "ab".into()));
        match reg.walk_supervisor_chain(&a) {
            Err((status, msg)) => {
                assert_eq!(status, StatusCode::FORBIDDEN);
                assert!(msg.contains("circular"));
            }
            Ok(_) => panic!("expected cycle error"),
        }
    }

    #[tokio::test]
    async fn walk_chain_rejects_broken_link() {
        let mut reg = AgentRegistry::new();
        let a = AgentId::random();
        let ghost = AgentId::random();
        reg.register(a.clone(), "t".into(), make_entry(Some(ghost), "a".into()));
        match reg.walk_supervisor_chain(&a) {
            Err((status, _)) => assert_eq!(status, StatusCode::FORBIDDEN),
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
            Err((status, _)) => assert_eq!(status, StatusCode::FORBIDDEN),
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
            Err((status, _)) => assert_eq!(status, StatusCode::FORBIDDEN),
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
            Err((status, _)) => assert_eq!(status, StatusCode::FORBIDDEN),
            Ok(_) => panic!("expected FORBIDDEN"),
        }
    }

    #[tokio::test]
    async fn supervisor_returns_not_found_for_missing() {
        let reg = AgentRegistry::new();
        let ghost = AgentId::random();
        match reg.require_supervisor(&Identity::Operator, &ghost) {
            Err((status, _)) => assert_eq!(status, StatusCode::NOT_FOUND),
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
            Err((status, msg)) => {
                assert_eq!(status, StatusCode::FORBIDDEN);
                assert!(msg.contains("root agents"));
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
            Err((status, _)) => assert_eq!(status, StatusCode::FORBIDDEN),
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
}
