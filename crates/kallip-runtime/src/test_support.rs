//! Shared test fixtures for the runtime crate's inline test modules.
//!
//! The concern split places tests that exercise the same `AgentContext` construction across
//! several modules (`runner`, `context::estimate`, `profile::registry`). This module factors out
//! the shared fixtures so each is written once. `#[cfg(test)]`-gated — never compiled into a build.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Context;
use just_llm_client::types::generation::{GenerationRequest, Message, ToolCall, Usage};

/// The test-facing message element type. Re-pointed to the generation face by the
/// just-agent-libs migration; test signatures never name the upstream type.
pub(crate) type TurnMessage = Message;
use just_llm_client::{LlmBackend, ToolDispatcher};
use tokio_util::sync::CancellationToken;

use crate::agent_task::AgentContext;
use crate::approval::ApprovalStore;
use crate::config::{AgentConfig, PermissionProfile};
use crate::context::{ContextStore, ContextSummarizer};
use crate::failover::{FailoverState, ProfileSnapshot};
use crate::policy::{AgentPolicy, AuthorizedToolExecutor};
use crate::profile::{BackendSource, Profile, ProfileRegistry, ProfileSet};
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

/// Test-only [`BackendSource`]: provider id → backend. A missing provider yields `Err`, used to
/// simulate an unbuildable failover candidate (the skip path).
pub(crate) struct MapSource(pub(crate) HashMap<String, Arc<dyn LlmBackend>>);
impl BackendSource for MapSource {
    fn get(&self, provider_id: &str) -> anyhow::Result<Arc<dyn LlmBackend>> {
        self.0
            .get(provider_id)
            .cloned()
            .with_context(|| format!("unknown provider '{provider_id}'"))
    }
}

/// A minimal [`Profile`] with `{id}`-derived model name.
pub(crate) fn profile(id: &str, provider: &str, window: usize) -> Profile {
    Profile {
        id: id.into(),
        endpoint: provider.into(),
        model: format!("{id}-model"),
        max_context_window: window,
        store: None,
        effort: None,
    }
}

