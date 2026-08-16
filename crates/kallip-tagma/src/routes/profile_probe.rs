//! Profile endpoint probing: POST /profiles/probe.
//!
//! A read-only diagnostics endpoint for the manage UI's Test buttons. The
//! caller submits endpoint definitions **inline** (draft config, before
//! save) or references live endpoints by id; the tagma builds throwaway
//! backends and exercises the zero-cost capability probes upstream offers
//! (`ModelCatalog::list_models`, `Balance::get_balance`). Nothing is
//! persisted, the live registry (`ArcSwap`) is untouched, and no chat
//! request is ever sent — probing costs no tokens.
//!
//! Statuses are layered so the UI can tell apart the failure classes an
//! operator can act on differently: `invalid_config` (fix the definition),
//! `unreachable` (network/endpoint down), `unauthorized` (credential), and
//! `model_missing`-style catalog mismatch (checked per tier profile).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use futures_util::future::join_all;
use just_llm_client::client::BackendFactory;
use kallip_common::protocol::ApiError;
use kallip_runtime::profile::{Endpoint, ProfileConfig};
use serde::{Deserialize, Serialize};

use crate::auth::AuthIdentity;
use crate::backend::{self, DEFAULT_USER_AGENT};
use crate::state::SharedState;

/// Per-endpoint probe budget. Generous for slow cold starts, tight enough
/// that a wedged endpoint cannot pin the request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
pub struct ProbeRequest {
    /// Inline endpoint definitions (draft config). `api_key: null` means
    /// "reuse the live key stored for this endpoint id" — the same draft
    /// semantics the masked PUT uses, so an unchanged key never needs to be
    /// sent back up.
    #[serde(default)]
    pub endpoints: Vec<ProbeEndpoint>,
    /// Tiers of profiles to check model names against the fetched catalogs.
    #[serde(default)]
    pub tiers: Vec<ProbeTier>,
}

#[derive(Deserialize)]
pub struct ProbeEndpoint {
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
    /// Endpoint id this profile connects through (inline or live).
    pub endpoint: String,
    pub model: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// Catalog (or balance) came back — endpoint is reachable and authorized.
    Ok,
    /// Transport-level failure or timeout — nothing HTTP-shaped responded.
    Unreachable,
    /// HTTP 401/403 — the credential was rejected.
    Unauthorized,
    /// The definition failed backend construction, the endpoint answered
    /// with an unexpected HTTP status (404/429/5xx on the probe path), or a
    /// tier profile's model is absent from the endpoint's catalog.
    InvalidConfig,
    /// Endpoint responded, but the family offers no zero-cost probe
    /// capability — liveness could not be established without a chat call.
    Partial,
}

#[derive(Serialize)]
pub struct EndpointReport {
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
    pub results: Vec<EndpointReport>,
    pub tiers: Vec<TierReport>,
}

/// POST /profiles/probe — build throwaway backends and probe them.
///
/// Operator-only, like the rest of the profiles API (definitions carry keys).
pub async fn probe_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Json(request): Json<ProbeRequest>,
) -> Result<Json<ProbeResponse>, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    let live = state.profiles.load().config.clone();
    let wants_models = !request.tiers.is_empty();

    // Resolve the endpoint set: inline definitions win; tiers may reference
    // endpoint ids not submitted inline — those resolve to live definitions.
    let mut defs: HashMap<String, Endpoint> = HashMap::new();
    let mut reports: Vec<EndpointReport> = Vec::new();
    for probe_ep in &request.endpoints {
        match resolve_endpoint(probe_ep, &live) {
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
            "referenced endpoint is neither inline nor live".to_string(),
        ));
    }

    // Probe all resolved endpoints concurrently; each is independent.
    let factory = BackendFactory::new();
    let mut probed = join_all(
        defs.into_values()
            .map(|def| probe_one(&factory, def, wants_models)),
    )
    .await;
    reports.append(&mut probed);

    // Tier checks reuse the per-endpoint reports; a missing catalog cannot
    // verify a model name, which reports as partial rather than a failure.
    let by_id: HashMap<&str, &EndpointReport> = reports
        .iter()
        .map(|r| (r.endpoint_id.as_str(), r))
        .collect();
    let tiers = request
        .tiers
        .iter()
        .enumerate()
        .map(|(index, tier)| {
            let profiles = tier
                .profiles
                .iter()
                .map(|p| profile_report_for(p, by_id.get(p.endpoint.as_str()).copied()))
                .collect::<Vec<_>>();
            let all_ok = profiles.iter().all(|p| p.status == ProbeStatus::Ok);
            TierReport {
                index,
                all_ok,
                profiles,
            }
        })
        .collect();

    Ok(Json(ProbeResponse {
        results: reports,
        tiers,
    }))
}

