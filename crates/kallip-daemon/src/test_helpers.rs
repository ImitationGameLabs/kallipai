//! Shared test helpers for daemon tests.
//!
//! This module is only compiled in test builds and provides utilities for
//! constructing agent entries and registries used across `state.rs`,
//! `bridge.rs`, and other test modules.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use kallip_common::agentid::AgentId;
use kallip_common::policy::{ExecPolicy, PolicyPreset};
use kallip_common::protocol::AgentState;
use kallip_runtime::approval::ApprovalStore;
use kallip_runtime::config::{AgentConfig, PermissionProfile};
use kallip_runtime::context::ContextStore;
use kallip_runtime::retry::RetryPolicy;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::state::{
    Agent, AgentEntry, AgentIdentity, AgentRegistry, AppState, FaultedEntry, RegistryEntry,
    SharedState,
};
use kallip_common::authtoken::TokenHash;

/// Construct a full `AgentEntry` with real channels, the default preset, and an
/// empty exec-policy.
pub fn make_entry(created_by: Option<AgentId>, auth_token: String) -> AgentEntry {
    make_entry_inner(
        created_by,
        auth_token,
        PolicyPreset::Default,
        ExecPolicy::default(),
    )
    .0
}

/// Like [`make_entry`], but returns the `prompt_rx` for capturing notifications.
pub fn make_entry_with_rx(
    created_by: Option<AgentId>,
    auth_token: String,
) -> (AgentEntry, mpsc::Receiver<String>) {
    make_entry_inner(
        created_by,
        auth_token,
        PolicyPreset::Default,
        ExecPolicy::default(),
    )
}

/// Like [`make_entry`], but installs a custom preset and exec-policy on the agent.
pub fn make_entry_with_policy(
    created_by: Option<AgentId>,
    auth_token: String,
    preset: PolicyPreset,
    exec_policy: ExecPolicy,
) -> AgentEntry {
    make_entry_inner(created_by, auth_token, preset, exec_policy).0
}

/// Like [`make_entry_with_policy`], but returns the `prompt_rx`.
pub fn make_entry_with_policy_rx(
    created_by: Option<AgentId>,
    auth_token: String,
    preset: PolicyPreset,
    exec_policy: ExecPolicy,
) -> (AgentEntry, mpsc::Receiver<String>) {
    make_entry_inner(created_by, auth_token, preset, exec_policy)
}

fn make_entry_inner(
    created_by: Option<AgentId>,
    auth_token: String,
    preset: PolicyPreset,
    exec_policy: ExecPolicy,
) -> (AgentEntry, mpsc::Receiver<String>) {
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
        permissions_class: Default::default(),
        role: String::new(),
        description: String::new(),
    };
    let entry = AgentEntry {
        identity: AgentIdentity {
            config,
            agent_dir: None,
        },
        agent: Agent {
            prompt_tx,
            events_tx,
            approvals: Arc::new(Mutex::new(ApprovalStore::new())),
            agent_handle: tokio::spawn(async {}),
            bridge_handle: tokio::spawn(async {}),
            store: Arc::new(Mutex::new(ContextStore::new())),
            cancel: CancellationToken::new(),
            round_cancel: Arc::new(std::sync::Mutex::new(None)),
            notify: Arc::new(tokio::sync::Notify::new()),
            state: Arc::new(AtomicU8::new(AgentState::IDLE)),
            activity: Arc::new(std::sync::Mutex::new(String::new())),
            auth_token_hash: TokenHash::of(&auth_token),
            env: std::collections::HashMap::new(),
            preset,
            exec_policy: Arc::new(std::sync::RwLock::new(exec_policy)),
        },
        subagent_ids: vec![],
    };
    (entry, prompt_rx)
}

/// Construct a faulted entry (no running task) with the given reason.
pub fn make_faulted_entry(created_by: Option<AgentId>, reason: &str) -> FaultedEntry {
    let config = AgentConfig {
        created_by,
        workspace_root: PathBuf::from("/tmp"),
        ..AgentConfig::default()
    };
    FaultedEntry {
        identity: AgentIdentity {
            config,
            agent_dir: None,
        },
        subagent_ids: vec![],
        reason: reason.to_string(),
    }
}

