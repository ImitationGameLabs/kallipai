//! Tagma-owned backend construction.
//!
//! The tagma owns the HTTP-client concern (reqwest TLS + connect/read timeouts) and, via [`BackendFactory`],
//! builds one shared [`LlmBackend`] per provider. At startup only the **active set** — each
//! set's `profiles[0]` — is built, so misconfiguration of the primary path fails fast. Failover
//! profiles' providers are built lazily by [`TagmaBackendSource`] on first use (within-set
//! failover). The resulting [`BackendSource`] is handed to `ProfileRegistry`, which does
//! selection + lookup only; the runtime reuses `reqwest` types for HTTP-shape retry
//! classification (see `retry.rs`) but never constructs a backend.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use just_llm_client::CapabilityNegotiation;
use just_llm_client::client::BackendFactory;
use just_llm_client::family;
use just_llm_client::types::generation::{
    GenerationRequest, GenerationResponse, Message, ToolDefinition,
};
use just_llm_client::{
    BackendConstructError, BackendError, GenerationClient, GenerationClientOptions,
    GenerationStream, Identifiable, LlmBackend,
};
use kallip_runtime::profile::{BackendSource, NO_PROFILE_HINT, ProfileConfig, Provider};

/// Default timeout for establishing the outbound LLM HTTP connection (DNS + TCP + TLS). Distinct
/// from [`DEFAULT_READ_TIMEOUT`], which bounds per-read idle.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default per-read idle timeout for outbound LLM HTTP calls. Unlike a whole-request timeout,
/// reqwest's `read_timeout` resets on each successful read, so a streaming completion that keeps
/// producing tokens (or SSE keep-alive comments) never trips it — only genuine connection silence
/// does. It also bounds time-to-response-headers. To apply a custom timeout/proxy, build the
/// `reqwest::Client` differently inside [`build_one`].
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Default `User-Agent` for outbound LLM HTTP calls: `kallip/<tagma-version>`, with the version
/// inlined at compile time from this crate's `Cargo.toml` (`env!("CARGO_PKG_VERSION")`). Override
/// per-process with `KALLIP_LLM_API_USER_AGENT`.
pub(crate) const DEFAULT_USER_AGENT: &str = concat!("kallip/", env!("CARGO_PKG_VERSION"));

/// Resolve the effective `User-Agent`: a non-empty `provided` value (forwarded verbatim, leading/
/// trailing whitespace included) wins; otherwise the built-in default. The `.trim()` only decides
/// fallback — it does not trim the returned value. Borrows `provided` (or returns the `'static`
/// default), so callers needing ownership copy as needed.
pub(crate) fn resolve_user_agent(provided: Option<&str>) -> &str {
    provided
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_USER_AGENT)
}

/// Validate every provider referenced by `cfg`'s sets (active **and** failover): the provider
/// exists, its family is registered with `factory`, and an openai-compatible provider declares a
/// `base_url`. Cheap — no construction — so misconfiguration fails fast at startup, before any
/// agent relies on a failover profile. Unreferenced providers are not checked (dead config).
fn validate_providers(cfg: &ProfileConfig, factory: &BackendFactory) -> Result<()> {
    let families: Vec<&str> = factory.families().collect();
    for set in cfg.sets.values() {
        for profile in &set.profiles {
            let provider = cfg.endpoints.get(&profile.endpoint).with_context(|| {
                format!(
                    "profile '{}' references unknown provider '{}'",
                    profile.id, profile.endpoint
                )
            })?;
            if !families.contains(&provider.family.as_str()) {
                bail!(
                    "provider '{}' has unknown family '{}' (registered: {})",
                    provider.id,
                    provider.family,
                    families.join(", ")
                );
            }
            if provider.family == family::OPENAI_COMPATIBLE && provider.base_url.is_none() {
                bail!(
                    "provider '{}' (openai-compatible) requires a base_url",
                    provider.id
                );
            }
        }
    }
    Ok(())
}

