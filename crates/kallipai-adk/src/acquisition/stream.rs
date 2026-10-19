//! Stream consumption and the result vocabulary of an acquisition.
//!
//! Consume one SSE stream: accumulate content, reasoning, tool calls and usage, and
//! report how it ended — completed, cancelled, or dropped mid-way with the transport
//! error. The acquisition loop that drives it lives in `super::phases`; the types here
//! are what that loop, the round loop and the tool execution path pass around.

use crate::event::{AgentEvent, AgentOutcome};
use crate::stream_accumulator::ToolCallAccumulator;
use anyhow::Error;
use futures_util::StreamExt;
use just_llm_client::types::generation::{GenerationEvent, ToolCall};
use just_llm_client::{BackendError, ConversationStream, provider_rejection};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Outcome of consuming an LLM response stream.
pub(super) enum StreamOutcome {
    /// The stream was cancelled mid-stream.
    Cancelled,
    /// The stream completed normally.
    Completed(StreamConsumed),
    /// The stream dropped mid-way with a transport error. The partial content accumulated so far
    /// is abandoned (already-emitted deltas are void); the caller re-sends the same request:
    /// with an anchor kept, that resend is a delta continuation, and a send that dropped before
    /// any anchor arrived is the full context again. Carries the error so the caller can drive
    /// retry/failover and surface a diagnostic.
    Transient(just_llm_client::TransportError),
}

/// Data accumulated from a completed LLM response stream.
pub(crate) struct StreamConsumed {
    pub(crate) content: String,
    pub(crate) reasoning: String,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) usage: Option<just_llm_client::types::generation::Usage>,
    /// The assembled response of a stream that reached its `End` event carrying a response
    /// id — the adoptable turn record. `None` when the backend is stateless (no id on
    /// `End`) or the stream never completed; the caller adopts only when this is `Some`.
    /// Boxed so this struct stays small next to the variants that carry it.
    pub(crate) response: Option<Box<just_llm_client::types::generation::GenerationResponse>>,
}

/// Consume an SSE stream, accumulating content, reasoning, tool calls, and usage.
///
/// Takes ownership of the stream; the assembled response rides the `Completed` outcome.
/// Returns `Cancelled` if the cancellation token fires mid-stream.
pub(super) async fn consume_stream(
    mut stream: ConversationStream,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    cancel: &CancellationToken,
) -> StreamOutcome {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_acc = ToolCallAccumulator::new();
    let mut response_usage: Option<just_llm_client::types::generation::Usage> = None;

    loop {
        tokio::select! {
            event_result = stream.next() => {
                let event = match event_result {
                    Some(Ok(e)) => e,
                    Some(Err(e)) => {
                        // Mid-stream transport drop (connection reset, h2 error, premature EOF, ...).
                        // The partial content already emitted via deltas is void; the caller re-sends
                        // the same request and emits a `StreamReset` so downstream folds/discards it.
                        warn!(
                            error = %crate::llm_error::render_error(&e),
                            "LLM stream dropped mid-stream"
                        );
                        return StreamOutcome::Transient(e);
                    }
                    None => break,
                };
                match event {
                    GenerationEvent::Text { delta } => {
                        content.push_str(&delta);
                        tx.send(AgentEvent::AssistantContentDelta { delta })
                            .await
                            .ok();
                    }
                    GenerationEvent::Reasoning { delta } => {
                        reasoning.push_str(&delta);
                        tx.send(AgentEvent::ReasoningDelta { delta }).await.ok();
                    }
                    GenerationEvent::ToolCall { delta } => {
                        tool_acc.push(&delta);
                    }
                    GenerationEvent::Usage { usage } => {
                        response_usage = Some(usage);
                    }
                    // Stream close, not `End`, terminates the loop; `End` only carries the
                    // finish reason / response id, which the round loop does not consume yet.
                    GenerationEvent::End { .. } => {}
                }
            }
            _ = cancel.cancelled() => {
                tracing::info!("LLM stream cancelled mid-stream");
                return StreamOutcome::Cancelled;
            }
        }
    }

    // The stream ran to its end: the loop above reads on past `End` until the
    // stream reports `None`. Stopping at `End` and dropping the stream there
    // would leave the response body unread, so the connection closes instead of
    // being reused. Take the assembled response for the caller to adopt: `None`
    // = no `End` carrying an id — a stateless backend, or a stream that ended
    // without one — so there is nothing to adopt.
    let response = stream.into_response().map(Box::new);
    StreamOutcome::Completed(StreamConsumed {
        content,
        reasoning,
        tool_calls: tool_acc.finish(),
        usage: response_usage,
        response,
    })
}

/// Outcome of the within-set failover acquisition loop.
pub(crate) enum AcquireResult {
    /// A stream was acquired AND fully consumed — proceed to post-stream budgeting / tool calls.
    Consumed(StreamConsumed),
    /// A terminal round outcome (chain exhausted / cancelled / budget exceeded).
    Outcome(AgentOutcome),
    /// A request-level error — the round errors.
    Error(Error),
    /// The chain cannot help: stop the round with this message (no candidate
    /// left, or a local misconfiguration). Crate-internal — it never reaches
    /// the wire.
    Stop { message: String },
}

/// The provider HTTP status behind a request-level acquisition error, if
/// any: a `400`/`422`-family rejection that failed on this profile.
/// Auth/404 statuses never reach the Fatal arm (they fail over instead)
/// and pre-flight serialization failures carry no status — both return
/// `None`, so the caller's image-degrade path only fires on true
/// provider rejections.
pub(super) fn fatal_provider_status(error: &Error) -> Option<reqwest::StatusCode> {
    let backend = error.downcast_ref::<BackendError>()?;
    provider_rejection(backend)
        .map(|rejection| rejection.status)
        .filter(|status| {
            status.is_client_error()
                && *status != reqwest::StatusCode::UNAUTHORIZED
                && *status != reqwest::StatusCode::FORBIDDEN
                && *status != reqwest::StatusCode::NOT_FOUND
        })
}
