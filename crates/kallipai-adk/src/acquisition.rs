//! LLM stream acquisition for the agent round loop.
//!
//! Owns the within-set acquisition loop: consume an SSE stream, retry mid-stream
//! transport drops in place, and advance the failover chain when an endpoint or the
//! retry budget gives out. The [`crate::failover::FailoverState`] state machine stays
//! pure in its own module — this module drives it, and
//! [`crate::acquisition::failover::advance_failover`] is the sole driver of chain
//! advancement. The round loop in `crate::runner` calls
//! [`crate::acquisition::phases::acquire_stream`] and dispatches on the result.
//!
//! The work lives in submodules: `phases` (the acquisition loop and the stages it drives),
//! `stream` (one stream's consumption and the result vocabulary), `facts` (the shared
//! table's reading and the signals it fires) and `failover` (the chain advance and its
//! vocabulary). The re-exports below keep the names the round loop,
//! the tool execution path, the runner tests and the crate root import resolving under
//! this path: the runner tests through the test-only re-export of the failover driver,
//! and the crate root through the gateway signal type it hands on to the tagma. The
//! compaction recorder imports the `retry_keep_since` helper kept here, which this module
//! defines rather than re-exports.

mod facts;
mod failover;
mod phases;
mod stream;
#[cfg(test)]
mod tests;

use crate::agent_task::AgentContext;
use kallipai_common::retry::RetryRecord;
use kallipai_common::timefmt;

pub use self::facts::GatewaySignal;
pub(crate) use self::facts::{claim_window, release_unspent_window};

// The one channel that takes a window, and the one that gives an unspent
// one back: the compaction entry and the send path share the first, and
// the episode boundaries use the second.
#[cfg(test)]
pub(crate) use self::failover::advance_failover;
pub(crate) use self::phases::{acquire_stream, degrade_on_image_rejection};
pub(crate) use self::stream::{AcquireResult, StreamConsumed};

/// Unix-seconds cutoff for retry-log retention: records older than the retry
/// window are pruned on append. The window is a log-retention policy only:
/// the episode ledger, not the log, is what bounds retries.
/// The compaction recorder shares it so both writers keep one log under
/// one cutoff.
pub(crate) fn retry_keep_since(ctx: &AgentContext) -> u64 {
    timefmt::now_epoch().saturating_sub(ctx.config.retry_policy.retry_timeout.as_secs())
}

/// Persist any buffered retry records for this endpoint into the store. A no-op when empty.
async fn flush_retry_records(ctx: &mut AgentContext, retry_records: &mut Vec<RetryRecord>) {
    if retry_records.is_empty() {
        return;
    }
    ctx.store
        .lock()
        .await
        .record_retries(std::mem::take(retry_records), retry_keep_since(ctx));
    ctx.persist().await;
}
