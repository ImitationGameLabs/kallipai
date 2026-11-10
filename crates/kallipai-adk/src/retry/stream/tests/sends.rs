//! What a send does: how each outcome is classified, what a retry resends, and
//! what the stream hands back when it opens.

use super::*;

#[tokio::test]
async fn retries_on_server_error_then_errors() {
    let server = MockServer::start().await;
    mount_status(&server, 500).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &fast_policy(2),
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
            provider_id: "test",
            profile_id: "test",
            pacing: None,
            metrics: &Default::default(),
        },
        &tx,
        &mut retry_log,
        CancellationToken::new(),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(retry_log.len(), 2);
    assert_eq!(retrying_count(&mut rx), 2);
}

#[tokio::test]
async fn does_not_retry_on_client_error() {
    let server = MockServer::start().await;
    mount_status(&server, 400).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &fast_policy(3),
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
            provider_id: "test",
            profile_id: "test",
            pacing: None,
            metrics: &Default::default(),
        },
        &tx,
        &mut retry_log,
        CancellationToken::new(),
    )
    .await;

    // 400 is request-level permanent — Fatal, not retried, not a failover candidate.
    assert!(matches!(result, Err(RequestFailure::Fatal(_))));
    assert!(retry_log.is_empty(), "fatal 4xx is not retried");
    assert_eq!(retrying_count(&mut rx), 0);
}

#[tokio::test]
async fn retries_on_network_error() {
    // Point the backend at a port nothing listens on — `send` fails at the transport layer.
    let backend = OpenAiCompatBackend::new(
        reqwest::Client::builder().use_rustls_tls(),
        "test-key",
        Some("http://127.0.0.1:0"),
    )
    .expect("openai-compat backend constructs without network");
    let client = GenerationClient::new(backend, GenerationClientOptions::new("gpt-4.1-mini"));
    let mut conversation = client.conversation();

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &fast_policy(1),
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
            provider_id: "test",
            profile_id: "test",
            pacing: None,
            metrics: &Default::default(),
        },
        &tx,
        &mut retry_log,
        CancellationToken::new(),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(retry_log.len(), 1, "a network failure is retried");
    assert_eq!(retrying_count(&mut rx), 1);
}

#[tokio::test]
async fn succeeds_on_ok_stream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "data: {\"id\":\"chatcmpl-s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4.1-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n",
                    "text/event-stream",
                ),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();

    let mut stream = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &fast_policy(2),
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
            provider_id: "test",
            profile_id: "test",
            pacing: None,
            metrics: &Default::default(),
        },
        &tx,
        &mut retry_log,
        CancellationToken::new(),
    )
    .await
    .expect("2xx yields a stream")
    .stream;

    assert!(retry_log.is_empty());
    assert_eq!(retrying_count(&mut rx), 0);
    let event = stream.next().await.unwrap().unwrap();
    assert_eq!(
        event,
        GenerationEvent::Text {
            delta: "hi".to_owned()
        }
    );
}

/// A stream that opens spends the window: the record the profile carried is
/// dropped, so the next round is not told to wait for a window that served.
#[tokio::test]
async fn an_open_stream_drops_the_record_the_profile_carried() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    let client = GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
    let mut conversation = client.conversation();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();
    let pacing = Arc::new(RecordingPacing::default());
    let now = timefmt::now_epoch();
    pacing.seed(
        "test",
        Block {
            kind: BlockKind::Quota,
            until: Some(now + 600),
            detail: "seeded evidence".to_owned(),
            status: None,
            recorded_at: now,
        },
    );

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("test-model", vec![user_msg("q1")]),
            policy: &fast_policy(2),
            round: 0,
            episode: &mut Episode::new(now),
            provider_id: "test",
            profile_id: "test",
            pacing: Some(pacing.as_ref()),
            metrics: &Default::default(),
        },
        &tx,
        &mut retry_log,
        CancellationToken::new(),
    )
    .await;

    assert!(result.is_ok(), "the stream opens");
    assert!(
        pacing.recorded().is_empty(),
        "the window the record paced is spent"
    );
}

#[tokio::test]
async fn retries_on_request_timeout_classified_as_timeout() {
    // 408 keeps its own RetryKind (Timeout) through the BackendError classification path.
    let server = MockServer::start().await;
    mount_status(&server, 408).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &fast_policy(1),
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
            provider_id: "test",
            profile_id: "test",
            pacing: None,
            metrics: &Default::default(),
        },
        &tx,
        &mut retry_log,
        CancellationToken::new(),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(retry_log.len(), 1);
    assert!(matches!(retry_log[0].kind, RetryKind::Timeout));
    assert_eq!(retrying_count(&mut rx), 1);
}

