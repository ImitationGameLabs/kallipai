//! Profile endpoint probing: POST /profiles/probe.
//!
//! A read-only diagnostics domain for the manage UI's Test buttons. The
//! caller submits endpoint definitions **inline** (draft config, before
//! save) or references live endpoints by id; the tagma builds throwaway
//! backends and exercises the zero-cost capability probes upstream offers
//! (`ModelCatalog::list_models`, `Balance::get_balance`). Nothing is
//! persisted and the live registry (`ArcSwap`) is untouched. Provider tests
//! stay zero-cost, but a tier profile that survives its catalog pre-check
//! is verified with one minimal, billed chat completion (test prompt,
//! 256-token cap) — that call is the profile Test's real verdict.
//!
//! Statuses are layered so the UI can tell apart the failure classes an
//! operator can act on differently: `invalid_config` (fix the definition),
//! `unreachable` (network/endpoint down), `unauthorized` (credential), and
//! `model_missing`-style catalog mismatch (checked per tier profile).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use just_llm_client::client::BackendFactory;
use just_llm_client::types::chat::{ChatCompletionRequest, ChatMessage};
use kallip_runtime::profile::{ProfileConfig, Provider};
use serde::{Deserialize, Serialize};

use crate::backend::{self, DEFAULT_USER_AGENT};
use crate::state::SharedState;

/// Per-provider probe budget. Generous for slow cold starts, tight enough
/// that a wedged provider cannot pin the request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on inline providers per request: every provider fans out one
/// concurrent outbound connection, so an unbounded list is a self-DoS lever
/// on an operator-only route.
pub(crate) const MAX_PROBE_PROVIDERS: usize = 64;
/// Upper bound on tier profiles per request: every profile that survives the
/// catalog pre-check fans out one real (billed) inference call, so an
/// unbounded list is a spend lever on an operator-only route.
pub(crate) const MAX_PROBE_PROFILES: usize = 64;

/// Per-profile inference budget. A non-streaming 256-token generation can
/// legitimately run past PROBE_TIMEOUT; reusing it would misreport slow
/// models as unreachable.
const INFERENCE_TIMEOUT: Duration = Duration::from_secs(60);

/// The one user message a profile Test sends. Announces itself as a test so
/// provider-side logs are self-explanatory, and asks for brevity.
const INFERENCE_TEST_PROMPT: &str = "This is a one-shot connectivity test of this model from the kallip management UI. Reply with a single short sentence confirming that you respond.";

#[derive(Deserialize)]
pub struct ProbeRequest {
    /// Inline provider definitions (draft config). `api_key: null` means
    /// "reuse the live key stored for this provider id" — the same draft
    /// semantics the masked PUT uses, so an unchanged key never needs to be
    /// sent back up.
    #[serde(default)]
    pub endpoints: Vec<ProbeProvider>,
    /// Tiers of profiles to check model names against the fetched catalogs.
    #[serde(default)]
    pub tiers: Vec<ProbeTier>,
}

#[derive(Deserialize)]
pub struct ProbeProvider {
    pub id: String,
    pub family: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Deserialize)]
pub struct ProbeTier {
    pub profiles: Vec<ProbeProfile>,
}

#[derive(Deserialize)]
pub struct ProbeProfile {
    pub id: String,
    /// Provider id this profile connects through (inline or live).
    pub endpoint: String,
    pub model: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// Catalog (or balance) came back — provider is reachable and authorized.
    Ok,
    /// Transport-level failure or timeout — nothing HTTP-shaped responded.
    Unreachable,
    /// HTTP 401/403 — the credential was rejected.
    Unauthorized,
    /// The definition failed backend construction, the provider answered
    /// with an unexpected HTTP status (404/429/5xx on the probe path), or a
    /// tier profile's model is absent from the provider's catalog.
    InvalidConfig,
    /// Provider responded, but the family offers no zero-cost probe
    /// capability — liveness could not be established without a chat call.
    Partial,
}

