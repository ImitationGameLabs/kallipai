//! What the episode ledger and its clock allow: sends spent, the deadline, the
//! wait that actually happened, and a cancel during a backoff.

use super::*;

#[tokio::test]
async fn cancel_during_backoff_returns_cancelled_without_recording() {
    // A pre-cancelled token wins the backoff `select!` on the first retry → the outcome is
    // `Cancelled` (not `Failover`), and the truncated backoff pushes no record.
    let server = MockServer::start().await;
    mount_status(&server, 500).await; // forces a retry → backoff

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();
    let cancel = CancellationToken::new();
    cancel.cancel(); // already cancelled before the backoff select runs

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
        cancel,
    )
    .await;

    assert!(
        matches!(result, Err(RequestFailure::Cancelled)),
        "cancel during backoff surfaces as Cancelled, got {result:?}"
    );
    assert!(
        retry_log.is_empty(),
        "a cancel-truncated backoff must not persist a retry record"
    );
    // The Retrying event still fires pre-select (try_send is non-blocking but still emits).
    assert_eq!(retrying_count(&mut rx), 1);
}

/// An episode that already spent its sends makes no request: the entry guard
/// refuses before the wire, and the round fails instead of retrying.
#[tokio::test]
async fn a_spent_episode_sends_nothing() {
    let server = MockServer::start().await;
    mount_status(&server, 429).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();
    let policy = fast_policy(2);
    let mut episode = Episode::new(timefmt::now_epoch());
    for _ in 0..max_sends(&policy) {
        episode = episode.after_attempt();
    }

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &policy,
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

    assert!(matches!(result, Err(RequestFailure::Fatal(_))));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "a spent ledger sends nothing"
    );
    assert!(retry_log.is_empty());
    assert_eq!(retrying_count(&mut rx), 0);
}

/// Sends already charged elsewhere in the episode shrink this call's budget: the
/// ledger, not the persisted retry log, decides how many sends remain.
#[tokio::test]
async fn a_partly_spent_episode_spends_only_what_is_left() {
    let server = MockServer::start().await;
    mount_status(&server, 500).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();
    let policy = fast_policy(2);
    // Two of the episode's three sends are gone: one send is left, and its
    // failure exhausts the budget without a retry.
    let mut episode = Episode::new(timefmt::now_epoch());
    episode = episode.after_attempt();
    episode = episode.after_attempt();

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &policy,
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

    assert!(matches!(result, Err(RequestFailure::Failover(_))));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "exactly the one send the ledger allowed"
    );
    assert_eq!(episode.attempts(), 3, "the send charged the ledger");
    assert!(retry_log.is_empty(), "an exhausted budget records no retry");
    assert_eq!(retrying_count(&mut rx), 0);
}

/// The deadline runs from the episode's start, not from this call: an episode
/// whose `retry_timeout` is spent fails the round without scheduling a retry.
#[tokio::test]
async fn the_deadline_is_measured_from_the_episode_start() {
    let server = MockServer::start().await;
    mount_status(&server, 500).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();
    let policy = RetryPolicy {
        retry_timeout: Duration::from_secs(1),
        ..fast_policy(3)
    };
    let mut episode = Episode::new(timefmt::now_epoch() - 5);

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &policy,
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

    assert!(matches!(result, Err(RequestFailure::Failover(_))));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(
        retry_log.is_empty(),
        "no retry is recorded when the deadline is spent"
    );
    assert_eq!(retrying_count(&mut rx), 0);
}

/// The wait before a retry is capped by what is left of the deadline, so a
/// recorded `delay_secs` is the wait that actually happened.
#[tokio::test]
async fn the_recorded_wait_is_capped_by_the_remaining_deadline() {
    let server = MockServer::start().await;
    mount_status(&server, 503).await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();
    // The backoff schedule asks for 30s; the episode has a few seconds left.
    let policy = RetryPolicy {
        max_retries: 1,
        base_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(30),
        retry_timeout: Duration::from_secs(10),
    };
    let mut episode = Episode::new(timefmt::now_epoch() - 7);

    let result = stream_with_retry(
        RetryCall {
            conversation: &mut conversation,
            request: request("gpt-4.1-mini", vec![user_msg("hi")]),
            policy: &policy,
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

    assert!(matches!(result, Err(RequestFailure::Failover(_))));
    assert_eq!(retry_log.len(), 1);
    assert_eq!(retrying_count(&mut rx), 1);
    let waited = retry_log[0].delay_secs;
    assert!(
        (0.5..=3.0).contains(&waited),
        "the wait is the remaining deadline, not the schedule: {waited}"
    );
}

/// A rejection that names when service resumes hands that moment to the
/// record: it is the offline aggregation face, so the upstream's own value
/// rides it as given.
#[tokio::test]
async fn a_quota_rejection_records_the_reset_hint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string("{\"error\":\"slow down\",\"retry_after\":120}"),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let mut conversation = client.conversation();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let mut retry_log = Vec::new();
    let before = timefmt::now_epoch();

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

    assert!(matches!(result, Err(RequestFailure::Switch { .. })));
    assert_eq!(retry_log.len(), 1, "one rejection, one record");
    assert!(matches!(retry_log[0].kind, RetryKind::RateLimit));
    assert_eq!(retry_log[0].delay_secs, 0.0);
    let reset = retry_log[0].quota_reset.expect("the hint rides the record");
    assert!(reset >= before + 120, "the upstream's own moment: {reset}");
}