#[tokio::test]
async fn stateful_chain_appends_with_previous_response_id() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    backend.queue_stream("resp-2");
    let client = GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
    let mut conversation = client.conversation();

    // Turn 1: the full context goes out, storage on, no continuation id.
    run_turn(
        &mut conversation,
        request("test-model", vec![user_msg("q1")]).with_tools(vec![]),
    )
    .await
    .expect("turn 1 succeeds");

    // Turn 2: mirror + new user turn, as the runner composes it — the wire request is
    // the delta only, chained by response id, with tools stripped.
    let mirror = conversation.last_message().expect("turn 1 mirrored");
    run_turn(
        &mut conversation,
        request("test-model", vec![user_msg("q1"), mirror, user_msg("q2")]).with_tools(vec![]),
    )
    .await
    .expect("turn 2 succeeds");

    let requests = backend.take_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].previous_response_id, None);
    assert_eq!(requests[0].store, Some(true));
    assert_eq!(requests[0].messages, vec![user_msg("q1")]);
    assert!(
        requests[0].tools.is_some(),
        "the opening turn carries tools"
    );
    assert_eq!(requests[1].previous_response_id.as_deref(), Some("resp-1"));
    assert_eq!(requests[1].store, Some(true));
    assert_eq!(requests[1].messages, vec![user_msg("q2")], "delta only");
    assert!(
        requests[1].tools.is_some(),
        "continuation keeps the caller's tools"
    );
}

#[tokio::test]
async fn settings_change_resends_full_context() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    backend.queue_stream("resp-2");
    let client = GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
    let mut conversation = client.conversation();

    run_turn(
        &mut conversation,
        request("test-model", vec![user_msg("q1")]),
    )
    .await
    .expect("turn 1 succeeds");
    let mirror = conversation.last_message().expect("turn 1 mirrored");
    run_turn(
        &mut conversation,
        request("test-model", vec![user_msg("q1"), mirror, user_msg("q2")]).with_temperature(0.7),
    )
    .await
    .expect("turn 2 succeeds");

    let requests = backend.take_requests();
    assert_eq!(requests.len(), 2);
    // A settings change is not a pure append: the whole context goes out statelessly.
    assert_eq!(requests[1].previous_response_id, None);
    assert_eq!(requests[1].messages.len(), 3);
}

#[tokio::test]
async fn provider_error_retries_and_resends_delta_with_anchor_kept() {
    let backend = RecordingBackend::new();
    backend.queue_stream("resp-1");
    backend.queue_stream("resp-2");
    let client = GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
    let mut conversation = client.conversation();

    run_turn(
        &mut conversation,
        request("test-model", vec![user_msg("q1")]),
    )
    .await
    .expect("turn 1 succeeds");
    let mirror = conversation.last_message().expect("turn 1 mirrored");
    // Turn 2's first attempt fails at the provider; a failed turn keeps the anchor
    // (the stored response stays valid server-side), so the retry continues it with
    // a delta. (Queued after turn 1: the mock's error queue fires before any stream.)
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::HttpStatus {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: "upstream failed".to_owned(),
        },
    ));
    // A real counter set and a backoff long enough to see: the two sends of
    // this turn must count once each, and the wait between them must stay
    // out of their durations.
    let metrics = crate::metrics::AdkMetrics::new();
    let policy = RetryPolicy {
        max_retries: 2,
        base_delay: Duration::from_millis(120),
        max_delay: Duration::from_millis(120),
        retry_timeout: Duration::from_secs(10),
    };

    let mut retry_log = Vec::new();
    let mut stream = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("test-model", vec![user_msg("q1"), mirror, user_msg("q2")]),
            policy: &policy,
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
            provider_id: "test",
            profile_id: "test",
            pacing: None,
            metrics: &metrics,
        },
        &tokio::sync::mpsc::channel(16).0,
        &mut retry_log,
        CancellationToken::new(),
    )
    .await
    .expect("turn 2 succeeds on retry")
    .stream;
    while let Some(event) = stream.next().await {
        event.expect("turn 2 events are healthy");
    }

    assert_eq!(retry_log.len(), 1);
    assert!(matches!(retry_log[0].kind, RetryKind::Server));

    let requests = backend.take_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].previous_response_id, None, "turn 1 opens full");
    assert_eq!(
        requests[1].previous_response_id.as_deref(),
        Some("resp-1"),
        "the rejected attempt chained normally"
    );
    assert_eq!(
        requests[2].previous_response_id.as_deref(),
        Some("resp-1"),
        "a failed turn keeps the anchor: the retry continues the stored turn"
    );
    assert_eq!(
        requests[2].messages,
        vec![user_msg("q2")],
        "delta only — the stored prefix is not resent"
    );

    // This walk books the send it could not open; the send it did open is
    // booked by whoever owns the stream, so only the failure is visible
    // here. What this test pins is the clock: 120ms of backoff passed
    // between the two sends and none of it is in the attempt's duration.
    let snapshot = metrics.snapshot().expect("local tallies");
    let failed = snapshot
        .model_requests_total
        .iter()
        .find(|(name, _)| *name == "error")
        .map(|(_, value)| *value)
        .expect("error is part of the outcome vocabulary");
    assert_eq!(failed, 1, "the send that could not open is counted");
    assert_eq!(snapshot.model_duration.count, 1);
    assert!(
        snapshot.model_duration.sum_seconds < 0.06,
        "the backoff never lands in a duration: {:?}",
        snapshot.model_duration
    );
}
