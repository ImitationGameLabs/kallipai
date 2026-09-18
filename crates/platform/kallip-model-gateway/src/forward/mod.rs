//! The forwarding face: `POST /v1/chat/completions`, the OpenAI-compat
//! shape. The proxy is a reverse proxy on this path (design decision 1a):
//! it resolves the presenting proxy key, picks the target profile, stamps
//! the upstream Authorization header from the secret domain, and streams
//! the response back verbatim. The request body is never parsed -- the
//! client's `model` field passes through as the deployment refinement.
//!
//! Profile resolution: `X-Kallipai-Profile` overrides when present
//! (unauthorized -> 403, unknown -> 404); otherwise the default set's
//! head profile (the set's first entry, the active deployment),
//! allowed-sets contract as the override (unauthorized -> 403; missing
//! marker or empty set -> 404). An upstream transport failure is a
//! hand-built 502 in the provider's error shape: this face's consumers
//! are LLM clients reading the OpenAI wire vocabulary, not this service's
//! `ApiError`, which serves the internal plane's callers.
//!
//! The authorization matrix gates this path (429 on an exhausted rpm/tpm
//! window or budget, fail-closed when a check cannot run) and every
//! forwarded request lands one audit row in Postgres.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::distribution::ProxyKey;
mod support;
use crate::registry;
use crate::routes::ApiError;
use crate::secret;
use crate::state::AppState;

/// Upstream call timeout. Unlike the internal-plane clients' 10s contract,
/// LLM generations run long and streaming responses keep the connection
/// open for the whole generation -- this is the total wall-clock bound.
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Request body bound: 8 MB -- generous for chat completions, bounded so a
/// runaway client cannot balloon proxy memory.
pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// The optional explicit profile override header (decision 2: needed only
/// when one key authorizes several profiles).
pub const PROFILE_HEADER: &str = "x-kallipai-profile";

