//! Which exit a failure takes: retry on the same endpoint, advance the chain, stop
//! the round, or leave the fact the failure earned.

use super::*;

#[tokio::test]
async fn switches_on_not_found() {
    let server = MockServer::start().await;
    mount_status(&server, 404).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
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

    // 404 is endpoint/profile-level: not retried in-profile, and the round hands
    // the switch to the caller, which advances the chain.
    assert!(matches!(
        result,
        Err(RequestFailure::Switch {
            reason: "endpoint or credentials rejected the request",
            ..
        })
    ));
    assert!(retry_log.is_empty(), "404 is not retried in-profile");
}

#[tokio::test]
async fn switches_on_auth_error() {
    let server = MockServer::start().await;
    mount_status(&server, 401).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
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

    // 401 (auth) is endpoint-level: the round hands the switch to the caller, and a
    // different profile with different credentials may succeed.
    assert!(matches!(
        result,
        Err(RequestFailure::Switch {
            reason: "endpoint or credentials rejected the request",
            ..
        })
    ));
    assert!(retry_log.is_empty());
}

/// A throttle names no reset moment, so the same endpoint is worth one more
/// send: the episode spends it, then the round hands the switch on.
#[tokio::test]
async fn a_throttle_spends_one_resend_then_switches() {
    let server = MockServer::start().await;
    mount_status(&server, 429).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();
    let mut episode = Episode::new(timefmt::now_epoch());

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &fast_policy(4),
            round: 0,
            episode: &mut episode,
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

    assert!(matches!(
        result,
        Err(RequestFailure::Switch {
            reason: "upstream throttled the request with no reset moment",
            ..
        })
    ));
    assert_eq!(retrying_count(&mut rx), 1, "one retry precedes the switch");
    assert_eq!(
        episode.attempts(),
        2,
        "the episode spent its throttle allowance"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "the throttled send is repeated once, no more"
    );
    assert_eq!(retry_log.len(), 2, "one retry record and one observation");
    assert!(
        retry_log
            .iter()
            .all(|record| record.kind == RetryKind::RateLimit),
        "both records name the rate limit"
    );
}

#[tokio::test]
async fn a_quota_wall_that_names_its_reset_switches_without_retrying() {
    let server = MockServer::start().await;
    mount_status_with_body(&server, 429, QUOTA_RESET_BODY).await;

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

    // A quota rejection does not recover by resending the same request: the
    // round hands the switch to the caller, and the endpoint saw exactly one
    // send. The failure is still an observation: one record with no wait.
    assert!(matches!(
        result,
        Err(RequestFailure::Switch {
            reason: "upstream quota or payment rejection",
            ..
        })
    ));
    assert_eq!(retry_log.len(), 1, "a quota rejection writes one record");
    assert!(matches!(retry_log[0].kind, RetryKind::RateLimit));
    assert_eq!(retry_log[0].delay_secs, 0.0, "no wait precedes the switch");
    assert!(
        retry_log[0].quota_reset.is_some(),
        "the body named the reset moment"
    );
    assert_eq!(retrying_count(&mut rx), 0);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "the same endpoint is not sent again"
    );
}

/// A response shape that fails the same way twice is not a transient problem:
/// the first shape failure retries, the repeat leaves through the switch.
#[tokio::test]
async fn a_repeated_response_shape_stops_the_episode() {
    let backend = RecordingBackend::new();
    for _ in 0..2 {
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::InvalidResponse("empty body".into()),
        ));
    }
    let client = GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("test-model", vec![user_msg("q1")]),
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

    assert!(matches!(
        result,
        Err(RequestFailure::Switch {
            reason: "the response shape failed twice in this episode",
            ..
        })
    ));
    assert_eq!(
        backend.take_requests().len(),
        2,
        "the first shape failure retries once; the repeat stops"
    );
    assert_eq!(retry_log.len(), 1, "the repeat records no retry");
    assert_eq!(retrying_count(&mut rx), 1);
}

/// A client-construction failure is local: no send can fix it, so the round stops
/// on the first answer with no retry recorded.
#[tokio::test]
async fn a_local_configuration_error_stops_without_retrying() {
    let backend = RecordingBackend::new();
    backend.queue_error(BackendError::provider(
        "recording",
        TransportError::InvalidConfig("no key"),
    ));
    let client = GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("test-model", vec![user_msg("q1")]),
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

    assert!(matches!(
        result,
        Err(RequestFailure::Stop {
            reason: "local client configuration error",
            ..
        })
    ));
    assert_eq!(backend.take_requests().len(), 1);
    assert!(retry_log.is_empty());
    assert_eq!(retrying_count(&mut rx), 0);
}

/// The connect path writes what the rejection taught: the 429 that stops this
/// profile leaves a quota fact, keyed by the profile id the call was made
/// through.
#[tokio::test]
async fn the_connect_path_writes_the_fact_a_rejection_leaves() {
    let server = MockServer::start().await;
    mount_status(&server, 429).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();
    let pacing = Arc::new(RecordingPacing::default());

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &fast_policy(2),
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
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

    assert!(matches!(result, Err(RequestFailure::Switch { .. })));
    let recorded = pacing.recorded();
    assert_eq!(recorded.len(), 1, "one rejection, one fact");
    assert_eq!(
        recorded[0].0, "test",
        "the record is keyed by the profile the send ran on"
    );
    assert_eq!(recorded[0].1.kind, BlockKind::Quota);
    assert!(
        recorded[0].1.kind.parks_the_round(),
        "a quota rejection parks the round"
    );
}