/// Build one backend for `provider` via the factory: a fresh `reqwest::Client` (rustls TLS, the
/// default connect + per-read idle timeouts, and the resolved `User-Agent`) with credentials
/// passed into the constructor.
///
/// The `User-Agent` survives upstream today because `BackendFactory::create` forwards our builder
/// verbatim and `just-common::build_client` injects only `Authorization`/`Accept` — it sets no UA of
/// its own. This is an implementation-level property, not a contract: migrate to a
/// `GenerationClientOptions`-level UA API if upstream ever adds one. An override containing characters
/// illegal in a header value (e.g. CR/LF, control bytes) fails fast here —
/// `reqwest::ClientBuilder::build` rejects it and the error bubbles up to the caller — at startup
/// for the active set, lazily on first failover use otherwise.
pub(crate) fn build_one(
    factory: &BackendFactory,
    provider: &Provider,
    user_agent: &str,
) -> Result<Arc<dyn LlmBackend>> {
    let builder = reqwest::Client::builder()
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .read_timeout(DEFAULT_READ_TIMEOUT)
        .use_rustls_tls()
        .user_agent(user_agent);
    factory
        .create(
            &provider.family,
            builder,
            &provider.api_key,
            provider.base_url.as_deref(),
        )
        .with_context(|| format!("failed to build backend for provider '{}'", provider.id))
}

/// Validate the whole config, pre-build the **active set**, and return a lazily-constructing
/// [`BackendSource`]. Failover endpoints (`profiles[1..]`) are validated but **not** built here —
/// [`TagmaBackendSource`] builds them on first failover use.
pub fn build_backends(
    cfg: &ProfileConfig,
    factory: BackendFactory,
    user_agent: &str,
) -> Result<Arc<dyn BackendSource>> {
    validate_providers(cfg, &factory)?;

    let mut cache = HashMap::new();
    for set in cfg.sets.values() {
        let active = set.active_profile();
        if cache.contains_key(&active.endpoint) {
            continue;
        }
        let provider = cfg.endpoints.get(&active.endpoint).with_context(|| {
            format!(
                "active profile '{}' references unknown provider '{}'",
                active.id, active.endpoint
            )
        })?;
        cache.insert(
            active.endpoint.clone(),
            build_one(&factory, provider, user_agent)?,
        );
    }

    Ok(Arc::new(TagmaBackendSource {
        providers: cfg.endpoints.clone(),
        factory,
        user_agent: user_agent.to_string(),
        cache: std::sync::Mutex::new(cache),
    }))
}

/// Tagma-owned [`BackendSource`]: a locked cache of built backends over the provider config and
/// factory. Active providers are pre-seeded at construction; failover providers are built on first
/// [`get`](BackendSource::get), under the cache lock, so concurrent callers share one backend.
pub struct TagmaBackendSource {
    providers: HashMap<String, Provider>,
    factory: BackendFactory,
    user_agent: String,
    cache: std::sync::Mutex<HashMap<String, Arc<dyn LlmBackend>>>,
}

impl BackendSource for TagmaBackendSource {
    fn get(&self, provider_id: &str) -> Result<Arc<dyn LlmBackend>> {
        // Fast path: return the cached backend if present. `.ok()` treats a poisoned mutex as a
        // cache miss (a panicked builder doesn't invalidate already-built backends, so poison must
        // not brick subsequent lookups).
        if let Some(backend) = self
            .cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(provider_id).cloned())
        {
            return Ok(backend);
        }
        // Slow path: resolve + build OUTSIDE the lock, so concurrent first-failover lookups for
        // *other* providers aren't blocked on reqwest/rustls client construction.
        let provider = self.providers.get(provider_id).with_context(|| {
            format!(
                "unknown provider '{provider_id}'; if no profile is \
                    configured yet, add one via the tagma management page"
            )
        })?;
        let backend = build_one(&self.factory, provider, &self.user_agent)?;
        // Re-lock to publish; a racing builder may have inserted first — reuse theirs. Poison is
        // recovered (the map is still valid data), so a prior panic doesn't propagate.
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = cache.get(provider_id) {
            return Ok(existing.clone());
        }
        cache.insert(provider_id.to_string(), backend.clone());
        Ok(backend)
    }
}

