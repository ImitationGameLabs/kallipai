//! Daemon-owned backend construction.
//!
//! The daemon owns the HTTP-client concern (reqwest TLS/timeout) and, via [`BackendFactory`],
//! builds one shared [`LlmBackend`] per endpoint. At startup only the **active set** — each
//! tier's `profiles[0]` — is built, so misconfiguration of the primary path fails fast. Failover
//! profiles' endpoints are built lazily by [`DaemonBackendSource`] on first use (within-tier
//! failover). The resulting [`BackendSource`] is handed to `ProfileRegistry`, which does
//! selection + lookup only; the runtime reuses `reqwest` types for HTTP-shape retry
//! classification (see `retry.rs`) but never constructs a backend.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use just_agent_runtime::profile::{BackendSource, Endpoint, ProfileConfig};
use just_llm_client::LlmBackend;
use just_llm_client::client::BackendFactory;
use just_llm_client::family;

/// Default total-request timeout for outbound LLM HTTP calls. LLM completions can be slow, but
/// this also bounds streaming — to apply a custom timeout/proxy, build the `reqwest::Client`
/// differently inside [`build_one`].
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Default `User-Agent` for outbound LLM HTTP calls: `just-agent/<daemon-version>`, with the version
/// inlined at compile time from this crate's `Cargo.toml` (`env!("CARGO_PKG_VERSION")`). Override
/// per-process with `JUST_AGENT_LLM_API_USER_AGENT`.
pub(crate) const DEFAULT_USER_AGENT: &str = concat!("just-agent/", env!("CARGO_PKG_VERSION"));

/// Resolve the effective `User-Agent`: a non-empty `provided` value (forwarded verbatim, leading/
/// trailing whitespace included) wins; otherwise the built-in default. The `.trim()` only decides
/// fallback — it does not trim the returned value. Borrows `provided` (or returns the `'static`
/// default), so callers needing ownership copy as needed.
pub(crate) fn resolve_user_agent(provided: Option<&str>) -> &str {
    provided
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_USER_AGENT)
}

/// Validate every endpoint referenced by `cfg`'s tiers (active **and** failover): the endpoint
/// exists, its family is registered with `factory`, and an openai-compatible endpoint declares a
/// `base_url`. Cheap — no construction — so misconfiguration fails fast at startup, before any
/// agent relies on a failover profile. Unreferenced endpoints are not checked (dead config).
fn validate_endpoints(cfg: &ProfileConfig, factory: &BackendFactory) -> Result<()> {
    let families: Vec<&str> = factory.families().collect();
    for tier in &cfg.tiers {
        for profile in &tier.profiles {
            let endpoint = cfg.endpoints.get(&profile.endpoint).with_context(|| {
                format!(
                    "profile '{}' references unknown endpoint '{}'",
                    profile.id, profile.endpoint
                )
            })?;
            if !families.contains(&endpoint.family.as_str()) {
                bail!(
                    "endpoint '{}' has unknown family '{}' (registered: {})",
                    endpoint.id,
                    endpoint.family,
                    families.join(", ")
                );
            }
            if endpoint.family == family::OPENAI_COMPATIBLE && endpoint.base_url.is_none() {
                bail!(
                    "endpoint '{}' (openai-compatible) requires a base_url",
                    endpoint.id
                );
            }
        }
    }
    Ok(())
}

