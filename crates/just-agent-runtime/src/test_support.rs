//! Shared test fixtures for the runtime crate's inline test modules.
//!
//! The concern split places tests that exercise the same `AgentContext` construction across
//! several modules (`runner`, `context::estimate`, `profile::registry`). This module factors out
//! the shared fixtures so each is written once. `#[cfg(test)]`-gated — never compiled into a build.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Context;
use just_llm_client::types::chat::Usage;
use just_llm_client::{LlmBackend, ToolDispatcher};
use tokio_util::sync::CancellationToken;

use crate::agent_task::AgentContext;
use crate::approval::ApprovalStore;
use crate::config::{AgentConfig, PermissionProfile, default_tool_policy};
use crate::context::{ContextStore, ContextSummarizer};
use crate::failover::FailoverState;
use crate::policy::{AgentPolicy, AuthorizedToolExecutor};
use crate::profile::{BackendSource, Profile, ProfileRegistry, Tier};
use crate::retry::RetryPolicy;
use crate::token_budget::TokenBudget;

/// Network-free DeepSeek backend (construction touches no network).
pub(crate) fn ds_backend() -> Arc<dyn LlmBackend> {
    just_llm_client::provider::DeepSeekBackend::new(
        reqwest::Client::builder().use_rustls_tls(),
        "fake",
        None,
    )
    .expect("deepseek backend constructs without network")
}

/// Test-only [`BackendSource`]: endpoint id → backend. A missing endpoint yields `Err`, used to
/// simulate an unbuildable failover candidate (the skip path).
pub(crate) struct MapSource(pub(crate) HashMap<String, Arc<dyn LlmBackend>>);
impl BackendSource for MapSource {
    fn get(&self, endpoint_id: &str) -> anyhow::Result<Arc<dyn LlmBackend>> {
        self.0
            .get(endpoint_id)
            .cloned()
            .with_context(|| format!("unknown endpoint '{endpoint_id}'"))
    }
}

/// A minimal [`Profile`] with `{id}`-derived model name.
pub(crate) fn profile(id: &str, endpoint: &str, window: usize) -> Profile {
    Profile {
        id: id.into(),
        endpoint: endpoint.into(),
        model: format!("{id}-model"),
        max_context_window: window,
    }
}

/// Minimal valid `AgentConfig` for tests (mirrors `config.rs` fixtures).
pub(crate) fn test_config() -> AgentConfig {
    AgentConfig {
        prompt: None,
        system_prompt: String::new(),
        max_tool_rounds: 1,
        workspace_root: PathBuf::from("/tmp"),
        context_window_tokens: 500_000,
        output_reserve_tokens: 8_192,
        summary_max_tokens: 1_200,
        tool_timeout_secs: 120,
        skills: vec![],
        retry_policy: RetryPolicy::default(),
        pinned_budget_ratio: 0.25,
        context_thresholds: vec![50, 80],
        token_budget_warnings: vec![80, 95],
        agent_id: None,
        created_by: None,
        permissions: PermissionProfile::new(PathBuf::from("/tmp")),
        permissions_class: Default::default(),
        role: String::new(),
        description: String::new(),
    }
}

/// Build an `AgentContext` over `profiles` backed by `source`, with `retry_policy`. The store
/// starts empty (seed a user turn for `run_agent_rounds` tests); `summarize_and_evict` no-ops
/// on it. `profiles[0]` must be buildable (its client is constructed here).
pub(crate) async fn ctx_from_source(
    profiles: Vec<Profile>,
    source: Arc<dyn BackendSource>,
    retry_policy: RetryPolicy,
) -> AgentContext {
    let mut config = test_config();
    config.retry_policy = retry_policy;
    let tier = Tier { profiles };
    let registry = Arc::new(ProfileRegistry::new(vec![tier.clone()], source).unwrap());
    let failover = FailoverState::new(tier, registry, Some("sys".into()));
    let client = failover
        .build_client(failover.current_profile())
        .expect("active profile is buildable");
    let store = Arc::new(tokio::sync::Mutex::new(ContextStore::new()));
    let approvals = Arc::new(tokio::sync::Mutex::new(ApprovalStore::new()));
    let executor = AuthorizedToolExecutor::new(
        ToolDispatcher::new(),
        AgentPolicy::new(
            Arc::new(RwLock::new(default_tool_policy())),
            Arc::new(RwLock::new(just_agent_common::policy::ExecPolicy::default())),
        ),
        approvals.clone(),
    );
    {
        let mut guard = store.lock().await;
        guard.set_tool_definitions(executor.tool_definitions());
        guard.set_pinned_budget(config.pinned_budget());
    }
    AgentContext {
        client,
        failover,
        store,
        approvals,
        executor,
        summarizer: ContextSummarizer::new(config.summary_max_tokens),
        config,
        agent_dir: None,
        history: None,
        cancel: CancellationToken::new(),
        round_cancel: Arc::new(std::sync::Mutex::new(None)),
        notify: Arc::new(tokio::sync::Notify::new()),
        token_budget: TokenBudget::new(1_000_000, 0),
    }
}

/// A `MapSource` of network-free DeepSeek backends for `endpoints` (unit-test convenience).
pub(crate) fn map_source(endpoints: &[&str]) -> Arc<dyn BackendSource> {
    let mut map = HashMap::new();
    for ep in endpoints {
        map.insert((*ep).into(), ds_backend());
    }
    Arc::new(MapSource(map))
}

/// Convenience: build an `AgentContext` over `profiles` whose endpoints are `endpoints`, with the
/// default retry policy.
pub(crate) async fn make_ctx(profiles: Vec<Profile>, endpoints: &[&str]) -> AgentContext {
    ctx_from_source(profiles, map_source(endpoints), RetryPolicy::default()).await
}

/// A [`Usage`] with only `prompt_tokens` set (and derived `total_tokens`).
pub(crate) fn usage(prompt_tokens: u32) -> Usage {
    Usage {
        prompt_tokens,
        completion_tokens: 0,
        prompt_cache_hit_tokens: None,
        prompt_cache_miss_tokens: None,
        total_tokens: prompt_tokens,
        completion_tokens_details: None,
    }
}