/// Sentinel marker for the profile-less root (empty-profile boot): the
/// placeholder profile's endpoint id and the sentinel backend's family.
pub(crate) const UNCONFIGURED: &str = "unconfigured";

/// The profile-less root's placeholder set (`ProfileSet { one placeholder
/// profile }`), shared by first boot (`Materialize::run`) and restore
/// (`restore_one`) so both paths register the root against the sentinel.
pub(crate) fn unconfigured_set() -> kallip_runtime::profile::ProfileSet {
    let placeholder = kallip_runtime::profile::Profile {
        id: UNCONFIGURED.into(),
        endpoint: UNCONFIGURED.into(),
        model: UNCONFIGURED.into(),
        max_context_window: 128_000,
    };
    kallip_runtime::profile::ProfileSet {
        name: UNCONFIGURED.into(),
        description: None,
        profiles: vec![placeholder],
    }
}

/// Backend handed to the profile-less root's `GenerationClient`: every call fails
/// with the management-page hint until the first profile lands, at which
/// point `apply_pending_profile_reset` rebuilds the client with a real
/// backend — keeping `GenerationClient` non-optional throughout the runtime.
struct UnconfiguredBackend;

fn unconfigured_error() -> BackendError {
    BackendError::provider(UNCONFIGURED, std::io::Error::other(NO_PROFILE_HINT))
}

impl Identifiable for UnconfiguredBackend {
    fn family(&self) -> &'static str {
        UNCONFIGURED
    }
}

// Defaults surface every capability as unsupported for this family.
impl CapabilityNegotiation for UnconfiguredBackend {}

#[async_trait::async_trait]
impl LlmBackend for UnconfiguredBackend {
    fn prepare(&self, _: GenerationRequest) -> Result<reqwest::Request, BackendError> {
        Err(unconfigured_error())
    }

    fn prepare_streaming(&self, _: GenerationRequest) -> Result<reqwest::Request, BackendError> {
        Err(unconfigured_error())
    }

    async fn send(&self, _: reqwest::Request) -> Result<reqwest::Response, BackendError> {
        Err(unconfigured_error())
    }

    async fn parse(&self, _: reqwest::Response) -> Result<GenerationResponse, BackendError> {
        Err(unconfigured_error())
    }

    async fn parse_streaming(
        &self,
        _: reqwest::Response,
    ) -> Result<GenerationStream, BackendError> {
        Err(unconfigured_error())
    }

    fn render_messages(&self, _: &[Message]) -> Result<String, BackendError> {
        Err(unconfigured_error())
    }

    fn render_tools(&self, _: &[ToolDefinition]) -> Result<String, BackendError> {
        Err(unconfigured_error())
    }

    fn family() -> &'static str
    where
        Self: Sized,
    {
        UNCONFIGURED
    }

    // Never factory-constructed; inputs are ignored.
    fn new(
        _http: reqwest::ClientBuilder,
        _api_key: &str,
        _base_url: Option<&str>,
    ) -> Result<Arc<dyn LlmBackend>, BackendConstructError>
    where
        Self: Sized,
    {
        Ok(Arc::new(UnconfiguredBackend))
    }
}