#[derive(Serialize)]
pub struct ProviderReport {
    pub endpoint_id: String,
    pub status: ProbeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_count: Option<usize>,
    /// Serialized `BalanceSnapshot` when the family supports balance and it
    /// was fetched successfully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Model ids from the catalog when fetched AND tier checks were
    /// requested (the model-name verification needs the full list).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct ProfileReport {
    pub profile_id: String,
    pub endpoint_id: String,
    pub status: ProbeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize)]
pub struct TierReport {
    pub index: usize,
    pub all_ok: bool,
    pub profiles: Vec<ProfileReport>,
}

#[derive(Serialize)]
pub struct ProbeResponse {
    pub results: Vec<ProviderReport>,
    pub tiers: Vec<TierReport>,
}

/// Run the full probe: resolve the provider set against the live config,
/// probe every endpoint concurrently, then settle tier profiles in two
/// stages (catalog pre-check, shared real inference). Bounds were already
/// enforced by the HTTP layer (`MAX_PROBE_PROVIDERS`/`MAX_PROBE_PROFILES`).
pub(crate) async fn run_probe(state: &SharedState, request: ProbeRequest) -> ProbeResponse {
    let live = state.profiles.load().config.clone();
    let wants_models = !request.tiers.is_empty();

    // Resolve the provider set: inline definitions win; tiers may reference
    // provider ids not submitted inline — those resolve to live definitions.
    let mut defs: HashMap<String, Provider> = HashMap::new();
    let mut reports: Vec<ProviderReport> = Vec::new();
    for probe_ep in &request.endpoints {
        match resolve_provider(probe_ep, &live) {
            Ok(def) => {
                defs.insert(def.id.clone(), def);
            }
            Err(detail) => reports.push(invalid_config_report(probe_ep.id.clone(), detail)),
        }
    }
    let mut referenced: Vec<String> = Vec::new();
    for tier in &request.tiers {
        for profile in &tier.profiles {
            if !defs.contains_key(&profile.endpoint)
                && !reports.iter().any(|r| r.endpoint_id == profile.endpoint)
                && !referenced.contains(&profile.endpoint)
            {
                match live.endpoints.get(&profile.endpoint) {
                    Some(def) => {
                        defs.insert(def.id.clone(), def.clone());
                    }
                    None => referenced.push(profile.endpoint.clone()),
                }
            }
        }
    }
    for id in referenced {
        reports.push(invalid_config_report(
            id,
            "referenced provider is neither inline nor live".to_string(),
        ));
    }

    // Probe all resolved providers concurrently; each is independent.
    // The inference stage below needs the definitions after this join_all
    // consumes `defs` (into_values moves them out).
    let inference_pool: HashMap<String, Provider> = defs.clone();
    let factory = BackendFactory::new();
    let mut probed = join_all(
        defs.into_values()
            .map(|def| probe_one(&factory, def, wants_models)),
    )
    .await;
    reports.append(&mut probed);

    // Tier checks run in two stages. Stage 1 settles every profile the
    // per-provider reports already condemn (failed endpoint, catalog miss);
    // catalog hits and catalog-less ok providers defer to stage 2 — one
    // minimal real inference per unique (provider, model), shared by every
    // profile that referenced the pair.
    let by_id: HashMap<&str, &ProviderReport> = reports
        .iter()
        .map(|r| (r.endpoint_id.as_str(), r))
        .collect();
    let mut settled: Vec<Vec<Option<ProfileReport>>> = request
        .tiers
        .iter()
        .map(|t| (0..t.profiles.len()).map(|_| None).collect::<Vec<_>>())
        .collect();
    // One deferred (provider, model) pair and the tier/profile coordinates
    // waiting on its shared inference verdict.
    struct PendingInference {
        endpoint_id: String,
        model: String,
        coords: Vec<(usize, usize)>,
    }
    let mut pending: Vec<PendingInference> = Vec::new();
    for (t_idx, tier) in request.tiers.iter().enumerate() {
        for (p_idx, p) in tier.profiles.iter().enumerate() {
            match catalog_stage(p, by_id.get(p.endpoint.as_str()).copied()) {
                CatalogStage::Settled(report) => settled[t_idx][p_idx] = Some(report),
                CatalogStage::DeferInference => {
                    match pending
                        .iter_mut()
                        .find(|job| job.endpoint_id == p.endpoint && job.model == p.model)
                    {
                        Some(job) => job.coords.push((t_idx, p_idx)),
                        None => pending.push(PendingInference {
                            endpoint_id: p.endpoint.clone(),
                            model: p.model.clone(),
                            coords: vec![(t_idx, p_idx)],
                        }),
                    }
                }
            }
        }
    }

    let jobs = pending.into_iter().map(|job| {
        let def = inference_pool.get(&job.endpoint_id).cloned();
        let factory = &factory;
        async move {
            let verdict = match def {
                Some(def) => inference_verdict(factory, &def, &job.model, INFERENCE_TIMEOUT).await,
                // Unreachable in practice: deferring requires an ok endpoint
                // report, which only resolved providers produce.
                None => (
                    ProbeStatus::InvalidConfig,
                    Some("provider was not probed".to_string()),
                ),
            };
            (verdict, job.coords)
        }
    });
    for ((status, detail), coords) in join_all(jobs).await {
        for (t_idx, p_idx) in coords {
            let p = &request.tiers[t_idx].profiles[p_idx];
            settled[t_idx][p_idx] = Some(ProfileReport {
                profile_id: p.id.clone(),
                endpoint_id: p.endpoint.clone(),
                status,
                detail: detail.clone(),
            });
        }
    }

    let tiers = request
        .tiers
        .iter()
        .zip(settled)
        .enumerate()
        .map(|(index, (tier, reports))| {
            let profiles = tier
                .profiles
                .iter()
                .zip(reports)
                .map(|(p, report)| report.unwrap_or_else(|| not_probed_report(p)))
                .collect::<Vec<_>>();
            let all_ok = profiles.iter().all(|p| p.status == ProbeStatus::Ok);
            TierReport {
                index,
                all_ok,
                profiles,
            }
        })
        .collect();

    ProbeResponse {
        results: reports,
        tiers,
    }
}
fn invalid_config_report(endpoint_id: String, detail: String) -> ProviderReport {
    ProviderReport {
        endpoint_id,
        status: ProbeStatus::InvalidConfig,
        latency_ms: None,
        catalog_count: None,
        balance: None,
        detail: Some(detail),
        models: None,
    }
}
/// Outcome of the catalog pre-check for one tier profile: the report is
/// either settled without spending tokens, or it defers to a real inference.
enum CatalogStage {
    Settled(ProfileReport),
    DeferInference,
}