/// POST /v1/chat/completions.
pub async fn chat_completions(
    State(state): State<AppState>,
    key: ProxyKey,
    headers: HeaderMap,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    let started = std::time::Instant::now();
    // Profile resolution (one fetch per branch): the explicit override
    // or the default set's head, authorized under the same allowed-sets
    // contract -- then the parking gate. Availability is a second gate
    // orthogonal to authorization: a parked profile inside an allowed
    // set is still not forwardable (pinned by a regression test).
    let profile = match headers.get(PROFILE_HEADER).and_then(|v| v.to_str().ok()) {
        Some(explicit) => {
            let profile = registry::profile_by_id(&state.db, explicit)
                .await
                .map_err(crate::db::map_db_err)?
                .ok_or_else(|| ApiError::not_found("no such profile"))?;
            let authorized = registry::profile_in_authorized_sets(
                &state.db,
                explicit,
                &key.resolved.allowed_sets,
            )
            .await
            .map_err(crate::db::map_db_err)?;
            if !authorized {
                return Err(ApiError::forbidden(
                    "profile is outside the key's allowed sets",
                ));
            }
            if profile.parked {
                return Err(ApiError::forbidden("profile is parked"));
            }
            profile
        }
        None => {
            let default_set = registry::default_set_name(&state.db)
                .await
                .map_err(crate::db::map_db_err)?
                .ok_or_else(|| ApiError::not_found("no default set configured"))?;
            let profile = registry::set_head(&state.db, &default_set)
                .await
                .map_err(crate::db::map_db_err)?
                .ok_or_else(|| ApiError::not_found("default set has no profiles"))?;
            // Same contract as the override path: a key that cannot reach
            // the profile it would land on is unauthorized, default or
            // not -- otherwise the default deployment is a fail-open side
            // door around the allowed-sets contract.
            let authorized = registry::profile_in_authorized_sets(
                &state.db,
                &profile.profile_id,
                &key.resolved.allowed_sets,
            )
            .await
            .map_err(crate::db::map_db_err)?;
            if !authorized {
                return Err(ApiError::forbidden(
                    "default profile is outside the key's allowed sets",
                ));
            }
            if profile.parked {
                return Err(ApiError::forbidden("profile is parked"));
            }
            profile
        }
    };
    let profile_id = profile.profile_id.clone();
    let model = profile.model.clone();

    // The authorization matrix gate: 429 on an exhausted window or
    // budget, fail-closed (5xx) when a check cannot run. The budget
    // sums read the audit table; window counters live in the
    // process-local ledger.
    if let Err(denied) =
        support::enforce_quotas(&state.quota, &state.db, &key.resolved, &profile).await
    {
        return Ok(*denied);
    }

    let credential = secret::credential_for_profile(&state.db, &profile_id)
        .await
        .map_err(crate::db::map_db_err)?
        .ok_or_else(|| {
            ApiError::internal(format_args!(
                "no upstream credential for profile {profile_id}"
            ))
        })?;

    let url = format!(
        "{}/chat/completions",
        credential.endpoint_prefix().trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(UPSTREAM_TIMEOUT)
        .build()
        .map_err(ApiError::internal)?;
    let upstream_request = credential
        .inject(client.post(url))
        .headers(header_map_subset(&headers));
    // Body: raw passthrough. Chat-completions payloads are JSON, not
    // streams; collect the bounded axum body into the reqwest request.
    let bytes = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::bad_request("request body too large or unreadable"))?;
    let upstream_response = match upstream_request.body(bytes.to_vec()).send().await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(error = %e, "upstream forward failed");
            return Ok(upstream_unavailable());
        }
    };

    let status = upstream_response.status();
    let response_headers = upstream_response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();
    let content_type = upstream_response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut pending = support::PendingAudit {
        db: state.db.clone(),
        row: crate::audit::RequestAuditRow {
            key_hash: key.resolved.key_hash.clone(),
            tagma_id: key.resolved.tagma_id.clone(),
            profile_id: profile_id.clone(),
            model,
            status_code: status.as_u16() as i32,
            duration_ms: 0,
            usage: None,
        },
    };

    let mut builder = Response::builder().status(status);
    if let Some(ct) = response_headers {
        builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
    }

    let stream = upstream_response.bytes_stream();
    if content_type.contains("text/event-stream") {
        // Streamed: tee through the usage scanner (bytes stay verbatim);
        // the audit row lands when the upstream stream ends.
        let tee = support::audited_stream(
            stream,
            pending,
            state.quota.clone(),
            key.resolved.tagma_id.clone(),
            profile_id,
            started,
        );
        return Ok(builder
            .body(Body::from_stream(tee))
            .expect("static builder"));
    }

    // Buffered: a non-streaming response is a complete JSON document, so
    // parse the usage before the body goes out and land the row here.
    let bytes =
        match axum::body::to_bytes(Body::from_stream(stream), support::MAX_RESPONSE_BUFFER).await {
            Ok(bytes) => bytes,
            Err(_) => {
                tracing::warn!("upstream response exceeded the audit buffer bound");
                return Ok(upstream_unbounded());
            }
        };
    let usage = crate::audit::usage::usage_from_json(&bytes);
    support::refill_tokens(&state.quota, &key.resolved.tagma_id, &profile_id, usage);
    pending.row.duration_ms = started.elapsed().as_millis() as i64;
    pending.row.usage = usage;
    pending.land().await;
    Ok(builder.body(Body::from(bytes)).expect("static builder"))
}

/// The 502 for an upstream response past the audit buffer bound (the
/// non-streaming branch buffers bounded; unbounded is treated as a
/// broken upstream rather than forwarded without its audit side).
fn upstream_unbounded() -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({
            "error": {
                "message": "upstream response unbounded",
                "type": "upstream_unavailable",
                "code": "bad_gateway"
            }
        })),
    )
        .into_response()
}

/// The 502 response for upstream transport failures.
/// Hand-built in the provider's error envelope: this face speaks to LLM
/// clients, which read the OpenAI wire error shape -- a different
/// consumer from this service's own `ApiError` vocabulary (the internal
/// plane's callers).
fn upstream_unavailable() -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({
            "error": {
                "message": "upstream provider unreachable",
                "type": "upstream_unavailable",
                "code": "bad_gateway"
            }
        })),
    )
        .into_response()
}

