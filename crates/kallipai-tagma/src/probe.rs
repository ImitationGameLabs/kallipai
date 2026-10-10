//! Profile endpoint probing: POST /profiles/probe.
//!
//! A read-only diagnostics domain for the manage UI's Test buttons. The
//! caller submits endpoint definitions **inline** (draft config, before
//! save) or references live endpoints by id; the tagma builds throwaway
//! backends and exercises the zero-cost capability probes upstream offers
//! (`ModelCatalog::list_models`, `Balance::get_balance`). Nothing is
//! persisted and the live registry (`ArcSwap`) is untouched. Provider tests
//! stay zero-cost, but every set profile is verified with one minimal,
//! billed chat completion (test prompt, 256-token cap) — that call alone is
//! the profile Test's verdict; the model list is provider-level information.
//!
//! Statuses are layered so the UI can tell apart the failure classes an
//! operator can act on differently: `invalid_config` (fix the definition),
//! `upstream_status` (the provider answered HTTP but the probe path failed:
//! fix the list address or retry later), `unreachable` (network/endpoint
//! down), `unauthorized` (credential).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use just_llm_client::client::BackendFactory;
use just_llm_client::types::generation::{GenerationRequest, Message};
use kallipai_adk::profile::{ProfileConfig, Provider};
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
/// Upper bound on set profiles per request: every profile fans out one real
/// (billed) inference call (the model list does not gate it), so an
/// unbounded list is a spend lever on an operator-only route.
pub(crate) const MAX_PROBE_PROFILES: usize = 64;

/// Per-profile inference budget. A non-streaming 256-token generation can
/// legitimately run past PROBE_TIMEOUT; reusing it would misreport slow
/// models as unreachable.
const INFERENCE_TIMEOUT: Duration = Duration::from_secs(60);

/// The one user message a profile Test sends. Announces itself as a test so
/// provider-side logs are self-explanatory, and asks for brevity.
const INFERENCE_TEST_PROMPT: &str = "This is a one-shot connectivity test of this model from the kallipai management UI. Reply with a single short sentence confirming that you respond.";

#[derive(Deserialize)]
pub struct ProbeRequest {
    /// Inline provider definitions (draft config). `api_key: null` means
    /// "reuse the live key stored for this provider id" — the same draft
    /// semantics the masked PUT uses, so an unchanged key never needs to be
    /// sent back up.
    #[serde(default)]
    pub endpoints: Vec<ProbeProvider>,
    /// Named sets of profiles to check model names against the fetched
    /// model lists.
    #[serde(default)]
    pub sets: Vec<ProbeSet>,
}

#[derive(Deserialize)]
pub struct ProbeProvider {
    pub id: String,
    pub family: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// Tri-state, as the PUT body defines it: absent keeps the live value,
    /// an empty string clears it (the rule both bases share).
    pub model_list_base_url: Option<String>,
}

#[derive(Deserialize)]
pub struct ProbeSet {
    pub name: String,
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
    /// The model list (or balance) came back — reachable and authorized.
    Ok,
    /// Transport-level failure or timeout — nothing HTTP-shaped responded.
    Unreachable,
    /// HTTP 401/403 — the credential was rejected.
    Unauthorized,
    /// The definition failed backend construction, or a 2xx body could not
    /// be parsed — the local side of the probe.
    InvalidConfig,
    /// The provider answered HTTP, but the probe path itself failed (404 on
    /// the model list, 429, 5xx). The status rides along so the UI can tell
    /// a wrong list address from a rate limit from an upstream fault.
    UpstreamStatus { status: u16 },
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
    pub models_count: Option<usize>,
    /// Serialized `BalanceSnapshot` when the family supports balance and it
    /// was fetched successfully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Model ids from the list when fetched AND set checks were
    /// requested (the model-name verification needs the full list).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

/// The single advisory a profile report can carry: the provider's model list
/// was fetched and does not name the model. The verdict itself always comes
/// from the real call — a list that is absent, or unreachable, says nothing
/// either way and stays silent here.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelsHint {
    /// The list was fetched and does not name the model.
    NotListed,
}