/// Minimal valid `AgentConfig` for tests (mirrors `config.rs` fixtures).
pub(crate) fn test_config() -> AgentConfig {
    AgentConfig {
        prompt: None,
        system_prompt: String::new(),
        max_tool_rounds: 1,
        max_heartbeat_rounds: 3,
        max_transient_retries: 3,
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
        profile_set: None,
        permissions_class: Default::default(),
        role: String::new(),
        description: String::new(),
        delegation_mode: crate::config::DelegationMode::CarveOut,
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
    let set = ProfileSet {
        name: "default".into(),
        description: None,
        profiles,
    };
    let sets = BTreeMap::from([("default".to_string(), set.clone())]);
    let registry = Arc::new(ProfileRegistry::new(sets, source).unwrap());
    let snapshot = Arc::new(std::sync::Mutex::new(ProfileSnapshot::default()));
    let failover = FailoverState::new(set, registry, Some("sys".into()), snapshot);
    let client = failover
        .build_client(failover.current_profile())
        .expect("active profile is buildable");
    let store = Arc::new(tokio::sync::Mutex::new(ContextStore::new()));
    let approvals = Arc::new(tokio::sync::Mutex::new(ApprovalStore::new()));
    let executor = AuthorizedToolExecutor::new(
        ToolDispatcher::new(),
        AgentPolicy::new(
            Arc::new(RwLock::new(kallip_common::policy::ExecPolicy::default())),
            kallip_common::policy::PolicyPreset::Default,
            Arc::new(Vec::new()),
        ),
        approvals.clone(),
        None,
    );
    {
        let mut guard = store.lock().await;
        guard.set_tool_definitions(executor.tool_definitions());
        guard.set_pinned_budget(config.pinned_budget());
    }
    AgentContext {
        conversation: client.conversation(),
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
        retry_notify: Arc::new(tokio::sync::Notify::new()),
        retry_at: Arc::new(std::sync::Mutex::new(None)),
        transient_fails: 0,
        lifecycle: std::sync::Mutex::new(crate::lifecycle::LifecycleState::Idle),
        wait_until: Arc::new(std::sync::Mutex::new(None)),
        wait_notify: Arc::new(tokio::sync::Notify::new()),
        wait_armed_secs: 0,
        token_budget: TokenBudget::new(1_000_000, 0),
        pending_profile_reset: Arc::new(std::sync::Mutex::new(None)),
        message_puller: None,
        persist_failures: Default::default(),
    }
}

/// A `MapSource` of network-free DeepSeek backends for `endpoints` (unit-test convenience).
pub(crate) fn map_source(providers: &[&str]) -> Arc<dyn BackendSource> {
    let mut map = HashMap::new();
    for ep in providers {
        map.insert((*ep).into(), ds_backend());
    }
    Arc::new(MapSource(map))
}

/// Convenience: build an `AgentContext` over `profiles` whose endpoints are `endpoints`, with the
/// default retry policy.
pub(crate) async fn make_ctx(profiles: Vec<Profile>, providers: &[&str]) -> AgentContext {
    ctx_from_source(profiles, map_source(providers), RetryPolicy::default()).await
}

/// A [`Usage`] with only `prompt_tokens` set (and derived `total_tokens`).
pub(crate) fn usage(prompt_tokens: u32) -> Usage {
    usage_with_completion(prompt_tokens, 0)
}

// --- chat-type construction funnel ----------------------------------------------
// Every test-side chat type construction goes through these helpers, so
// body changes touch no test call site. Keep call sites free of chat-face
// type names: helpers own the types, callers own the intent.

/// A user turn carrying `text`.
pub(crate) fn user_msg(text: impl Into<String>) -> Message {
    Message::user(text)
}

/// An assistant turn carrying `text`.
pub(crate) fn assistant_msg(text: impl Into<String>) -> Message {
    Message::assistant(text)
}

/// A tool result delivering `content` for call `tool_call_id`.
pub(crate) fn tool_result_msg(
    content: impl Into<String>,
    tool_call_id: impl Into<String>,
) -> Message {
    Message::tool(content, tool_call_id)
}

/// An assistant turn declaring a single function call with explicit
/// `arguments` JSON text.
pub(crate) fn tool_call_msg(id: &str, name: &str, arguments: &str) -> Message {
    tool_calls_message(vec![ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }])
}

/// An assistant turn declaring `(id, name)` function calls, each with
/// empty (`{}`) arguments.
pub(crate) fn tool_calls_msg(calls: &[(&str, &str)]) -> Message {
    tool_calls_message(
        calls
            .iter()
            .map(|(id, name)| ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                arguments: "{}".into(),
            })
            .collect(),
    )
}

fn tool_calls_message(tool_calls: Vec<ToolCall>) -> Message {
    Message::assistant_tool_calls(None, tool_calls, None)
}

/// A completion request for `model` over `messages`.
pub(crate) fn request(model: &str, messages: Vec<Message>) -> GenerationRequest {
    GenerationRequest::new(model, messages)
}

/// A [`Usage`] with both token counts set (and derived `total_tokens`).
pub(crate) fn usage_with_completion(prompt_tokens: u32, completion_tokens: u32) -> Usage {
    Usage {
        prompt_tokens,
        completion_tokens,
        cache_read_tokens: None,
        cache_write_tokens: None,
        total_tokens: prompt_tokens + completion_tokens,
        completion_tokens_details: None,
    }
}

/// Minimal stateful backend modeled on the upstream conversation test mock: it records
/// the wire requests it receives and answers `stream_generate` from a queue of canned
/// event lists (plus an optional error queue), so tests can assert exactly what reached
/// the provider boundary. Never touches the network.
pub(crate) struct RecordingBackend {
    requests: Mutex<Vec<GenerationRequest>>,
    failures: Mutex<VecDeque<just_llm_client::BackendError>>,
    streams: Mutex<VecDeque<Vec<just_llm_client::types::generation::GenerationEvent>>>,
}

