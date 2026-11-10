//! Fixtures the retry behaviour tests share: a mock-server client, a fast
//! policy, the two status mounts, the retrying-event counter, and a turn runner
//! that drains a stream to completion.

use std::time::Duration;

use futures_util::StreamExt;
use just_llm_client::types::generation::GenerationRequest;
use just_llm_client::{
    Conversation, GenerationClientOptions, LlmBackend, provider::OpenAiCompatBackend,
};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use crate::event::AgentEvent;
use crate::profile::GenerationClient;
use crate::retry::{Episode, RequestFailure, RetryCall, RetryPolicy, stream_with_retry};
use kallipai_common::timefmt;

/// A real OpenAI-compatible backend pointed at a mock server, wrapped in a `GenerationClient`.
pub(super) fn mock_client(server: &MockServer) -> GenerationClient {
    let backend = OpenAiCompatBackend::new(
        reqwest::Client::builder().use_rustls_tls(),
        "test-key",
        Some(&server.uri()),
    )
    .expect("openai-compat backend constructs without network");
    GenerationClient::new(backend, GenerationClientOptions::new("gpt-4.1-mini"))
}

/// Fast policy so the suite stays snappy; caller sets `max_retries` per scenario.
pub(super) fn fast_policy(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
        retry_timeout: Duration::from_secs(10),
    }
}

/// Count `AgentEvent::Retrying` events queued on `rx`.
pub(super) fn retrying_count(rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>) -> usize {
    let mut n = 0;
    while let Ok(AgentEvent::Retrying { .. }) = rx.try_recv() {
        n += 1;
    }
    n
}

pub(super) async fn mount_status(server: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

/// The same mount, answering with a body: where a quota row names the moment
/// it can be used again.
pub(super) async fn mount_status_with_body(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("content-type", "application/json")
                .set_body_string(body.to_owned()),
        )
        .mount(server)
        .await;
}

/// Drive one turn through `stream_with_retry`, drain the stream to completion, and
/// adopt the assembled response — the explicit three-step contract the production
/// runner follows (`consume_stream` drains, `acquire_stream` adopts).
pub(super) async fn run_turn(
    conversation: &mut Conversation,
    request: GenerationRequest,
) -> Result<(), RequestFailure> {
    let mut stream = stream_with_retry(
        RetryCall {
            conversation,
            request,
            policy: &fast_policy(2),
            round: 0,
            episode: &mut Episode::new(timefmt::now_epoch()),
            provider_id: "test",
            profile_id: "test",
            pacing: None,
            metrics: &Default::default(),
        },
        &tokio::sync::mpsc::channel(16).0,
        &mut Vec::new(),
        CancellationToken::new(),
    )
    .await?
    .stream;
    while let Some(event) = stream.next().await {
        event.expect("turn events are healthy");
    }
    let response = stream
        .into_response()
        .expect("a drained test stream reaches `End` with a response id");
    conversation
        .adopt(response)
        .expect("the just-prepared turn adopts cleanly");
    Ok(())
}