/// A profile whose provider has no report to consult at all.
fn not_probed_report(p: &ProbeProfile) -> ProfileReport {
    ProfileReport {
        profile_id: p.id.clone(),
        endpoint_id: p.endpoint.clone(),
        status: ProbeStatus::InvalidConfig,
        detail: Some("provider was not probed".to_string()),
    }
}

/// Stage 1 of tier checking — settle what the per-endpoint reports already
/// decide, for free: a failed provider propagates its status, a catalog miss
/// settles as invalid_config, and a catalog hit (or a catalog-less-but-ok
/// endpoint) defers to a real inference, which is the profile Test's verdict.
fn catalog_stage(p: &ProbeProfile, ep: Option<&ProviderReport>) -> CatalogStage {
    let Some(ep) = ep else {
        return CatalogStage::Settled(not_probed_report(p));
    };
    match ep.status {
        ProbeStatus::Ok => match &ep.models {
            Some(models) if models.iter().any(|m| m == &p.model) => CatalogStage::DeferInference,
            Some(models) => CatalogStage::Settled(ProfileReport {
                profile_id: p.id.clone(),
                endpoint_id: p.endpoint.clone(),
                status: ProbeStatus::InvalidConfig,
                detail: Some(format!(
                    "model '{}' not in provider catalog ({} models)",
                    p.model,
                    models.len()
                )),
            }),
            // Catalog-less ok (balance-only probe): inference is the only judge.
            None => CatalogStage::DeferInference,
        },
        other => CatalogStage::Settled(ProfileReport {
            profile_id: p.id.clone(),
            endpoint_id: p.endpoint.clone(),
            status: other,
            detail: None,
        }),
    }
}