impl RecordingBackend {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            failures: Mutex::new(VecDeque::new()),
            streams: Mutex::new(VecDeque::new()),
        })
    }

    /// One text delta followed by `End` carrying `response_id` — the minimal stream a
    /// successful turn needs (the capture requires at least one accumulated event).
    pub(crate) fn queue_stream(&self, response_id: &str) {
        self.streams.lock().unwrap().push_back(vec![
            just_llm_client::types::generation::GenerationEvent::Text {
                delta: "a".to_owned(),
            },
            just_llm_client::types::generation::GenerationEvent::End {
                finish_reason: None,
                response_id: Some(response_id.to_owned()),
            },
        ]);
    }

    /// Queue an error that surfaces from `stream_generate` itself (pre-stream).
    pub(crate) fn queue_error(&self, error: just_llm_client::BackendError) {
        self.failures.lock().unwrap().push_back(error);
    }

    pub(crate) fn take_requests(&self) -> Vec<GenerationRequest> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}

impl just_llm_client::Identifiable for RecordingBackend {
    fn family(&self) -> &'static str {
        "recording"
    }
}

impl just_llm_client::CapabilityNegotiation for RecordingBackend {
    fn supports_stateful_conversation(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl LlmBackend for RecordingBackend {
    fn prepare(
        &self,
        _request: GenerationRequest,
    ) -> Result<reqwest::Request, just_llm_client::BackendError> {
        unimplemented!("tests drive stream_generate directly")
    }

    fn prepare_streaming(
        &self,
        _request: GenerationRequest,
    ) -> Result<reqwest::Request, just_llm_client::BackendError> {
        unimplemented!("tests drive stream_generate directly")
    }

    async fn send(
        &self,
        _prepared: reqwest::Request,
    ) -> Result<reqwest::Response, just_llm_client::BackendError> {
        unimplemented!("tests drive stream_generate directly")
    }

    async fn parse(
        &self,
        _response: reqwest::Response,
    ) -> Result<just_llm_client::types::generation::GenerationResponse, just_llm_client::BackendError>
    {
        unimplemented!("tests drive stream_generate directly")
    }

    async fn parse_streaming(
        &self,
        _response: reqwest::Response,
    ) -> Result<just_llm_client::GenerationStream, just_llm_client::BackendError> {
        unimplemented!("tests drive stream_generate directly")
    }

    async fn stream_generate(
        &self,
        request: GenerationRequest,
    ) -> Result<just_llm_client::GenerationStream, just_llm_client::BackendError> {
        self.requests.lock().unwrap().push(request);
        if let Some(error) = self.failures.lock().unwrap().pop_front() {
            return Err(error);
        }
        let events = self
            .streams
            .lock()
            .unwrap()
            .pop_front()
            .expect("no stream queued for recording backend");
        let stream = futures_util::stream::iter(
            events
                .into_iter()
                .map(Ok::<_, just_llm_client::TransportError>),
        );
        Ok(just_llm_client::GenerationStream::new(Box::pin(stream)))
    }

    fn render_messages(
        &self,
        _messages: &[Message],
    ) -> Result<String, just_llm_client::BackendError> {
        unimplemented!("tests drive stream_generate directly")
    }

    fn render_tools(
        &self,
        _tools: &[just_llm_client::types::generation::ToolDefinition],
    ) -> Result<String, just_llm_client::BackendError> {
        unimplemented!("tests drive stream_generate directly")
    }

    fn family() -> &'static str {
        "recording"
    }

    fn new(
        _http: reqwest::ClientBuilder,
        _api_key: &str,
        _base_url: Option<&str>,
    ) -> Result<Arc<dyn LlmBackend>, just_llm_client::BackendConstructError> {
        unimplemented!("tests drive stream_generate directly")
    }
}
