//! HTTP shell for the profile probe: POST /profiles/probe.
//!
//! A thin envelope over [`crate::probe`]: operator auth and the request-size
//! bounds (self-DoS and spend levers on an operator-only route) are HTTP
//! policy enforced here; the probing domain — resolution, concurrent
//! provider checks, set settlement — lives in the domain module, whose
//! wire types the relay's manage shim also reuses.

use axum::Json;
use axum::extract::State;
use kallipai_common::protocol::ApiError;

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
    let profile_count: usize = request.sets.iter().map(|s| s.profiles.len()).sum();
    if profile_count > MAX_PROBE_PROFILES {
        return Err(ApiError::bad_request(format!(
            "probe request carries {profile_count} set profiles; at most {MAX_PROBE_PROFILES} per request"
        )));
    }

    let response = crate::probe::run_probe(&state, request).await;
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::ModelsHint;
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
            id: kallipai_common::agentid::AgentId::random(),
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
        let req =
            probe_req(serde_json::json!({ "sets": [{ "name": "s0", "profiles": profiles }] }));
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
    async fn set_reference_to_unknown_provider_reports_invalid() {
        let state = make_state();
        let req = probe_req(serde_json::json!({
            "sets": [{ "name": "s0", "profiles": [{
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
        // ...and the set profile inherits the failure.
        assert_eq!(resp.sets[0].profiles[0].status, ProbeStatus::InvalidConfig);
        assert!(!resp.sets[0].all_ok);
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
    /// model-list and chat requests both land on mocks.
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

    async fn mock_models(server: &wiremock::MockServer, models: &[&str]) {
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
    async fn set_profile_test_runs_minimal_inference() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        mock_models(&server, &["m1"]).await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok_body()))
            .expect(1)
            .mount(&server)
            .await;

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [mock_provider(&server.uri())],
            "sets": [{ "name": "s0", "profiles": [{ "id": "p1", "endpoint": "mock", "model": "m1" }] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.results[0].status, ProbeStatus::Ok);
        let report = &resp.sets[0].profiles[0];
        assert!(resp.sets[0].all_ok);
        assert_eq!(report.status, ProbeStatus::Ok);
        assert!(report.detail.is_none());
    }

    #[tokio::test]
    async fn inference_auth_failure_is_unauthorized() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        mock_models(&server, &["m1"]).await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("bad key"))
            .expect(1)
            .mount(&server)
            .await;

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [mock_provider(&server.uri())],
            "sets": [{ "name": "s0", "profiles": [{ "id": "p1", "endpoint": "mock", "model": "m1" }] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        // The model-list probe succeeded, so the profile's verdict is the
        // inference call's own classification.
        assert_eq!(resp.results[0].status, ProbeStatus::Ok);
        assert_eq!(resp.sets[0].profiles[0].status, ProbeStatus::Unauthorized);
        assert!(!resp.sets[0].all_ok);
    }

    #[tokio::test]
    async fn models_miss_hints_but_the_call_decides() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        // The list names a different model; the call still decides the verdict.
        mock_models(&server, &["m2"]).await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok_body()))
            .expect(1)
            .mount(&server)
            .await;

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [mock_provider(&server.uri())],
            "sets": [{ "name": "s0", "profiles": [{ "id": "p1", "endpoint": "mock", "model": "m1" }] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        let report = &resp.sets[0].profiles[0];
        assert_eq!(report.status, ProbeStatus::Ok);
        assert_eq!(report.models_hint, Some(ModelsHint::NotListed));
        assert!(resp.sets[0].all_ok, "the call decides, not the list");
    }

    #[tokio::test]
    async fn model_list_base_url_carries_the_list_only() {
        use wiremock::matchers::{method, path};

        // The list lives on one server, inference on another: no single base
        // serves both, which is what `model_list_base_url` exists for.
        let list = wiremock::MockServer::start().await;
        mock_models(&list, &["m1"]).await;
        let chat = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok_body()))
            .expect(1)
            .mount(&chat)
            .await;

        let mut provider = mock_provider(&chat.uri());
        provider["model_list_base_url"] = serde_json::json!(list.uri());

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [provider],
            "sets": [{ "name": "s0", "profiles": [{ "id": "p1", "endpoint": "mock", "model": "m1" }] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.results[0].status, ProbeStatus::Ok);
        assert_eq!(resp.results[0].models_count, Some(1));
        assert_eq!(resp.sets[0].profiles[0].status, ProbeStatus::Ok);
        assert!(resp.sets[0].profiles[0].models_hint.is_none());
    }

    #[tokio::test]
    async fn an_empty_list_base_reads_the_list_from_the_inference_base() {
        // The live provider carries a model-list base; the draft clears it, so
        // the list must come from the inference base the draft also sends.
        // The live list base is a bogus host on purpose: were it dialed, the
        // mock's single expected hit would never arrive and the test fails.
        let server = wiremock::MockServer::start().await;
        mock_models(&server, &["m1"]).await;

        let state = make_state();
        let config = {
            let live = state.profiles.load();
            let mut config = live.config.clone();
            let entry = config.endpoints.get_mut("test").expect("live provider");
            entry.base_url = None;
            entry.model_list_base_url = Some("https://live-list.invalid/v1".into());
            config
        };
        {
            let live = state.profiles.load();
            state
                .profiles
                .store(std::sync::Arc::new(crate::state::ProfileBundle {
                    config,
                    registry: live.registry.clone(),
                }));
        }

        let mut provider = mock_provider(&server.uri());
        provider["id"] = serde_json::json!("test");
        provider["family"] = serde_json::json!("deepseek");
        provider["model_list_base_url"] = serde_json::json!("");

        let req = probe_req(serde_json::json!({
            "endpoints": [provider],
            "sets": [{ "name": "s0", "profiles": [{ "id": "p1", "endpoint": "test", "model": "m1" }] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        // The list came from the inference base: the mock saw it (expect(1)),
        // and the live list base was never inherited.
        assert_eq!(resp.results[0].models_count, Some(1));
        assert_eq!(resp.results[0].status, ProbeStatus::Ok);
    }

    #[tokio::test]
    async fn a_missing_list_does_not_settle_the_profile() {
        use wiremock::matchers::{method, path};

        // No list mock: `/models` answers 404 and the provider test fails, yet
        // the profile is judged anyway by its own call.
        let list = wiremock::MockServer::start().await;
        let chat = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok_body()))
            .expect(1)
            .mount(&chat)
            .await;

        let mut provider = mock_provider(&chat.uri());
        provider["model_list_base_url"] = serde_json::json!(list.uri());

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [provider],
            "sets": [{ "name": "s0", "profiles": [{ "id": "p1", "endpoint": "mock", "model": "m1" }] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(
            resp.results[0].status,
            ProbeStatus::UpstreamStatus { status: 404 }
        );
        assert_eq!(resp.sets[0].profiles[0].status, ProbeStatus::Ok);
        assert!(resp.sets[0].profiles[0].models_hint.is_none());
        assert!(resp.sets[0].all_ok);
    }

    #[tokio::test]
    async fn shared_provider_model_pair_dedupes_inference() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        mock_models(&server, &["m1"]).await;
        wiremock::Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok_body()))
            .expect(1)
            .mount(&server)
            .await;

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [mock_provider(&server.uri())],
            "sets": [{ "name": "s0", "profiles": [
                { "id": "p1", "endpoint": "mock", "model": "m1" },
                { "id": "p2", "endpoint": "mock", "model": "m1" }
            ] }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.sets[0].profiles[0].status, ProbeStatus::Ok);
        assert_eq!(resp.sets[0].profiles[1].status, ProbeStatus::Ok);
        assert!(resp.sets[0].all_ok);
    }

    #[tokio::test]
    async fn responses_family_models_probe_ok() {
        let server = wiremock::MockServer::start().await;
        mock_models(&server, &["gpt-test"]).await;

        let state = make_state();
        // Endpoints only (no sets): the model list is the whole probe.
        let req = probe_req(serde_json::json!({
            "endpoints": [{
                "id": "resp",
                "family": "openai-responses",
                "api_key": "sk-test",
                "base_url": server.uri()
            }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.results[0].status, ProbeStatus::Ok);
        assert_eq!(resp.results[0].models_count, Some(1));
    }

    #[tokio::test]
    async fn anthropic_family_models_probe_ok() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [{ "id": "claude-test", "type": "model" }]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let state = make_state();
        let req = probe_req(serde_json::json!({
            "endpoints": [{
                "id": "ant",
                "family": "anthropic",
                "api_key": "sk-ant-test",
                "base_url": server.uri()
            }]
        }));
        let resp = probe_profiles(State(state), op_auth(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.results[0].status, ProbeStatus::Ok);
        assert_eq!(resp.results[0].models_count, Some(1));
    }
}
