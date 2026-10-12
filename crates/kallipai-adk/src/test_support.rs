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
use kallipai_common::facts::{Block, BlockedFact, EndpointHealth, FactView, is_active};
use tokio_util::sync::CancellationToken;

use crate::agent_task::AgentContext;
use crate::approval::ApprovalStore;
use crate::config::{AgentConfig, PermissionProfile};
use crate::context::{ContextStore, ContextSummarizer};
use crate::failover::{FailoverState, ProfileSnapshot};
use crate::policy::{AgentPolicy, AuthorizedToolExecutor};
use crate::profile::{BackendSource, Profile, ProfileRegistry, ProfileSet};
use crate::retry::{AttemptOrigin, Episode, RetryPolicy};
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
        modalities: Profile::default_modalities(),
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
            Arc::new(RwLock::new(kallipai_common::policy::ExecPolicy::default())),
            kallipai_common::policy::PolicyPreset::Default,
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
        episode: Episode::new(kallipai_common::timefmt::now_epoch()),
        attempt_origin: AttemptOrigin::System,
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
        usage_stats: crate::usage_stats::UsageStats::default(),
        endpoint_health: None,
        pending_profile_reset: Arc::new(std::sync::Mutex::new(None)),
        message_puller: None,
        persist_failures: Default::default(),
        gateway_signals: None,
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

/// The status a request carrying the placeholder key draws from the live
/// endpoint. A mock that answers with it keeps a round's own path through the
/// retry layers, so the wake tests reach the same classification without
/// touching the network.
pub(crate) const PLACEHOLDER_KEY_STATUS: u16 = 401;

/// A `MapSource` of loopback-mock backends: every provider id maps to a real
/// OpenAI-compatible backend pointed at a mock server, so a round's HTTP call
/// never leaves the host and a wall-clock budget races neither DNS nor TLS. The
/// mock answers every request with `status`. Keep the returned server alive for as
/// long as the context is used.
pub(crate) async fn mock_source(
    providers: &[&str],
    status: u16,
) -> (Arc<dyn BackendSource>, wiremock::MockServer) {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(status))
        .mount(&server)
        .await;
    let backend = just_llm_client::provider::OpenAiCompatBackend::new(
        reqwest::Client::builder().use_rustls_tls(),
        "test-key",
        Some(&server.uri()),
    )
    .expect("openai-compat backend constructs without network");
    let mut map = HashMap::new();
    for provider in providers {
        map.insert((*provider).into(), backend.clone() as Arc<dyn LlmBackend>);
    }
    (Arc::new(MapSource(map)), server)
}

/// A context over `profiles` whose backends answer from a loopback mock server with
/// `status`; the returned server handle must outlive the context.
pub(crate) async fn ctx_with_mock(
    profiles: Vec<Profile>,
    providers: &[&str],
    status: u16,
) -> (AgentContext, wiremock::MockServer) {
    let (source, server) = mock_source(providers, status).await;
    let ctx = ctx_from_source(profiles, source, RetryPolicy::default()).await;
    (ctx, server)
}
/// The window the compaction-failure pins shrink to: two `over_budget_turn_text`
/// turns exceed it while the budget shape stays feasible. The feasibility floor
/// rules out a 20K window — the tool-result truncated cap (10K) must stay well
/// below the summarizer input budget (window − output reserve − summary max), and
/// that budget must in turn exceed half the effective budget: each turn sits near
/// the halfway mark, so each must fit the summarizer on its own while the pair
/// together exceeds the effective budget.
pub(crate) const FAILURE_PIN_WINDOW: usize = 40_000;

/// Shrink `ctx`'s window to `window` and re-sync the store's pinned-budget guard —
/// the first two of the three steps `acquisition::reapply_window` applies on
/// failover. The third, `mark_needs_full_estimate`, is deliberately omitted: these
/// tests feed the gates `total_estimated_tokens`, a plain per-turn sum that never
/// consults the anchor flag the marker guards. Lets a test build an over-budget
/// store from small turns instead of the 600K-char wedge turn.
pub(crate) async fn set_context_window(ctx: &mut AgentContext, window: usize) {
    ctx.config
        .set_context_window(window)
        .expect("the test window must satisfy the budget invariants");
    let budget = ctx.config.pinned_budget();
    ctx.store.lock().await.set_pinned_budget(budget);
}

/// A user turn sized for [`FAILURE_PIN_WINDOW`]: the 20_000-char literal below
/// estimates to roughly 20K tokens (CJK text scores about one token per char), so
/// one turn fits the summarizer input budget while two exceed the effective
/// budget. The size is tied to that literal, not derived from the window constant;
/// the seed helper asserts both relations, so a drift fails loudly.
pub(crate) fn over_budget_turn_text() -> String {
    "错".repeat(20_000)
}