fn invalid_config_report(endpoint_id: String, detail: String) -> EndpointReport {
    EndpointReport {
        endpoint_id,
        status: ProbeStatus::InvalidConfig,
        latency_ms: None,
        catalog_count: None,
        balance: None,
        detail: Some(detail),
        models: None,
    }
}

fn profile_report_for(p: &ProbeProfile, ep: Option<&EndpointReport>) -> ProfileReport {
    match ep {
        Some(ep) => {
            let (status, detail) = match ep.status {
                ProbeStatus::Ok => match &ep.models {
                    Some(models) if models.iter().any(|m| m == &p.model) => (ProbeStatus::Ok, None),
                    Some(models) => (
                        ProbeStatus::InvalidConfig,
                        Some(format!(
                            "model '{}' not in endpoint catalog ({} models)",
                            p.model,
                            models.len()
                        )),
                    ),
                    // Catalog-less ok (balance-only probe): cannot verify.
                    None => (
                        ProbeStatus::Partial,
                        Some("endpoint ok but no catalog to verify the model name".to_string()),
                    ),
                },
                other => (other, None),
            };
            ProfileReport {
                profile_id: p.id.clone(),
                endpoint_id: p.endpoint.clone(),
                status,
                detail,
            }
        }
        None => ProfileReport {
            profile_id: p.id.clone(),
            endpoint_id: p.endpoint.clone(),
            status: ProbeStatus::InvalidConfig,
            detail: Some("endpoint was not probed".to_string()),
        },
    }
}