/// Copy safe request headers onto the upstream request. Identity and
/// hop-by-hop headers are rebuilt for the upstream; the proxy-key
/// Authorization and the profile-override header never leave the proxy.
/// `accept-encoding` is stripped too: reqwest is built without
/// decompression features, so a compressed upstream body would reach the
/// client with its `content-encoding` dropped -- labeled corruption.
/// Compression arrives with the decompression feature, if ever.
fn header_map_subset(headers: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in headers.iter() {
        if matches!(
            name.as_str(),
            "accept-encoding"
                | "authorization"
                | "host"
                | "content-length"
                | "connection"
                | "x-kallipai-profile"
        ) {
            continue;
        }
        if let Ok(wn) = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()) {
            out.insert(wn, value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::test_support::{UPSTREAM_KEY, migrated_test_db};
    use axum::routing::post;
    use tower::ServiceExt;

    /// An in-process mock upstream capturing the forwarded headers and
    /// answering a canned openai-wire completion. Returns its base URL.
    async fn spawn_mock_upstream() -> (
        String,
        std::sync::Arc<tokio::sync::Mutex<Option<HeaderMap>>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock upstream");
        let addr = listener.local_addr().expect("local addr");
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(None::<HeaderMap>));
        let seen_for_handler = seen.clone();
        let app = axum::Router::new().route(
            "/chat/completions",
            post(
                move |headers: HeaderMap, body: axum::body::Bytes| async move {
                    *seen_for_handler.lock().await = Some(headers);
                    assert!(!body.is_empty(), "forwarded body must not be empty");
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "id": "cmpl-test-1",
                            "object": "chat.completion",
                            "choices": [{"message": {"role": "assistant", "content": "hi"}}],
                            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
                        })),
                    )
                },
            ),
        );
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock upstream serve");
        });
        (format!("http://{addr}"), seen)
    }

    fn app_with(db: crate::db::Db) -> axum::Router {
        crate::routes::data_plane_router(
            AppState {
                db,
                public_base_url: "http://gw.test:7501".to_string(),
                quota: std::sync::Arc::new(crate::quota::QuotaLedger::new()),
                management: crate::test_support::test_management(),
                key_cache: std::sync::Arc::new(crate::secret::KeyCache::default()),
            },
            "",
        )
    }

    fn forward_request(bearer: &str, profile: Option<&str>, body: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"));
        if let Some(profile) = profile {
            builder = builder.header(PROFILE_HEADER, profile);
        }
        builder.body(Body::from(body.to_string())).expect("request")
    }

    /// The forwarding path end to end against the mock upstream: default-set
    /// selection, Authorization injection (the proxy key must NOT travel),
    /// and response passthrough.
    #[tokio::test]
    async fn forward_injects_upstream_authorization_and_passes_through() {
        let db = migrated_test_db().await;
        let (upstream_base, seen) = spawn_mock_upstream().await;
        crate::test_support::seed_registry(&db, &upstream_base).await;
        let app = app_with(db);

        let response = app
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER,
                None,
                r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["id"], "cmpl-test-1");
        let seen_headers = seen.lock().await;
        let auth = seen_headers
            .as_ref()
            .and_then(|h| h.get("authorization"))
            .and_then(|v| v.to_str().ok());
        assert_eq!(auth, Some(format!("Bearer {UPSTREAM_KEY}").as_str()));
    }

    /// Authorization scoping on the forward path: unknown key 401, known key
    /// asking for an out-of-scope profile 403, unknown profile 404.
    #[tokio::test]
    async fn forward_rejects_unknown_keys_and_out_of_scope_profiles() {
        let db = migrated_test_db().await;
        let (upstream_base, _seen) = spawn_mock_upstream().await;
        crate::test_support::seed_registry(&db, &upstream_base).await;
        let app = app_with(db);

        let response = app
            .clone()
            .oneshot(forward_request("not-a-key", None, "{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .clone()
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER,
                Some("p3"),
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER,
                Some("ghost"),
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// The default path answers to the same allowed-sets contract as the
    /// override path: a key with no allowed sets is 403 on the default
    /// deployment too (fail-open regression).
    #[tokio::test]
    async fn forward_default_path_requires_authorized_key() {
        let db = migrated_test_db().await;
        let (upstream_base, _seen) = spawn_mock_upstream().await;
        crate::test_support::seed_registry(&db, &upstream_base).await;
        let app = app_with(db);

        let response = app
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER_STARVED,
                None,
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// accept-encoding never reaches the upstream: reqwest is built without
    /// decompression features, so forwarding the header would buy a
    /// compressed body whose content-encoding the proxy drops on the way
    /// back (unlabeled corruption).
    #[tokio::test]
    async fn forward_strips_accept_encoding() {
        let db = migrated_test_db().await;
        let (upstream_base, seen) = spawn_mock_upstream().await;
        crate::test_support::seed_registry(&db, &upstream_base).await;
        let app = app_with(db);

        let mut request = forward_request(crate::test_support::TEST_BEARER, None, "{}");
        request.headers_mut().insert(
            "accept-encoding",
            axum::http::HeaderValue::from_static("gzip"),
        );
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            seen.lock()
                .await
                .as_ref()
                .and_then(|h| h.get("accept-encoding"))
                .is_none()
        );
    }

    /// The matrix gate: a tagma whose rpm share is 1 answers the first
    /// request with 200 and the next with a 429 in the provider's error
    /// envelope (the share is per-tagma, not per-key).
    #[tokio::test]
    async fn forward_answers_429_when_rpm_share_is_exhausted() {
        let db = migrated_test_db().await;
        let (upstream_base, _seen) = spawn_mock_upstream().await;
        crate::test_support::seed_registry(&db, &upstream_base).await;
        use sea_orm::{ConnectionTrait, Statement};
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE tagmas SET rpm_limit = 1",
        ))
        .await
        .expect("tighten the rpm share");
        let app = app_with(db);

        let response = app
            .clone()
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER,
                None,
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER,
                None,
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_exceeded");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message present")
                .contains("tagma rpm")
        );
    }

    /// The tpm share: the mock upstream's usage (18 tokens) fills the
    /// window; the second request is denied by the tagma tpm gate.
    #[tokio::test]
    async fn forward_answers_429_when_tpm_share_is_exhausted() {
        let db = migrated_test_db().await;
        let (upstream_base, _seen) = spawn_mock_upstream().await;
        crate::test_support::seed_registry(&db, &upstream_base).await;
        use sea_orm::{ConnectionTrait, Statement};
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE tagmas SET tpm_limit = 18",
        ))
        .await
        .expect("tighten the tpm share");
        let app = app_with(db);

        let response = app
            .clone()
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER,
                None,
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER,
                None,
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_exceeded");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message present")
                .contains("tagma tpm")
        );
    }

    /// The parking gate: a parked profile inside an allowed
    /// set passes authorization but fails availability -- 403 on the
    /// override path.
    #[tokio::test]
    async fn forward_rejects_parked_profiles_inside_allowed_sets() {
        let db = migrated_test_db().await;
        let (upstream_base, _seen) = spawn_mock_upstream().await;
        crate::test_support::seed_registry(&db, &upstream_base).await;
        // Park p5 into the allowed set `alpha` (position 2; the failover
        // slots 0/1 are taken).
        use sea_orm::{ActiveModelTrait, ConnectionTrait, Set, Statement};
        crate::registry::profile::ActiveModel {
            profile_id: Set("p5".to_string()),
            family: Set("deepseek".to_string()),
            model: Set("deepseek-parked".to_string()),
            max_context_window: Set(None),
            effort: Set(None),
            modalities: Set(None),
            parked: Set(true),
            max_budget: Set(None),
            tpm_limit: Set(None),
            rpm_limit: Set(None),
        }
        .insert(&db)
        .await
        .expect("insert parked profile");
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO set_members (set_name, profile_id, position) VALUES ('alpha', 'p5', 2)",
        ))
        .await
        .expect("park the parked profile into the allowed set");
        let app = app_with(db);

        let response = app
            .clone()
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER,
                Some("p5"),
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// The audit row: a forwarded request lands one request_audits row
    /// with the response's usage block (the mock upstream carries one),
    /// the served model, the status code, and the key's identity.
    #[tokio::test]
    async fn forward_lands_an_audit_row_with_usage() {
        let db = migrated_test_db().await;
        let (upstream_base, _seen) = spawn_mock_upstream().await;
        crate::test_support::seed_registry(&db, &upstream_base).await;
        let app = app_with(db.clone());

        let response = app
            .oneshot(forward_request(
                crate::test_support::TEST_BEARER,
                None,
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        use crate::audit::request_audit::Entity as RequestAudits;
        use sea_orm::EntityTrait;
        let rows = RequestAudits::find().all(&db).await.expect("audit rows");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.status_code, 200);
        assert_eq!(row.profile_id, "p1");
        assert_eq!(row.model, "deepseek-chat");
        assert_eq!(row.tagma_id, "tagma-under-test");
        assert_eq!(row.prompt_tokens, Some(11));
        assert_eq!(row.completion_tokens, Some(7));
        assert_eq!(row.total_tokens, Some(18));
        assert_eq!(row.cost_micros, None);
        let key_hash = kallip_common::authtoken::TokenHash::of(crate::test_support::TEST_BEARER)
            .as_bytes()
            .to_vec();
        assert_eq!(row.key_hash, key_hash);
    }
}
