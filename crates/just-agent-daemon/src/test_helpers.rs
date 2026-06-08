//! Shared test helpers for daemon tests.
//!
//! This module is only compiled in test builds and provides utilities for
//! constructing agent entries, registries, and policies used across
//! `state.rs`, `bridge.rs`, and other test modules.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use just_agent_common::agentid::AgentId;
use just_agent_common::policy::{PolicyDecision, ToolPolicy};
use just_agent_common::protocol::AgentState;
use just_agent_runtime::approval::ApprovalStore;
use just_agent_runtime::config::{AgentConfig, PermissionProfile, default_tool_policy};
use just_agent_runtime::context::ContextStore;
use just_agent_runtime::retry::RetryPolicy;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::state::{Agent, AgentEntry, AgentRegistry, AppState, SharedState};

/// Construct a full `AgentEntry` with real channels and default policy.
pub fn make_entry(created_by: Option<AgentId>, auth_token: String) -> AgentEntry {
    make_entry_with_rx(created_by, auth_token).0
}

/// Like `make_entry`, but returns the `prompt_rx` for capturing notifications.
pub fn make_entry_with_rx(
    created_by: Option<AgentId>,
    auth_token: String,
) -> (
    AgentEntry,
    mpsc::Receiver<just_agent_common::command::UserInput>,
) {
    let (prompt_tx, prompt_rx) = mpsc::channel(16);
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
        token_budget_warnings: vec![80, 95],
        agent_id: None,
        created_by,
        permissions: PermissionProfile::new(PathBuf::from("/tmp")),
    };
    let entry = AgentEntry {
        agent: Agent {
            prompt_tx,
            events_tx,
            approvals: Arc::new(Mutex::new(ApprovalStore::new())),
            config,
            agent_handle: tokio::spawn(async {}),
            bridge_handle: tokio::spawn(async {}),
            store: Arc::new(Mutex::new(ContextStore::new())),
            agent_dir: None,
            cancel: CancellationToken::new(),
            state: Arc::new(AtomicU8::new(AgentState::IDLE)),
            auth_token,
            env: std::collections::HashMap::new(),
            tool_policy: Arc::new(std::sync::RwLock::new(default_tool_policy())),
        },
        subagent_ids: vec![],
    };
    (entry, prompt_rx)
}

/// Construct an entry with a custom `ToolPolicy`.
pub fn make_entry_with_policy(
    created_by: Option<AgentId>,
    auth_token: String,
    policy: ToolPolicy,
) -> AgentEntry {
    let mut entry = make_entry(created_by, auth_token);
    entry.agent.tool_policy = Arc::new(std::sync::RwLock::new(policy));
    entry
}

/// Like `make_entry_with_policy`, but returns the `prompt_rx`.
pub fn make_entry_with_policy_rx(
    created_by: Option<AgentId>,
    auth_token: String,
    policy: ToolPolicy,
) -> (
    AgentEntry,
    mpsc::Receiver<just_agent_common::command::UserInput>,
) {
    let (mut entry, rx) = make_entry_with_rx(created_by, auth_token);
    entry.agent.tool_policy = Arc::new(std::sync::RwLock::new(policy));
    (entry, rx)
}

/// Register a root agent (no `created_by`).
pub fn add_root(registry: &mut AgentRegistry, id: &AgentId) {
    let token = format!("tok-{id}");
    registry.register(id.clone(), token, make_entry(None, format!("agent-{id}")));
}

/// Register a sub-agent under a supervisor.
pub fn add_sub(registry: &mut AgentRegistry, id: &AgentId, supervisor: &AgentId) {
    let token = format!("tok-{id}");
    registry.register(
        id.clone(),
        token,
        make_entry(Some(supervisor.clone()), format!("agent-{id}")),
    );
}

/// Register a root agent with a custom policy.
pub fn add_root_with_policy(registry: &mut AgentRegistry, id: &AgentId, policy: ToolPolicy) {
    let token = format!("tok-{id}");
    registry.register(
        id.clone(),
        token,
        make_entry_with_policy(None, format!("agent-{id}"), policy),
    );
}

/// Build a `ToolPolicy` that allows exactly one tool, defaults to `Ask`.
pub fn policy_allow_tool(tool: &str) -> ToolPolicy {
    let mut tools = BTreeMap::new();
    tools.insert(tool.to_string(), PolicyDecision::Allow);
    ToolPolicy {
        default: PolicyDecision::Ask,
        tools,
    }
}

/// Build a `ToolPolicy` that sets one tool to the given decision, defaults to `Ask`.
pub fn policy_for_tool(tool: &str, decision: PolicyDecision) -> ToolPolicy {
    let mut tools = BTreeMap::new();
    tools.insert(tool.to_string(), decision);
    ToolPolicy {
        default: PolicyDecision::Ask,
        tools,
    }
}

/// Enqueue and commit an approval on the target agent, return the approval ID.
pub async fn enqueue_committed_approval(
    registry: &tokio::sync::RwLockReadGuard<'_, AgentRegistry>,
    agent_id: &AgentId,
    tool_name: &str,
) -> String {
    let entry = registry.get(agent_id).expect("agent exists");
    let mut store = entry.agent.approvals.lock().await;
    let id = store.enqueue(tool_name, "{}");
    store.commit(&id, "test commit").expect("commit");
    id
}

/// Create a fresh `SharedState` for testing.
pub fn make_state() -> SharedState {
    Arc::new(AppState::new("op-token".into()))
}