/// Stage 2 of tier checking — the minimal real inference behind a profile's
/// Test button: one non-streaming chat completion with a fixed test prompt
/// and a 256-token cap. `timeout` is a parameter so tests can exercise the
/// deadline without waiting out the production budget.
async fn inference_verdict(
    factory: &BackendFactory,
    def: &Provider,
    model: &str,
    timeout: Duration,
) -> (ProbeStatus, Option<String>) {
    let backend = match backend::build_one(factory, def, DEFAULT_USER_AGENT) {
        Ok(backend) => backend,
        Err(e) => return (ProbeStatus::InvalidConfig, Some(format!("{e:#}"))),
    };
    let request = ChatCompletionRequest::new(
        model.to_string(),
        vec![ChatMessage::user(INFERENCE_TEST_PROMPT)],
    )
    .with_max_tokens(256);
    match tokio::time::timeout(timeout, backend.chat_completion(request)).await {
        Ok(Ok(_)) => (ProbeStatus::Ok, None),
        Ok(Err(e)) => {
            let (status, detail) = classify_backend_error(&e);
            (status, Some(detail))
        }
        Err(_) => (
            ProbeStatus::Unreachable,
            Some(format!("inference timed out after {timeout:?}")),
        ),
    }
}

/// The masked form of an API key (`first4********last4`; eight fixed stars
/// when the key is too short — the count never leaks its length). An echoed
/// mask in a probe or PUT body means "keep the live key"; GET returns it.
pub(crate) fn mask_key(key: &str) -> String {
    const STARS: &str = "********";
    let chars: Vec<char> = key.chars().collect();
    match chars.len() {
        0 => String::new(),
        1..=8 => STARS.to_string(),
        _ => {
            let head: String = chars[..4].iter().collect();
            let tail: String = chars[chars.len() - 4..].iter().collect();
            format!("{head}{STARS}{tail}")
        }
    }
}

/// Merge an inline probe definition with the live config: a null `api_key` or
/// null `base_url` means "keep the live value for this provider id".
fn resolve_provider(probe: &ProbeProvider, live: &ProfileConfig) -> Result<Provider, String> {
    let api_key = match &probe.api_key {
        Some(key) => match live.endpoints.get(&probe.id) {
            // The masked form echoed back (GET → draft → probe) means "keep",
            // never a credential to dial out with.
            Some(ep) if mask_key(&ep.api_key) == *key => ep.api_key.clone(),
            _ => key.clone(),
        },
        None => live
            .endpoints
            .get(&probe.id)
            .map(|ep| ep.api_key.clone())
            .ok_or_else(|| {
                format!(
                    "no api_key given and no live provider '{}' to take one from",
                    probe.id
                )
            })?,
    };
    Ok(Provider {
        id: probe.id.clone(),
        family: probe.family.clone(),
        api_key,
        base_url: probe.base_url.clone().or_else(|| {
            live.endpoints
                .get(&probe.id)
                .and_then(|ep| ep.base_url.clone())
        }),
    })
}