/// The profile-less root's client: typed, but every LLM call fails with the
/// management-page hint until a real profile is applied.
pub(crate) fn unconfigured_client(system_prompt: Option<String>) -> GenerationClient {
    let mut options = GenerationClientOptions::new(UNCONFIGURED.to_string());
    if let Some(sp) = system_prompt {
        options = options.with_system_prompt(sp);
    }
    GenerationClient::new(Arc::new(UnconfiguredBackend), options)
}
#[cfg(test)]
mod tests {
    use super::*;
    use just_llm_client::types::generation::{GenerationRequest, Message};
    use kallip_runtime::profile::{Profile, ProfileSet};
    use std::collections::BTreeMap;

    #[test]
    fn sentinel_errors_carry_the_management_page_hint() {
        let err = UnconfiguredBackend
            .render_messages(&[])
            .expect_err("sentinel render fails");
        assert!(
            format!("{err}").contains(kallip_runtime::profile::NO_PROFILE_HINT),
            "got: {err}"
        );
    }
    /// One deepseek provider + a single-profile set referencing it.
    fn ds_cfg() -> ProfileConfig {
        single_set_cfg("ds", "p", "ds")
    }

    /// Build a one-set config whose active profile references `provider_id`.
    fn single_set_cfg(provider_id: &str, profile: &str, endpoint: &str) -> ProfileConfig {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            provider_id.into(),
            Provider {
                id: provider_id.into(),
                family: family::DEEPSEEK.into(),
                api_key: "fake".into(),
                base_url: None,
            },
        );
        ProfileConfig {
            sets: BTreeMap::from([(
                "default".to_string(),
                ProfileSet {
                    name: "default".into(),
                    description: None,
                    profiles: vec![Profile {
                        id: profile.into(),
                        endpoint: endpoint.into(),
                        model: "deepseek-test".into(),
                        max_context_window: 500_000,
                    }],
                },
            )]),
            default: "default".into(),
            endpoints,
            parking: vec![],
        }
    }

    #[test]
    fn active_provider_pre_built_and_lookup_succeeds() {
        let source = build_backends(&ds_cfg(), BackendFactory::new(), DEFAULT_USER_AGENT).unwrap();
        // The active provider is pre-built, so lookup succeeds without lazy construction.
        assert!(source.get("ds").is_ok());
    }

    #[test]
    fn only_active_set_is_pre_built() {
        // Two profiles in one set: active "ds" (profiles[0]) + failover "backup" (profiles[1]).
        let mut cfg = ds_cfg();
        cfg.endpoints.insert(
            "backup".into(),
            Provider {
                id: "backup".into(),
                family: family::DEEPSEEK.into(),
                api_key: "fake".into(),
                base_url: None,
            },
        );
        cfg.sets.get_mut("default").unwrap().profiles.push(Profile {
            id: "p2".into(),
            endpoint: "backup".into(),
            model: "deepseek-backup".into(),
            max_context_window: 500_000,
        });

        let source = build_backends(&cfg, BackendFactory::new(), DEFAULT_USER_AGENT).unwrap();
        // The failover provider is validated but not pre-built — its lookup builds it lazily.
        assert!(
            source.get("backup").is_ok(),
            "failover provider builds lazily"
        );
    }

    #[test]
    fn unreferenced_provider_does_not_block_startup() {
        // A second provider no profile references: not validated, not built — startup succeeds.
        let mut cfg = ds_cfg();
        cfg.endpoints.insert(
            "dead".into(),
            Provider {
                id: "dead".into(),
                family: "anthropic".into(), // a valid family; the point is it is unreferenced
                api_key: "fake".into(),
                base_url: None,
            },
        );
        assert!(build_backends(&cfg, BackendFactory::new(), DEFAULT_USER_AGENT).is_ok());
    }

    #[test]
    fn unknown_family_referenced_errors() {
        let mut cfg = ds_cfg();
        // Any family string no factory registers — the registered set is
        // whatever the factory was built with (feature-gated registrations included).
        cfg.endpoints.get_mut("ds").unwrap().family = "nonexistent".into();
        let err = build_backends(&cfg, BackendFactory::new(), DEFAULT_USER_AGENT)
            .err()
            .expect("unregistered family should error");
        let msg = format!("{err}");
        assert!(msg.contains("unknown family 'nonexistent'"), "got: {msg}");
    }

    #[test]
    fn responses_family_builds_without_base_url() {
        // The responses family defaults to the official OpenAI endpoint when
        // base_url is omitted; construction is offline and must succeed.
        let mut cfg = ds_cfg();
        cfg.endpoints.get_mut("ds").unwrap().family = family::OPENAI_RESPONSES.into();
        cfg.sets.get_mut("default").unwrap().profiles[0].model = "gpt-test".into();
        let source = build_backends(&cfg, BackendFactory::new(), DEFAULT_USER_AGENT).unwrap();
        assert!(source.get("ds").is_ok(), "responses family builds");
    }

    #[test]
    fn anthropic_family_builds_without_base_url() {
        // Same optional-base_url rule; construction is offline.
        let mut cfg = ds_cfg();
        cfg.endpoints.get_mut("ds").unwrap().family = family::ANTHROPIC.into();
        cfg.sets.get_mut("default").unwrap().profiles[0].model = "claude-test".into();
        let source = build_backends(&cfg, BackendFactory::new(), DEFAULT_USER_AGENT).unwrap();
        assert!(source.get("ds").is_ok(), "anthropic family builds");
    }

    #[test]
    fn openai_compat_provider_without_base_url_errors() {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "oa".into(),
            Provider {
                id: "oa".into(),
                family: family::OPENAI_COMPATIBLE.into(),
                api_key: "fake".into(),
                base_url: None, // missing — must fail fast at startup
            },
        );
        let cfg = ProfileConfig {
            sets: BTreeMap::from([(
                "default".to_string(),
                ProfileSet {
                    name: "default".into(),
                    description: None,
                    profiles: vec![Profile {
                        id: "p".into(),
                        endpoint: "oa".into(),
                        model: "gpt-4.1-mini".into(),
                        max_context_window: 128_000,
                    }],
                },
            )]),
            default: "default".into(),
            endpoints,
            parking: vec![],
        };
        let err = build_backends(&cfg, BackendFactory::new(), DEFAULT_USER_AGENT)
            .err()
            .expect("openai-compatible without base_url should error");
        let msg = format!("{err}");
        assert!(msg.contains("requires a base_url"), "got: {msg}");
    }

    /// The resolved `User-Agent` reaches the provider on the wire. reqwest applies client default
    /// headers (including `User-Agent`) at `execute` time, not at `Request::build`, so this must send
    /// a real request and assert the header at a mock server — it cannot be inspected on the prepared
    /// `reqwest::Request`. Guards against an upstream `just-common::build_client` change silently
    /// overwriting our UA.
    async fn assert_user_agent_sent(user_agent: &str) {
        use wiremock::matchers::{header, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("user-agent", user_agent))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        // openai-compatible so the request targets `{base_url}/chat/completions`.
        let endpoint = Provider {
            id: "ua".into(),
            family: family::OPENAI_COMPATIBLE.into(),
            api_key: "test".into(),
            base_url: Some(server.uri()),
        };
        let factory = BackendFactory::new();
        let backend = build_one(&factory, &endpoint, user_agent).expect("backend builds");
        let request = GenerationRequest::new("m", vec![Message::user("hi")]);
        let prepared = backend.prepare(request).expect("prepare serializes");
        // `send` does not parse — a bare 200 satisfies it. The mock's `.and(header(...))` is the
        // assertion: a non-matching UA means zero hits, failing `.expect(1)` when `server` drops.
        backend.send(prepared).await.expect("send succeeds");
    }

    #[tokio::test]
    async fn default_user_agent_reaches_provider() {
        assert_user_agent_sent(DEFAULT_USER_AGENT).await;
    }

    #[tokio::test]
    async fn override_user_agent_reaches_provider() {
        assert_user_agent_sent("acme-bot/9").await;
    }
}