/// Seed `ctx`'s store with two [`over_budget_turn_text`] turns and assert the two
/// budget relations the compaction-failure pins rest on: the pair exceeds the
/// effective budget (otherwise no gate reaches compaction) and one turn fits the
/// summarizer input budget (otherwise compaction takes the oversized-turn slice
/// path instead of the summarize path). Returns the non-pinned turn count and the
/// store's token estimate.
pub(crate) async fn seed_over_budget_store(ctx: &mut AgentContext) -> (usize, usize) {
    let mut store = ctx.store.lock().await;
    store.push_turn(vec![user_msg(over_budget_turn_text())]);
    let single_turn = store.total_estimated_tokens();
    store.push_turn(vec![user_msg(over_budget_turn_text())]);
    let tokens = store.total_estimated_tokens();
    let effective = ctx.config.effective_budget();
    assert!(
        tokens > effective,
        "precondition: the store ({tokens}) must exceed the effective budget ({effective})"
    );
    let input_budget = effective.saturating_sub(ctx.summarizer.max_tokens as usize);
    assert!(
        single_turn < input_budget,
        "precondition: one turn ({single_turn}) must fit the summarizer input budget ({input_budget})"
    );
    let turns = store.turns().iter().filter(|t| !t.is_pinned()).count();
    (turns, tokens)
}

/// A [`Usage`] with only `prompt_tokens` set (and derived `total_tokens`).
pub(crate) fn usage(prompt_tokens: u32) -> Usage {
    usage_with_completion(prompt_tokens, 0)
}

// --- chat-type construction funnel ----------------------------------------------
// Every test-side chat type construction goes through these helpers, so
// body changes touch no test call site. Keep call sites free of chat-face
// type names: helpers own the types, callers own the intent.

/// A system turn carrying `text` (the wire face of a configured system prompt).
pub(crate) fn system_msg(text: impl Into<String>) -> Message {
    Message::system(text)
}

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

/// One queued stream answer: plain events, events followed by a mid-stream drop, or a
/// stream that never yields (for cancel tests).
enum CannedStream {
    Events(Vec<just_llm_client::types::generation::GenerationEvent>),
    /// The wire shape of a stream that dies mid-way: some events, then the transport
    /// error.
    Drop {
        events: Vec<just_llm_client::types::generation::GenerationEvent>,
        error: just_llm_client::TransportError,
    },
    /// Never yields an item — the stream hangs until the caller gives up.
    Pending,
}