/// Build a throwaway backend and run the zero-cost probes, classifying the
/// outcome. `wants_models` keeps the catalog ids for tier model checks.
async fn probe_one(factory: &BackendFactory, def: Provider, wants_models: bool) -> ProviderReport {
    let endpoint_id = def.id.clone();
    let mut report = ProviderReport {
        endpoint_id,
        status: ProbeStatus::InvalidConfig,
        latency_ms: None,
        catalog_count: None,
        balance: None,
        detail: None,
        models: None,
    };

    let backend = match backend::build_one(factory, &def, DEFAULT_USER_AGENT) {
        Ok(backend) => backend,
        Err(e) => {
            report.detail = Some(format!("{e:#}"));
            return report;
        }
    };

    let started = Instant::now();
    let catalog = match tokio::time::timeout(PROBE_TIMEOUT, catalog_ids(&backend)).await {
        Err(_) => {
            report.status = ProbeStatus::Unreachable;
            report.detail = Some(format!(
                "catalog probe timed out after {}s",
                PROBE_TIMEOUT.as_secs()
            ));
            report.latency_ms = Some(started.elapsed().as_millis() as u64);
            return report;
        }
        Ok(Err((status, detail))) => {
            report.status = status;
            report.detail = Some(detail);
            report.latency_ms = Some(started.elapsed().as_millis() as u64);
            return report;
        }
        Ok(Ok(ids)) => ids,
    };
    report.latency_ms = Some(started.elapsed().as_millis() as u64);

    match catalog {
        Some(ids) => {
            report.status = ProbeStatus::Ok;
            report.catalog_count = Some(ids.len());
            if wants_models {
                report.models = Some(ids);
            }
        }
        None => {
            // No catalog capability: fall back to balance as the liveness probe.
            match tokio::time::timeout(PROBE_TIMEOUT, balance_value(&backend)).await {
                Ok(Ok(Some(value))) => {
                    report.status = ProbeStatus::Ok;
                    report.balance = Some(value);
                }
                Ok(Ok(None)) => {
                    report.status = ProbeStatus::Partial;
                    report.detail =
                        Some("family offers no model catalog or balance probe".to_string());
                }
                Ok(Err((status, detail))) => {
                    report.status = status;
                    report.detail = Some(detail);
                }
                Err(_) => {
                    report.status = ProbeStatus::Unreachable;
                    report.detail = Some(format!(
                        "balance probe timed out after {}s",
                        PROBE_TIMEOUT.as_secs()
                    ));
                }
            }
        }
    }
    report
}

/// Fetch the model catalog ids; `Ok(None)` = capability unsupported (not an
/// error — the family simply has nothing to probe with).
async fn catalog_ids(
    backend: &Arc<dyn just_llm_client::LlmBackend>,
) -> Result<Option<Vec<String>>, (ProbeStatus, String)> {
    let handle = match backend.model_catalog() {
        Ok(handle) => handle,
        Err(_) => return Ok(None),
    };
    match handle.list_models().await {
        Ok(catalog) => Ok(Some(catalog.data.into_iter().map(|m| m.id).collect())),
        Err(e) => Err(classify_backend_error(&e)),
    }
}

/// Fetch the balance snapshot serialized to JSON; `Ok(None)` = unsupported.
async fn balance_value(
    backend: &Arc<dyn just_llm_client::LlmBackend>,
) -> Result<Option<serde_json::Value>, (ProbeStatus, String)> {
    let handle = match backend.balance() {
        Ok(handle) => handle,
        Err(_) => return Ok(None),
    };
    match handle.get_balance().await {
        Ok(snapshot) => serde_json::to_value(snapshot).map(Some).map_err(|e| {
            (
                ProbeStatus::Partial,
                format!("balance serialize failed: {e}"),
            )
        }),
        Err(e) => Err(classify_backend_error(&e)),
    }
}