#[derive(Serialize)]
pub struct ProfileReport {
    pub profile_id: String,
    pub endpoint_id: String,
    pub status: ProbeStatus,
    /// Advisory, never a verdict: the provider's model list was fetched and
    /// does not name this model. Absent when the list says nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_hint: Option<ModelsHint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize)]
pub struct SetReport {
    pub name: String,
    pub all_ok: bool,
    pub profiles: Vec<ProfileReport>,
}

#[derive(Serialize)]
pub struct ProbeResponse {
    pub results: Vec<ProviderReport>,
    pub sets: Vec<SetReport>,
}

/// Run the full probe: resolve the provider set against the live config,
/// probe every endpoint concurrently, then settle set profiles in two
/// stages (model-list advice, shared real inference). Bounds were already
/// enforced by the HTTP layer (`MAX_PROBE_PROVIDERS`/`MAX_PROBE_PROFILES`).
pub(crate) async fn run_probe(state: &SharedState, request: ProbeRequest) -> ProbeResponse {
    let live = state.profiles.load().config.clone();
    let wants_models = !request.sets.is_empty();

    // Resolve the provider set: inline definitions win; sets may reference
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
    for set in &request.sets {
        for profile in &set.profiles {
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

    // Set checks: the model list is advice, the real call is the verdict.
    // Stage 1 settles only what config alone condemns (an unresolvable
    // provider) and records the list's opinion as a hint; stage 2 runs one
    // minimal real inference per unique (provider, model), shared by every
    // profile that referenced the pair.
    let by_id: HashMap<&str, &ProviderReport> = reports
        .iter()
        .map(|r| (r.endpoint_id.as_str(), r))
        .collect();
    let mut settled: Vec<Vec<Option<ProfileReport>>> = request
        .sets
        .iter()
        .map(|t| (0..t.profiles.len()).map(|_| None).collect::<Vec<_>>())
        .collect();
    // One deferred (provider, model) pair and the set/profile coordinates
    // waiting on its shared inference verdict.
    struct PendingInference {
        endpoint_id: String,
        model: String,
        hint: Option<ModelsHint>,
        coords: Vec<(usize, usize)>,
    }
    let mut pending: Vec<PendingInference> = Vec::new();
    for (t_idx, set) in request.sets.iter().enumerate() {
        for (p_idx, p) in set.profiles.iter().enumerate() {
            let resolvable = inference_pool.contains_key(p.endpoint.as_str());
            match models_stage(p, by_id.get(p.endpoint.as_str()).copied(), resolvable) {
                ModelsStage::Settled(report) => settled[t_idx][p_idx] = Some(report),
                ModelsStage::Assess(hint) => {
                    match pending
                        .iter_mut()
                        .find(|job| job.endpoint_id == p.endpoint && job.model == p.model)
                    {
                        Some(job) => job.coords.push((t_idx, p_idx)),
                        None => pending.push(PendingInference {
                            endpoint_id: p.endpoint.clone(),
                            model: p.model.clone(),
                            hint,
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
                // A profile reaches the real call when its provider is a pool
                // member; a missing definition means that check condemned it.
                None => (
                    ProbeStatus::InvalidConfig,
                    Some("provider was not probed".to_string()),
                ),
            };
            (verdict, job.coords, job.hint)
        }
    });
    for ((status, detail), coords, hint) in join_all(jobs).await {
        for (t_idx, p_idx) in coords {
            let p = &request.sets[t_idx].profiles[p_idx];
            settled[t_idx][p_idx] = Some(ProfileReport {
                profile_id: p.id.clone(),
                endpoint_id: p.endpoint.clone(),
                status,
                models_hint: hint,
                detail: detail.clone(),
            });
        }
    }

    let sets = request
        .sets
        .iter()
        .zip(settled)
        .map(|(set, reports)| {
            let profiles = set
                .profiles
                .iter()
                .zip(reports)
                .map(|(p, report)| report.unwrap_or_else(|| not_probed_report(p)))
                .collect::<Vec<_>>();
            let all_ok = profiles.iter().all(|p| p.status == ProbeStatus::Ok);
            SetReport {
                name: set.name.clone(),
                all_ok,
                profiles,
            }
        })
        .collect();

    ProbeResponse {
        results: reports,
        sets,
    }
}
fn invalid_config_report(endpoint_id: String, detail: String) -> ProviderReport {
    ProviderReport {
        endpoint_id,
        status: ProbeStatus::InvalidConfig,
        latency_ms: None,
        models_count: None,
        balance: None,
        detail: Some(detail),
        models: None,
    }
}
/// What the model list says about one set profile, and what happens next:
/// the list never settles a verdict — only an unresolvable provider does —
/// so a profile either carries a hint into the real call, or is settled as a
/// config error without spending tokens.
enum ModelsStage {
    Settled(ProfileReport),
    Assess(Option<ModelsHint>),
}

/// A profile whose provider has no report to consult at all.
fn not_probed_report(p: &ProbeProfile) -> ProfileReport {
    ProfileReport {
        profile_id: p.id.clone(),
        endpoint_id: p.endpoint.clone(),
        status: ProbeStatus::InvalidConfig,
        detail: Some("provider was not probed".to_string()),
        models_hint: None,
    }
}

/// Stage 1 of set checking — the model list's say, which is advice only: a
/// provider that never resolved settles as a config error, everything else
/// goes to the real call with a hint when the fetched list does not name the
/// model. The call is the profile Test's verdict.
fn models_stage(p: &ProbeProfile, ep: Option<&ProviderReport>, resolvable: bool) -> ModelsStage {
    if !resolvable {
        return ModelsStage::Settled(not_probed_report(p));
    }
    let not_listed = ep
        .and_then(|ep| ep.models.as_ref())
        .is_some_and(|models| !models.iter().any(|m| m == &p.model));
    ModelsStage::Assess(not_listed.then_some(ModelsHint::NotListed))
}

/// Stage 2 of set checking — the minimal real inference behind a profile's
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
    let request = GenerationRequest::new(
        model.to_string(),
        vec![Message::user(INFERENCE_TEST_PROMPT)],
    )
    .with_max_tokens(256);
    match tokio::time::timeout(timeout, backend.generate(request)).await {
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

/// One base field of an inline draft against the live value: absent keeps,
/// an empty string clears (never a live fallback — the operator meant it),
/// and anything else replaces.
fn resolve_base(provided: Option<&String>, live: Option<&String>) -> Option<String> {
    match provided {
        None => live.cloned(),
        Some(url) if url.is_empty() => None,
        Some(url) => Some(url.clone()),
    }
}
/// Merge an inline probe definition with the live config. Both bases follow
/// the tri-state rule the PUT path owns (`merge_wire`): absent keeps the
/// live value, an empty string clears it, a value replaces.
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
        base_url: resolve_base(
            probe.base_url.as_ref(),
            live.endpoints
                .get(&probe.id)
                .and_then(|ep| ep.base_url.as_ref()),
        ),
        model_list_base_url: resolve_base(
            probe.model_list_base_url.as_ref(),
            live.endpoints
                .get(&probe.id)
                .and_then(|ep| ep.model_list_base_url.as_ref()),
        ),
    })
}

/// Build a throwaway backend and run the zero-cost probes, classifying the
/// outcome. `wants_models` keeps the model ids for set model checks.
async fn probe_one(factory: &BackendFactory, def: Provider, wants_models: bool) -> ProviderReport {
    let endpoint_id = def.id.clone();
    let mut report = ProviderReport {
        endpoint_id,
        status: ProbeStatus::InvalidConfig,
        latency_ms: None,
        models_count: None,
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
    // The library derives both `/models` and `/chat/completions` from one base,
    // so a provider whose model list lives elsewhere (a gateway that lists
    // models on another path) cannot say so through `base_url` alone: a second
    // throwaway backend — same family and key, the models base — carries the
    // list call. No change to the external client dependency is involved.
    let models_backend = match def.model_list_base_url.as_deref().filter(|b| !b.is_empty()) {
        Some(base) => {
            let models_def = Provider {
                base_url: Some(base.to_string()),
                model_list_base_url: None,
                ..def.clone()
            };
            match backend::build_one(factory, &models_def, DEFAULT_USER_AGENT) {
                Ok(backend) => backend,
                Err(e) => {
                    report.detail = Some(format!("{e:#}"));
                    return report;
                }
            }
        }
        // Same base: the primary backend already is the list backend.
        None => backend.clone(),
    };

    let started = Instant::now();
    let models = match tokio::time::timeout(PROBE_TIMEOUT, models_ids(&models_backend)).await {
        Err(_) => {
            report.status = ProbeStatus::Unreachable;
            report.detail = Some(format!(
                "model list probe timed out after {}s",
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

    match models {
        Some(ids) => {
            report.status = ProbeStatus::Ok;
            report.models_count = Some(ids.len());
            if wants_models {
                report.models = Some(ids);
            }
        }
        None => {
            // No model list capability: fall back to balance as the liveness probe.
            match tokio::time::timeout(PROBE_TIMEOUT, balance_value(&backend)).await {
                Ok(Ok(Some(value))) => {
                    report.status = ProbeStatus::Ok;
                    report.balance = Some(value);
                }
                Ok(Ok(None)) => {
                    report.status = ProbeStatus::Partial;
                    report.detail =
                        Some("family offers no model list or balance probe".to_string());
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

/// Fetch the provider's model ids; `Ok(None)` = capability unsupported (not
/// an error — the family simply has nothing to probe with).
async fn models_ids(
    backend: &Arc<dyn just_llm_client::LlmBackend>,
) -> Result<Option<Vec<String>>, (ProbeStatus, String)> {
    let handle = match backend.model_catalog() {
        Ok(handle) => handle,
        Err(_) => return Ok(None),
    };
    match handle.list_models().await {
        Ok(list) => Ok(Some(list.data.into_iter().map(|m| m.id).collect())),
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
        // failed (404 on /models, 429, 5xx, ...). The status is the
        // actionable part: it separates a wrong list address from a rate
        // limit from an upstream fault.
        Some(code) => (
            ProbeStatus::UpstreamStatus {
                status: code.as_u16(),
            },
            detail,
        ),
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
            model_list_base_url: None,
        };
        let resolved = resolve_provider(&probe_ep, &live).expect("masked echo resolves");
        assert_eq!(resolved.api_key, live.endpoints["test"].api_key);
    }

    #[test]
    fn an_empty_probe_base_is_unset_and_never_inherits_live() {
        // The UI sends a cleared field as an empty string (the PUT body's
        // clearing form), so the probe must read it as unset — the family's
        // own endpoint decides — and never as "take the live value", which is
        // what an absent field means.
        let mut live = make_state().profiles.load().config.clone();
        {
            let entry = live.endpoints.get_mut("test").unwrap();
            entry.base_url = Some("https://live.example".into());
            entry.model_list_base_url = Some("https://live-list.example".into());
        }
        let cleared = ProbeProvider {
            id: "test".into(),
            family: "deepseek".into(),
            api_key: None,
            base_url: Some(String::new()),
            model_list_base_url: Some(String::new()),
        };
        let factory = BackendFactory::new();
        let resolved = resolve_provider(&cleared, &live).expect("empty bases resolve");
        assert_eq!(
            resolved.base_url, None,
            "a blank must not inherit the live base"
        );
        assert_eq!(resolved.model_list_base_url, None);
        // The build is the step a blank base used to fail.
        assert!(
            backend::build_one(&factory, &resolved, DEFAULT_USER_AGENT).is_ok(),
            "an empty base must build against the family endpoint"
        );

        // The same layout with nothing live to inherit stays a clean build.
        let mut bare = make_state().profiles.load().config.clone();
        bare.endpoints.get_mut("test").unwrap().base_url = None;
        bare.endpoints.get_mut("test").unwrap().model_list_base_url = None;
        let resolved = resolve_provider(&cleared, &bare).expect("empty bases resolve");
        assert_eq!(resolved.base_url, None);
        assert!(backend::build_one(&factory, &resolved, DEFAULT_USER_AGENT).is_ok());
    }

    fn provider_report(status: ProbeStatus, models: Option<Vec<String>>) -> ProviderReport {
        ProviderReport {
            endpoint_id: "test".into(),
            status,
            latency_ms: None,
            models,
            balance: None,
            models_count: None,
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
    fn models_stage_hints_without_settling() {
        // A list that names the model adds nothing: the real call decides.
        match models_stage(
            &probe_profile("m1"),
            Some(&provider_report(ProbeStatus::Ok, Some(vec!["m1".into()]))),
            true,
        ) {
            ModelsStage::Assess(None) => {}
            _ => panic!("a listed model must carry no hint"),
        }

        // A fetched list that does not name the model is advice, not a verdict.
        match models_stage(
            &probe_profile("nope"),
            Some(&provider_report(ProbeStatus::Ok, Some(vec!["m1".into()]))),
            true,
        ) {
            ModelsStage::Assess(Some(ModelsHint::NotListed)) => {}
            _ => panic!("an unlisted model carries the hint"),
        }

        // No list to consult (balance-only family): nothing to hint.
        match models_stage(
            &probe_profile("any"),
            Some(&provider_report(ProbeStatus::Ok, None)),
            true,
        ) {
            ModelsStage::Assess(None) => {}
            _ => panic!("no list means no hint"),
        }
    }

    #[test]
    fn models_stage_never_inherits_a_provider_verdict() {
        // A failed provider probe says nothing about whether a model answers:
        // the profile still goes to the real call, which judges it.
        match models_stage(
            &probe_profile("m1"),
            Some(&provider_report(ProbeStatus::Unauthorized, None)),
            true,
        ) {
            ModelsStage::Assess(None) => {}
            _ => panic!("a provider verdict must not settle the profile"),
        }
    }

    #[test]
    fn models_stage_settles_an_unresolvable_provider() {
        let r = match models_stage(&probe_profile("m1"), None, false) {
            ModelsStage::Settled(report) => report,
            _ => panic!("an unresolvable provider must settle"),
        };
        assert_eq!(r.status, ProbeStatus::InvalidConfig);
        assert_eq!(r.models_hint, None);
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
    fn classify_an_upstream_http_failure_carries_its_status() {
        for status in [404_u16, 429, 500] {
            let code = reqwest::StatusCode::from_u16(status).unwrap();
            let e = backend_error_with_transport(just_llm_client::TransportError::HttpStatus {
                status: code,
                body: String::new(),
            });
            assert_eq!(
                classify_backend_error(&e).0,
                ProbeStatus::UpstreamStatus { status }
            );
        }
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
            model_list_base_url: None,
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
            model_list_base_url: None,
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