/// Minimal stateful backend modeled on the upstream conversation test mock: it records
/// the wire requests it receives through `prepare_streaming` and answers the streaming
/// path from queues of canned streams — plain events, events followed by a mid-stream
/// drop, or a stream that never yields — plus an optional pre-stream error queue, so
/// tests can assert exactly what reached the provider boundary. Never touches the network.
pub(crate) struct RecordingBackend {
    requests: Mutex<Vec<GenerationRequest>>,
    failures: Mutex<VecDeque<just_llm_client::BackendError>>,
    streams: Mutex<VecDeque<CannedStream>>,
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
        self.streams
            .lock()
            .unwrap()
            .push_back(CannedStream::Events(vec![
                just_llm_client::types::generation::GenerationEvent::Text {
                    delta: "a".to_owned(),
                },
                just_llm_client::types::generation::GenerationEvent::End {
                    finish_reason: None,
                    response_id: Some(response_id.to_owned()),
                },
            ]));
    }

    /// Queue an error that surfaces from `prepare_streaming` itself (pre-stream).
    pub(crate) fn queue_error(&self, error: just_llm_client::BackendError) {
        self.failures.lock().unwrap().push_back(error);
    }

    /// Queue a stream that delivers `events` and then dies with `error` mid-stream.
    pub(crate) fn queue_dropped_stream(
        &self,
        events: Vec<just_llm_client::types::generation::GenerationEvent>,
        error: just_llm_client::TransportError,
    ) {
        self.streams
            .lock()
            .unwrap()
            .push_back(CannedStream::Drop { events, error });
    }

    /// Queue a stream that never yields an item (a cancel test hangs on it).
    pub(crate) fn queue_pending_stream(&self) {
        self.streams
            .lock()
            .unwrap()
            .push_back(CannedStream::Pending);
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
        unimplemented!("the recording backend covers the streaming path only")
    }

    fn prepare_streaming(
        &self,
        request: GenerationRequest,
    ) -> Result<reqwest::Request, just_llm_client::BackendError> {
        self.requests.lock().unwrap().push(request);
        if let Some(error) = self.failures.lock().unwrap().pop_front() {
            return Err(error);
        }
        let url = "http://recording.invalid/v1/chat/completions"
            .parse()
            .expect("static recording url parses");
        Ok(reqwest::Request::new(reqwest::Method::POST, url))
    }

    async fn send(
        &self,
        _prepared: reqwest::Request,
    ) -> Result<reqwest::Response, just_llm_client::BackendError> {
        let response = http::Response::builder()
            .status(reqwest::StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(String::new())
            .expect("static recording response builds");
        Ok(reqwest::Response::from(response))
    }

    async fn parse(
        &self,
        _response: reqwest::Response,
    ) -> Result<just_llm_client::types::generation::GenerationResponse, just_llm_client::BackendError>
    {
        unimplemented!("the recording backend covers the streaming path only")
    }

    async fn parse_streaming(
        &self,
        _response: reqwest::Response,
    ) -> Result<just_llm_client::GenerationStream, just_llm_client::BackendError> {
        let canned = self
            .streams
            .lock()
            .unwrap()
            .pop_front()
            .expect("no stream queued for recording backend");
        let stream: std::pin::Pin<
            Box<
                dyn futures_util::Stream<
                        Item = Result<
                            just_llm_client::types::generation::GenerationEvent,
                            just_llm_client::TransportError,
                        >,
                    > + Send,
            >,
        > = match canned {
            CannedStream::Events(events) => Box::pin(futures_util::stream::iter(
                events
                    .into_iter()
                    .map(Ok::<_, just_llm_client::TransportError>),
            )),
            CannedStream::Drop { events, error } => Box::pin(futures_util::stream::iter(
                events
                    .into_iter()
                    .map(Ok::<_, just_llm_client::TransportError>)
                    .chain(std::iter::once(Err(error))),
            )),
            CannedStream::Pending => Box::pin(futures_util::stream::pending::<
                Result<
                    just_llm_client::types::generation::GenerationEvent,
                    just_llm_client::TransportError,
                >,
            >()),
        };
        Ok(just_llm_client::GenerationStream::new(stream))
    }

    fn render_messages(
        &self,
        messages: &[Message],
    ) -> Result<String, just_llm_client::BackendError> {
        // Token estimation renders through the backend. The recording backend has
        // no wire format, so it renders the text face: enough for a stable count,
        // and enough for a round to run end to end against it.
        Ok(messages
            .iter()
            .filter_map(|m| m.content())
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn render_tools(
        &self,
        tools: &[just_llm_client::types::generation::ToolDefinition],
    ) -> Result<String, just_llm_client::BackendError> {
        // Tool schemas are rendered by name: the count is what estimation needs.
        Ok(tools
            .iter()
            .map(|t| t.function.name.as_str())
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn family() -> &'static str {
        "recording"
    }

    fn new(
        _http: reqwest::ClientBuilder,
        _api_key: &str,
        _base_url: Option<&str>,
    ) -> Result<Arc<dyn LlmBackend>, just_llm_client::BackendConstructError> {
        unimplemented!("the recording backend covers the streaming path only")
    }
}

/// Records what the retry paths wrote into the shared fact table, so a test can
/// assert a write happened with no tagma behind the context — and answers reads
/// from what it holds, so a seeded fact reaches a verdict.
#[derive(Default)]
pub(crate) struct RecordingHealth {
    blocks: Mutex<Vec<(String, Block)>>,
    /// Provider ids listed in one inner vector share an account. Ids the groups
    /// do not name read as different accounts, the fail-open default.
    accounts: Mutex<Vec<Vec<String>>>,
}

impl RecordingHealth {
    /// Everything recorded so far, oldest first.
    pub(crate) fn recorded(&self) -> Vec<(String, Block)> {
        self.blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Seed a fact as if another agent had written it: no write happens on this
    /// context, which is what a reader's own reading must pick up.
    pub(crate) fn seed(&self, provider_id: &str, block: Block) {
        self.blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((provider_id.to_string(), block));
    }

    /// Declare two provider ids as one account: the credential they share, which
    /// this double has no hash for.
    pub(crate) fn share_account(&self, a: &str, b: &str) {
        self.accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(vec![a.to_string(), b.to_string()]);
    }
}

impl EndpointHealth for RecordingHealth {
    fn record(&self, provider_id: &str, block: Block) {
        self.blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((provider_id.to_string(), block));
    }

    fn blocked_for(&self, provider_id: &str, now: u64) -> Option<BlockedFact> {
        let accounts = self.accounts.lock().unwrap_or_else(|e| e.into_inner());
        // An id shares this provider's account when one group names both.
        let same_account = |id: &str| {
            accounts.iter().any(|group| {
                group.iter().any(|member| member == id)
                    && group.iter().any(|member| member == provider_id)
            })
        };
        // Newest first: the same reading order the tagma's table uses. An
        // account-level fact answers for every endpoint on the credential, which
        // is what `share_account` declares here.
        self.blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|(id, block)| {
                is_active(block.recorded_at, block.until, now)
                    && (id == provider_id || (block.kind.is_account_level() && same_account(id)))
            })
            .map(|(holder, block)| BlockedFact {
                block: block.clone(),
                // The holder's hint, as the tagma's table computes it from the
                // credential the fact was recorded through — not the asker's.
                account_hint: account_hint(holder),
            })
    }

    fn snapshot(&self, _now: u64) -> Vec<FactView> {
        Vec::new()
    }
}

/// A deterministic stand-in for the tagma's 8-hex credential hint: the double
/// has no credential to hash.
pub(crate) fn account_hint(provider_id: &str) -> String {
    let digest = provider_id.bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    format!("{digest:08x}")
}
