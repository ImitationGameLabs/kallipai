//! LLM request retry with exponential backoff.
//!
//! Retries transient failures (network errors, HTTP 408/5xx) at the
//! `just_llm_client::Conversation` boundary: each attempt asks the conversation to open a
//! streaming generation and classifies the resulting
//! [`BackendError`](just_llm_client::BackendError) through the `decision` table — by HTTP
//! status for provider rejections, by variant otherwise. Only the transient classes retry:
//! a quota or payment rejection (402, a 403 carrying quota evidence, or a 429 that names its
//! reset moment) advances the failover chain instead, and the endpoint-level permanent
//! classes (401/403/404) do the same. A 429 that names no reset moment is a throttle: it
//! spends this episode's allowance on the same endpoint first. All of them write what they
//! learned to the shared upstream fact table. The retry budget is the episode's own attempt
//! count ([`Episode`]), not the persisted retry log: `max_retries` bounds the sends of one
//! episode, `retry_timeout` bounds how long that episode stays on trial. The server's
//! `retry-after` header is not reachable through the conversation, so backoff is the
//! computed schedule only. A mid-stream failure is handled by the acquisition loop, which
//! continues the same episode.
//!
//! The duties are one facet per file:
//!
//! - `budget`: what one episode is allowed to spend — the knobs, the send
//!   count they imply, the shape slot and the fact a settled failure writes,
//!   and the local error a spent budget produces.
//! - `wait`: how long the wait before the next send is, and the sleep itself.
//! - `stream`: the loop that performs the verdict — what one call hands in
//!   ([`RetryCall`]), what it hands back ([`Sent`]), and the failure it hands
//!   back when it gives up ([`RequestFailure`]).
//! - `decision`: the pure core — the class table, the error classifier, the
//!   episode ledger, and the verdict the loop performs.
//!
//! Most callers reach these items through this file. The `decision` module stays
//! `pub(crate)`, so the acquisition and context code that works with its table
//! types reaches them directly.

mod budget;
pub(crate) mod decision;
mod stream;
mod wait;

#[cfg(test)]
mod test_support;

pub use budget::RetryPolicy;
pub use decision::{AttemptOrigin, Episode};
pub use stream::{RequestFailure, RetryCall, Sent, stream_with_retry};

pub(crate) use budget::{max_sends, record_fact, spend_shape_slot, spent_budget_error};
pub(crate) use wait::{RETRY_JITTER_MAX, backoff_delay, backoff_schedule, jitter_now};