/// Merge an inline probe definition with the live config: a null `api_key` or
/// null `base_url` means "keep the live value for this endpoint id".
fn resolve_endpoint(probe: &ProbeEndpoint, live: &ProfileConfig) -> Result<Endpoint, String> {
    let api_key = match &probe.api_key {
        Some(key)
            if live
                .endpoints
                .get(&probe.id)
                .is_some_and(|ep| crate::routes::profiles::mask_key(&ep.api_key) == *key) =>
        {
            live.endpoints[&probe.id].api_key.clone()
        }
        Some(key) => key.clone(),
        None => live
            .endpoints
            .get(&probe.id)
            .map(|ep| ep.api_key.clone())
            .ok_or_else(|| {
                format!(
                    "no api_key given and no live endpoint '{}' to take one from",
                    probe.id
                )
            })?,
    };
    Ok(Endpoint {
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
async fn probe_one(factory: &BackendFactory, def: Endpoint, wants_models: bool) -> EndpointReport {
    let endpoint_id = def.id.clone();
    let mut report = EndpointReport {
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
        // The endpoint answered HTTP — reachable, but the probe path itself
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

    fn op_auth() -> AuthIdentity {
        AuthIdentity::test_new(crate::auth::Identity::Operator)
    }

    fn probe_req(json: serde_json::Value) -> ProbeRequest {
        serde_json::from_value(json).expect("valid probe request")
    }

    #[test]
    fn masked_key_echo_resolves_to_live_key() {
        // GET /profiles returns masked keys; a draft probed unchanged echoes the
        // mask back. That must resolve to the live key, not probe with the mask.
        let live = make_state().profiles.load().config.clone();
        let masked = crate::routes::profiles::mask_key(&live.endpoints["test"].api_key);
        let probe_ep = ProbeEndpoint {
            id: "test".into(),
            family: "deepseek".into(),
            api_key: Some(masked),
            base_url: None,
        };
        let resolved = resolve_endpoint(&probe_ep, &live).expect("masked echo resolves");
        assert_eq!(resolved.api_key, live.endpoints["test"].api_key);
    }

    fn endpoint_report(status: ProbeStatus, models: Option<Vec<String>>) -> EndpointReport {
        EndpointReport {
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
    fn profile_report_matches_catalog_hit_and_miss() {
        let hit = profile_report_for(
            &probe_profile("m1"),
            Some(&endpoint_report(
                ProbeStatus::Ok,
                Some(vec!["m1".into(), "m2".into()]),
            )),
        );
        assert_eq!(hit.status, ProbeStatus::Ok);
        assert!(hit.detail.is_none());

        let miss = profile_report_for(
            &probe_profile("nope"),
            Some(&endpoint_report(ProbeStatus::Ok, Some(vec!["m1".into()]))),
        );
        assert_eq!(miss.status, ProbeStatus::InvalidConfig);
        assert!(
            miss.detail
                .as_deref()
                .unwrap()
                .contains("not in endpoint catalog")
        );
    }

    #[test]
    fn profile_report_without_catalog_is_partial() {
        let r = profile_report_for(
            &probe_profile("any"),
            Some(&endpoint_report(ProbeStatus::Ok, None)),
        );
        assert_eq!(r.status, ProbeStatus::Partial);
    }

    #[test]
    fn profile_report_inherits_endpoint_failure() {
        let r = profile_report_for(
            &probe_profile("m1"),
            Some(&endpoint_report(ProbeStatus::Unauthorized, None)),
        );
        assert_eq!(r.status, ProbeStatus::Unauthorized);
    }

    #[test]
    fn profile_report_without_endpoint_report_is_invalid() {
        let r = profile_report_for(&probe_profile("m1"), None);
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
    fn resolve_endpoint_null_base_url_merges_live() {
        let mut live = make_state().profiles.load().config.clone();
        live.endpoints.get_mut("test").unwrap().base_url = Some("https://live.example".into());
        let probe_ep = ProbeEndpoint {
            id: "test".into(),
            family: "deepseek".into(),
            api_key: None,
            base_url: None,
        };
        let resolved = resolve_endpoint(&probe_ep, &live).unwrap();
        assert_eq!(resolved.base_url.as_deref(), Some("https://live.example"));
    }
    #[tokio::test]

    async fn probe_requires_operator() {
        let state = make_state();
        let anon = AuthIdentity::test_new(crate::auth::Identity::Agent {
            id: kallip_common::agentid::AgentId::random(),
        });
        let result =
            probe_profiles(State(state), anon, Json(probe_req(serde_json::json!({})))).await;
        assert!(result.is_err(), "non-operator must be rejected");
    }

    #[tokio::test]
    async fn null_key_without_live_endpoint_is_invalid_config() {
        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [{
                "id": "ghost",
                "family": "openai-compatible",
                "api_key": null,
                "base_url": "https://example.invalid/v1"
            }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].status, ProbeStatus::InvalidConfig);
        assert!(
            resp.results[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("no live endpoint 'ghost'")
        );
    }

    #[tokio::test]
    async fn tier_reference_to_unknown_endpoint_reports_invalid() {
        let state = make_state();
        let req = probe_req(serde_json::json!({
            "tiers": [{ "profiles": [{
                "id": "p1", "endpoint": "nowhere", "model": "m"
            }]}]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        // Endpoint-level report for the dangling reference...
        assert!(
            resp.results
                .iter()
                .any(|r| r.endpoint_id == "nowhere" && r.status == ProbeStatus::InvalidConfig)
        );
        // ...and the tier profile inherits the failure.
        assert_eq!(resp.tiers[0].profiles[0].status, ProbeStatus::InvalidConfig);
        assert!(!resp.tiers[0].all_ok);
    }

    #[tokio::test]
    async fn unreachable_timeout_is_classified() {
        // RFC 2606 .invalid domain: resolution fails, exercising the
        // transport-error path (unreachable, not a timeout — fast fail).
        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [{
                "id": "dead",
                "family": "openai-compatible",
                "api_key": "sk-test",
                "base_url": "https://example.invalid/v1"
            }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].status, ProbeStatus::Unreachable);
        assert!(resp.results[0].latency_ms.is_some());
    }

    #[tokio::test]
    async fn unknown_family_is_invalid_config() {
        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [{
                "id": "weird",
                "family": "does-not-exist",
                "api_key": "sk-test",
                "base_url": "https://example.com/v1"
            }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.results[0].status, ProbeStatus::InvalidConfig);
        assert!(
            resp.results[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("does-not-exist")
        );
    }
}
