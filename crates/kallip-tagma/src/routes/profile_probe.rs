//! HTTP shell for the profile probe: POST /profiles/probe.
//!
//! A thin envelope over [`crate::probe`]: operator auth and the request-size
//! bounds (self-DoS and spend levers on an operator-only route) are HTTP
//! policy enforced here; the probing domain — resolution, concurrent
//! provider checks, tier settlement — lives in the domain module, whose
//! wire types the relay's manage shim also reuses.

use axum::Json;
use axum::extract::State;
use kallip_common::protocol::ApiError;

use crate::auth::AuthIdentity;
use crate::probe::{MAX_PROBE_PROFILES, MAX_PROBE_PROVIDERS, ProbeRequest, ProbeResponse};
use crate::state::SharedState;

/// POST /profiles/probe — build throwaway backends and probe them.
///
/// Operator-only, like the rest of the profiles API (definitions carry keys).
pub async fn probe_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Json(request): Json<ProbeRequest>,
) -> Result<Json<ProbeResponse>, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    if request.endpoints.len() > MAX_PROBE_PROVIDERS {
        return Err(ApiError::bad_request(format!(
            "probe request carries {} endpoints; at most {MAX_PROBE_PROVIDERS} per request",
            request.endpoints.len()
        )));
    }
    let profile_count: usize = request.tiers.iter().map(|t| t.profiles.len()).sum();
    if profile_count > MAX_PROBE_PROFILES {
        return Err(ApiError::bad_request(format!(
            "probe request carries {profile_count} tier profiles; at most {MAX_PROBE_PROFILES} per request"
        )));
    }

    let response = crate::probe::run_probe(&state, request).await;
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::ProbeStatus;
    use crate::test_helpers::make_state;

    fn op_auth() -> AuthIdentity {
        AuthIdentity::test_new(crate::auth::Identity::Operator)
    }

    fn probe_req(json: serde_json::Value) -> ProbeRequest {
        serde_json::from_value(json).expect("valid probe request")
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
    async fn probe_rejects_more_than_max_providers() {
        let state = make_state();
        let endpoints: Vec<serde_json::Value> = (0..=MAX_PROBE_PROVIDERS)
            .map(|i| {
                serde_json::json!({
                    "id": format!("ep{i}"),
                    "family": "deepseek",
                    "api_key": "sk-test",
                    "base_url": null
                })
            })
            .collect();
        let req = probe_req(serde_json::json!({ "endpoints": endpoints }));
        let err = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("at most"), "got: {err}");
    }

    #[tokio::test]
    async fn probe_rejects_more_than_max_profiles() {
        let state = make_state();
        let profiles: Vec<serde_json::Value> = (0..=MAX_PROBE_PROFILES)
            .map(|i| serde_json::json!({ "id": format!("p{i}"), "endpoint": "test", "model": "m" }))
            .collect();
        let req = probe_req(serde_json::json!({ "tiers": [{ "profiles": profiles }] }));
        let err = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("at most"), "got: {err}");
    }

    #[tokio::test]
    async fn null_key_without_live_provider_is_invalid_config() {
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
                .contains("no live provider 'ghost'")
        );
    }

    #[tokio::test]
    async fn tier_reference_to_unknown_provider_reports_invalid() {
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
        // Provider-level report for the dangling reference...
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

    /// Inline openai-compatible provider pointing at a wiremock server, so
    /// catalog and chat requests both land on mocks.
    fn mock_provider(server_uri: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "mock",
            "family": "openai-compatible",
            "api_key": "sk-test",
            "base_url": server_uri
        })
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

    async fn mock_catalog(server: &wiremock::MockServer, models: &[&str]) {
        use wiremock::matchers::{method, path};
        let data: Vec<serde_json::Value> = models
            .iter()
            .map(|m| serde_json::json!({ "id": m, "object": "model", "owned_by": "probe" }))
            .collect();
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "object": "list",
                    "data": data
                })),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn tier_profile_test_runs_minimal_inference() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        mock_catalog(&server, &["m1"]).await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok_body()))
            .expect(1)
            .mount(&server)
            .await;

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [mock_provider(&server.uri())],
            "tiers": [{ "profiles": [{ "id": "p1", "endpoint": "mock", "model": "m1" }] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.results[0].status, ProbeStatus::Ok);
        let report = &resp.tiers[0].profiles[0];
        assert_eq!(report.status, ProbeStatus::Ok);
        assert!(report.detail.is_none());
        assert!(resp.tiers[0].all_ok);
    }

    #[tokio::test]
    async fn inference_auth_failure_is_unauthorized() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        mock_catalog(&server, &["m1"]).await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("bad key"))
            .expect(1)
            .mount(&server)
            .await;

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [mock_provider(&server.uri())],
            "tiers": [{ "profiles": [{ "id": "p1", "endpoint": "mock", "model": "m1" }] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        // The catalog probe succeeded, so the profile's verdict is the
        // inference call's own classification.
        assert_eq!(resp.results[0].status, ProbeStatus::Ok);
        assert_eq!(resp.tiers[0].profiles[0].status, ProbeStatus::Unauthorized);
        assert!(!resp.tiers[0].all_ok);
    }

    #[tokio::test]
    async fn catalog_miss_settles_without_inference() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        mock_catalog(&server, &["m2"]).await;
        // Zero expected hits: the catalog miss must settle before any chat
        // request is spent.
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok_body()))
            .expect(0)
            .mount(&server)
            .await;

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [mock_provider(&server.uri())],
            "tiers": [{ "profiles": [{ "id": "p1", "endpoint": "mock", "model": "m1" }] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        let report = &resp.tiers[0].profiles[0];
        assert_eq!(report.status, ProbeStatus::InvalidConfig);
        assert!(
            report
                .detail
                .as_deref()
                .unwrap()
                .contains("not in provider catalog")
        );
    }

    #[tokio::test]
    async fn shared_provider_model_pair_dedupes_inference() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        mock_catalog(&server, &["m1"]).await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok_body()))
            .expect(1)
            .mount(&server)
            .await;

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [mock_provider(&server.uri())],
            "tiers": [{ "profiles": [
                { "id": "p1", "endpoint": "mock", "model": "m1" },
                { "id": "p2", "endpoint": "mock", "model": "m1" }
            ] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.tiers[0].profiles[0].status, ProbeStatus::Ok);
        assert_eq!(resp.tiers[0].profiles[1].status, ProbeStatus::Ok);
        assert!(resp.tiers[0].all_ok);
    }
}