/// Map a backend capability error onto the probe status taxonomy. The HTTP
/// status, when present, is recovered from the error's source chain — the
/// same downcast pattern the runtime's `llm_error::extract_http_body` uses —
/// while transport-layer InvalidConfig and unparseable 2xx bodies classify
/// as invalid_config before the status match, since neither implies
/// unreachability.
fn classify_backend_error(e: &just_llm_client::BackendError) -> (ProbeStatus, String) {
    let status = http_status_of(e);
    let detail = format!("{e:#}");
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(error) = current {
        if let Some(transport) = error.downcast_ref::<just_llm_client::TransportError>()
            && matches!(transport, just_llm_client::TransportError::InvalidConfig(_))
        {
            return (ProbeStatus::InvalidConfig, detail);
        }
        if let Some(provider) = error.downcast_ref::<just_llm_client::ProviderError>()
            && matches!(provider, just_llm_client::ProviderError::Deserialize { .. })
        {
            return (ProbeStatus::InvalidConfig, detail);
        }
        current = error.source();
    }
    match status {
        Some(code) if code.as_u16() == 401 || code.as_u16() == 403 => {
            (ProbeStatus::Unauthorized, detail)
        }
        // The provider answered HTTP — reachable, but the probe path itself
        // failed (404 on /models, 429, 5xx, ...). InvalidConfig is the
        // closest actionable bucket: inspect the definition/provider.
        Some(_) => (ProbeStatus::InvalidConfig, detail),
        None => (ProbeStatus::Unreachable, detail),
    }
}