/// Register a root agent (no `created_by`).
pub fn add_root(registry: &mut AgentRegistry, id: &AgentId) {
    registry.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, format!("agent-{id}"))),
    );
}

/// Register a sub-agent under a supervisor.
pub fn add_sub(registry: &mut AgentRegistry, id: &AgentId, supervisor: &AgentId) {
    registry.register(
        id.clone(),
        RegistryEntry::Live(make_entry(Some(supervisor.clone()), format!("agent-{id}"))),
    );
}

/// Register a root agent with a custom preset and exec-policy.
pub fn add_root_with_policy(
    registry: &mut AgentRegistry,
    id: &AgentId,
    preset: PolicyPreset,
    exec_policy: ExecPolicy,
) {
    registry.register(
        id.clone(),
        RegistryEntry::Live(make_entry_with_policy(
            None,
            format!("agent-{id}"),
            preset,
            exec_policy,
        )),
    );
}

/// Register a faulted root agent with the given reason.
pub fn add_faulted_root(registry: &mut AgentRegistry, id: &AgentId, reason: &str) {
    registry.register(
        id.clone(),
        RegistryEntry::Faulted(make_faulted_entry(None, reason)),
    );
}

/// Register a faulted sub-agent under a supervisor.
pub fn add_faulted_sub(
    registry: &mut AgentRegistry,
    id: &AgentId,
    supervisor: &AgentId,
    reason: &str,
) {
    registry.register(
        id.clone(),
        RegistryEntry::Faulted(make_faulted_entry(Some(supervisor.clone()), reason)),
    );
}

/// Enqueue and commit an approval on the target agent, return the approval ID.
pub async fn enqueue_committed_approval(
    registry: &tokio::sync::RwLockReadGuard<'_, AgentRegistry>,
    agent_id: &AgentId,
    tool_name: &str,
    arguments: &str,
) -> String {
    let entry = registry.get(agent_id).expect("agent exists");
    let live = entry.as_live().expect("agent is live");
    let mut store = live.agent.approvals.lock().await;
    let id = store.enqueue(tool_name, arguments, None);
    store.commit(&id, "test commit").expect("commit");
    id
}

/// Minimal single-profile registry for tests that need an `AppState` but won't spawn real
/// agents. No declared window (env-path semantics).
pub fn make_profile_registry() -> Arc<kallip_runtime::profile::ProfileRegistry> {
    use just_llm_client::family;
    use kallip_runtime::profile::{Endpoint, Profile, ProfileConfig, ProfileRegistry, Tier};
    use std::collections::HashMap;
    let mut endpoints = HashMap::new();
    endpoints.insert(
        "test".into(),
        Endpoint {
            id: "test".into(),
            family: family::DEEPSEEK.into(),
            api_key: "test".into(),
            base_url: None,
        },
    );
    let cfg = ProfileConfig {
        tiers: vec![Tier {
            profiles: vec![Profile {
                id: "test".into(),
                endpoint: "test".into(),
                model: "test".into(),
                max_context_window: 128_000,
            }],
        }],
        endpoints,
    };
    let source = crate::backend::build_backends(
        &cfg,
        just_llm_client::client::BackendFactory::new(),
        crate::backend::DEFAULT_USER_AGENT,
    )
    .expect("test backends build");
    Arc::new(ProfileRegistry::new(cfg.tiers, source).expect("valid test registry"))
}

/// Create a fresh `SharedState` (default preset) for testing. The operator token
/// plaintext is `"op-token"` (hashed into `AppState`); tests present it as a
/// bearer token.
pub fn make_state() -> SharedState {
    make_state_with_preset(PolicyPreset::Default)
}

/// Like [`make_state`], but with a custom daemon-global preset.
pub fn make_state_with_preset(preset: PolicyPreset) -> SharedState {
    Arc::new(AppState::new_with_preset(
        TokenHash::of("op-token"),
        make_profile_registry(),
        preset,
    ))
}