/// Build one backend for `endpoint` via the factory: a fresh `reqwest::Client` (rustls TLS, the default
/// timeout, and the resolved `User-Agent`) with credentials passed into the constructor.
///
/// The `User-Agent` survives upstream today because `BackendFactory::create` forwards our builder
/// verbatim and `just-common::build_client` injects only `Authorization`/`Accept` — it sets no UA of
/// its own. This is an implementation-level property, not a contract: migrate to a
/// `ChatClientOptions`-level UA API if upstream ever adds one. An override containing characters
/// illegal in a header value (e.g. CR/LF, control bytes) fails fast here —
/// `reqwest::ClientBuilder::build` rejects it and the error bubbles up to the caller — at startup
/// for the active set, lazily on first failover use otherwise.
fn build_one(
    factory: &BackendFactory,
    endpoint: &Endpoint,
    user_agent: &str,
) -> Result<Arc<dyn LlmBackend>> {
    let builder = reqwest::Client::builder()
        .timeout(DEFAULT_HTTP_TIMEOUT)
        .use_rustls_tls()
        .user_agent(user_agent);
    factory
        .create(
            &endpoint.family,
            builder,
            &endpoint.api_key,
            endpoint.base_url.as_deref(),
        )
        .with_context(|| format!("failed to build backend for endpoint '{}'", endpoint.id))
}

/// Validate the whole config, pre-build the **active set**, and return a lazily-constructing
/// [`BackendSource`]. Failover endpoints (`profiles[1..]`) are validated but **not** built here —
/// [`DaemonBackendSource`] builds them on first failover use.
pub fn build_backends(
    cfg: &ProfileConfig,
    factory: BackendFactory,
    user_agent: &str,
) -> Result<Arc<dyn BackendSource>> {
    validate_endpoints(cfg, &factory)?;

    let mut cache = HashMap::new();
    for tier in &cfg.tiers {
        let active = tier.active_profile();
        if cache.contains_key(&active.endpoint) {
            continue;
        }
        let endpoint = cfg.endpoints.get(&active.endpoint).with_context(|| {
            format!(
                "active profile '{}' references unknown endpoint '{}'",
                active.id, active.endpoint
            )
        })?;
        cache.insert(
            active.endpoint.clone(),
            build_one(&factory, endpoint, user_agent)?,
        );
    }

    Ok(Arc::new(DaemonBackendSource {
        endpoints: cfg.endpoints.clone(),
        factory,
        user_agent: user_agent.to_string(),
        cache: std::sync::Mutex::new(cache),
    }))
}

/// Daemon-owned [`BackendSource`]: a locked cache of built backends over the endpoint config and
/// factory. Active endpoints are pre-seeded at construction; failover endpoints are built on first
/// [`get`](BackendSource::get), under the cache lock, so concurrent callers share one backend.
pub struct DaemonBackendSource {
    endpoints: HashMap<String, Endpoint>,
    factory: BackendFactory,
    user_agent: String,
    cache: std::sync::Mutex<HashMap<String, Arc<dyn LlmBackend>>>,
}

impl BackendSource for DaemonBackendSource {
    fn get(&self, endpoint_id: &str) -> Result<Arc<dyn LlmBackend>> {
        // Fast path: return the cached backend if present. `.ok()` treats a poisoned mutex as a
        // cache miss (a panicked builder doesn't invalidate already-built backends, so poison must
        // not brick subsequent lookups).
        if let Some(backend) = self
            .cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(endpoint_id).cloned())
        {
            return Ok(backend);
        }
        // Slow path: resolve + build OUTSIDE the lock, so concurrent first-failover lookups for
        // *other* endpoints aren't blocked on reqwest/rustls client construction.
        let endpoint = self
            .endpoints
            .get(endpoint_id)
            .with_context(|| format!("unknown endpoint '{endpoint_id}'"))?;
        let backend = build_one(&self.factory, endpoint, &self.user_agent)?;
        // Re-lock to publish; a racing builder may have inserted first — reuse theirs. Poison is
        // recovered (the map is still valid data), so a prior panic doesn't propagate.
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = cache.get(endpoint_id) {
            return Ok(existing.clone());
        }
        cache.insert(endpoint_id.to_string(), backend.clone());
        Ok(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use just_agent_runtime::profile::{Profile, Tier};
    use just_llm_client::types::chat::{ChatCompletionRequest, ChatMessage};

    /// One deepseek endpoint + a single-profile tier referencing it.
    fn ds_cfg() -> ProfileConfig {
        single_tier_cfg("ds", "p", "ds")
    }

    /// Build a one-tier config whose active profile references `endpoint_id`.
    fn single_tier_cfg(endpoint_id: &str, profile: &str, endpoint: &str) -> ProfileConfig {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            endpoint_id.into(),
            Endpoint {
                id: endpoint_id.into(),
                family: family::DEEPSEEK.into(),
                api_key: "fake".into(),
                base_url: None,
            },
        );
        ProfileConfig {
            tiers: vec![Tier {
                profiles: vec![Profile {
                    id: profile.into(),
                    endpoint: endpoint.into(),
                    model: "deepseek-test".into(),
                    max_context_window: 500_000,
                }],
            }],
            endpoints,
        }
    }