/// Walk the error source chain for a `TransportError::HttpStatus`.
fn http_status_of(e: &(dyn std::error::Error + 'static)) -> Option<reqwest::StatusCode> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(error) = current {
        if let Some(transport) = error.downcast_ref::<just_llm_client::TransportError>()
            && let just_llm_client::TransportError::HttpStatus { status, .. } = transport
        {
            return Some(*status);
        }
        current = error.source();
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::make_state;

    #[test]
    fn masked_key_echo_resolves_to_live_key() {
        // GET /profiles returns masked keys; a draft probed unchanged echoes the
        // mask back. That must resolve to the live key, not probe with the mask.
        let live = make_state().profiles.load().config.clone();
        let masked = mask_key(&live.endpoints["test"].api_key);
        let probe_ep = ProbeProvider {
            id: "test".into(),
            family: "deepseek".into(),
            api_key: Some(masked),
            base_url: None,
        };
        let resolved = resolve_provider(&probe_ep, &live).expect("masked echo resolves");
        assert_eq!(resolved.api_key, live.endpoints["test"].api_key);
    }

    fn provider_report(status: ProbeStatus, models: Option<Vec<String>>) -> ProviderReport {
        ProviderReport {
            endpoint_id: "test".into(),
            status,
            latency_ms: None,
            models,
            balance: None,
            catalog_count: None,
            detail: None,
        }
    }

    fn probe_profile(model: &str) -> ProbeProfile {
        ProbeProfile {
            id: "p".into(),
            endpoint: "test".into(),
            model: model.into(),
        }
    }

    #[test]
    fn catalog_stage_settles_miss_and_defers_hit() {
        // Catalog hit: nothing settled yet — the real inference decides.
        match catalog_stage(
            &probe_profile("m1"),
            Some(&provider_report(
                ProbeStatus::Ok,
                Some(vec!["m1".into(), "m2".into()]),
            )),
        ) {
            CatalogStage::DeferInference => {}
            CatalogStage::Settled(_) => panic!("catalog hit must defer to inference"),
        }

        // Catalog miss: settled invalid_config without spending tokens.
        let miss = match catalog_stage(
            &probe_profile("nope"),
            Some(&provider_report(ProbeStatus::Ok, Some(vec!["m1".into()]))),
        ) {
            CatalogStage::Settled(report) => report,
            CatalogStage::DeferInference => panic!("catalog miss must settle"),
        };
        assert_eq!(miss.status, ProbeStatus::InvalidConfig);
        assert!(
            miss.detail
                .as_deref()
                .unwrap()
                .contains("not in provider catalog")
        );
    }

    #[test]
    fn catalog_less_ok_provider_defers_inference() {
        // A balance-only ok endpoint has no catalog to check against; the
        // inference is the profile's only judge.
        match catalog_stage(
            &probe_profile("any"),
            Some(&provider_report(ProbeStatus::Ok, None)),
        ) {
            CatalogStage::DeferInference => {}
            CatalogStage::Settled(_) => panic!("catalog-less ok provider must defer to inference"),
        }
    }

    #[test]
    fn catalog_stage_inherits_provider_failure() {
        let r = match catalog_stage(
            &probe_profile("m1"),
            Some(&provider_report(ProbeStatus::Unauthorized, None)),
        ) {
            CatalogStage::Settled(report) => report,
            CatalogStage::DeferInference => panic!("failed endpoint must settle"),
        };
        assert_eq!(r.status, ProbeStatus::Unauthorized);
    }

    #[test]
    fn catalog_stage_without_provider_report_is_invalid() {
        let r = match catalog_stage(&probe_profile("m1"), None) {
            CatalogStage::Settled(report) => report,
            CatalogStage::DeferInference => panic!("missing endpoint report must settle"),
        };
        assert_eq!(r.status, ProbeStatus::InvalidConfig);
    }

    fn backend_error_with_transport(
        transport: just_llm_client::TransportError,
    ) -> just_llm_client::BackendError {
        just_llm_client::BackendError::provider(
            "deepseek",
            just_llm_client::ProviderError::Transport(transport),
        )
    }

    #[test]
    fn classify_http_401_is_unauthorized() {
        let e = backend_error_with_transport(just_llm_client::TransportError::HttpStatus {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "bad key".into(),
        });
        assert_eq!(classify_backend_error(&e).0, ProbeStatus::Unauthorized);
    }

    #[test]
    fn classify_transport_invalid_config_is_invalid_config() {
        let e = backend_error_with_transport(just_llm_client::TransportError::InvalidConfig(
            "invalid base URL",
        ));
        assert_eq!(classify_backend_error(&e).0, ProbeStatus::InvalidConfig);
    }

    #[test]
    fn classify_unparseable_2xx_body_is_invalid_config() {
        let e = just_llm_client::BackendError::provider(
            "deepseek",
            just_llm_client::ProviderError::Deserialize {
                source: serde_json::from_str::<serde_json::Value>("nope").unwrap_err(),
                body: "<html>hi</html>".into(),
            },
        );
        assert_eq!(classify_backend_error(&e).0, ProbeStatus::InvalidConfig);
    }

    #[test]
    fn resolve_provider_null_base_url_merges_live() {
        let mut live = make_state().profiles.load().config.clone();
        live.endpoints.get_mut("test").unwrap().base_url = Some("https://live.example".into());
        let probe_ep = ProbeProvider {
            id: "test".into(),
            family: "deepseek".into(),
            api_key: None,
            base_url: None,
        };
        let resolved = resolve_provider(&probe_ep, &live).unwrap();
        assert_eq!(resolved.base_url.as_deref(), Some("https://live.example"));
    }

    /// Minimal legal openai-shaped chat completion body.
    fn chat_ok_body() -> serde_json::Value {
        serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "m1",
        "choices": [{
        "index": 0,
        "message": { "role": "assistant", "content": "ok" }
        }]
        })
    }
    #[tokio::test]
    async fn inference_verdict_times_out() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(chat_ok_body())
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;

        let provider = Provider {
            id: "mock".into(),
            family: "openai-compatible".into(),
            api_key: "sk-test".into(),
            base_url: Some(server.uri()),
        };
        let (status, detail) = inference_verdict(
            &BackendFactory::new(),
            &provider,
            "m1",
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(status, ProbeStatus::Unreachable);
        assert!(detail.as_deref().unwrap().contains("timed out"));
    }
}