    #[test]
    fn active_endpoint_pre_built_and_lookup_succeeds() {
        let source = build_backends(&ds_cfg(), BackendFactory::new(), DEFAULT_USER_AGENT).unwrap();
        // The active endpoint is pre-built, so lookup succeeds without lazy construction.
        assert!(source.get("ds").is_ok());
    }

    #[test]
    fn only_active_set_is_pre_built() {
        // Two profiles in one tier: active "ds" (profiles[0]) + failover "backup" (profiles[1]).
        let mut cfg = ds_cfg();
        cfg.endpoints.insert(
            "backup".into(),
            Endpoint {
                id: "backup".into(),
                family: family::DEEPSEEK.into(),
                api_key: "fake".into(),
                base_url: None,
            },
        );
        cfg.tiers[0].profiles.push(Profile {
            id: "p2".into(),
            endpoint: "backup".into(),
            model: "deepseek-backup".into(),
            max_context_window: 500_000,
        });

        let source = build_backends(&cfg, BackendFactory::new(), DEFAULT_USER_AGENT).unwrap();
        // The failover endpoint is validated but not pre-built — its lookup builds it lazily.
        assert!(
            source.get("backup").is_ok(),
            "failover endpoint builds lazily"
        );
    }

    #[test]
    fn unreferenced_endpoint_does_not_block_startup() {
        // A second endpoint no profile references: not validated, not built — startup succeeds.
        let mut cfg = ds_cfg();
        cfg.endpoints.insert(
            "dead".into(),
            Endpoint {
                id: "dead".into(),
                family: "anthropic".into(), // would be invalid if referenced, but it's not
                api_key: "fake".into(),
                base_url: None,
            },
        );
        assert!(build_backends(&cfg, BackendFactory::new(), DEFAULT_USER_AGENT).is_ok());
    }

    #[test]
    fn unknown_family_referenced_errors() {
        let mut cfg = ds_cfg();
        cfg.endpoints.get_mut("ds").unwrap().family = "anthropic".into();
        let err = build_backends(&cfg, BackendFactory::new(), DEFAULT_USER_AGENT)
            .err()
            .expect("unregistered family should error");
        let msg = format!("{err}");
        assert!(msg.contains("unknown family 'anthropic'"), "got: {msg}");
    }

    #[test]
    fn openai_compat_endpoint_without_base_url_errors() {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "oa".into(),
            Endpoint {
                id: "oa".into(),
                family: family::OPENAI_COMPATIBLE.into(),
                api_key: "fake".into(),
                base_url: None, // missing — must fail fast at startup
            },
        );
        let cfg = ProfileConfig {
            tiers: vec![Tier {
                profiles: vec![Profile {
                    id: "p".into(),
                    endpoint: "oa".into(),
                    model: "gpt-4.1-mini".into(),
                    max_context_window: 128_000,
                }],
            }],
            endpoints,
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
        let endpoint = Endpoint {
            id: "ua".into(),
            family: family::OPENAI_COMPATIBLE.into(),
            api_key: "test".into(),
            base_url: Some(server.uri()),
        };
        let factory = BackendFactory::new();
        let backend = build_one(&factory, &endpoint, user_agent).expect("backend builds");
        let request = ChatCompletionRequest::new("m", vec![ChatMessage::user("hi")]);
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
