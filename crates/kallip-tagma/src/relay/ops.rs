//! Pure helpers for the relay op path: the reply burst limiter and the
//! req_id/op-error helpers. Ported from the former standalone connector. The
//! SSE→external-event projection lives in [`crate::projector`] (shared with
//! the direct serving path).

use std::time::Duration;

use kallip_agora_common::ids::TraceId;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::message::TagmaReply;

/// Trace id stamped on every event-pump envelope (the pump is one logical
/// producer, not correlated to any single op `req_id`).
pub(super) const PUMP_TRACE: &str = "tagma:pump";

/// Backoff between lesche tunnel reconnect attempts.
pub(super) const TUNNEL_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Map a tagma error to an op `Error` reply, preserving the tagma's HTTP status
/// when the error carries one (otherwise 502 bad gateway). Only the `ApiError`'s
/// HTTP-facing message crosses the E2E boundary; arbitrary anyhow chains (which
/// may carry internal paths or source detail) are reduced to a fixed string and
/// logged server-side by `handle_user_op`.
pub(super) fn op_err_reply(req_id: u64, e: &anyhow::Error) -> TagmaReply {
    let (status, message) = match e.downcast_ref::<ApiError>() {
        Some(a) => (a.status, a.message.clone()),
        None => (502, "tagma op failed".to_string()),
    };
    TagmaReply::Error {
        req_id,
        status,
        message,
    }
}

/// A trace id for an inbound op, keyed by its `req_id`.
pub(super) fn op_trace(req_id: u64) -> TraceId {
    TraceId::from(format!("op:{req_id}"))
}

/// Configured message-burst limits. Process-global today: there is exactly one
/// root agent (one conversation), so per-process == per-conversation. A future
/// multi-root relay would scope this per agent/turn.
#[derive(Clone, Copy)]
pub(crate) struct MessageLimits {
    /// Max message deliveries per window. Generous for legitimate multi-part
    /// messages, tight enough to bound a runaway loop.
    pub max: u32,
    /// The fixed window length.
    pub window: Duration,
}

impl Default for MessageLimits {
    fn default() -> Self {
        Self {
            max: DEFAULT_MESSAGE_BURST_MAX,
            window: DEFAULT_MESSAGE_BURST_WINDOW,
        }
    }
}

/// Default cap unless `KALLIP_TAGMA_RELAY_MESSAGE_BURST_MAX` overrides it.
pub(crate) const DEFAULT_MESSAGE_BURST_MAX: u32 = 20;
/// Default window unless `KALLIP_TAGMA_RELAY_MESSAGE_BURST_WINDOW_SECS` overrides it.
pub(crate) const DEFAULT_MESSAGE_BURST_WINDOW: Duration = Duration::from_secs(10);

/// A simple fixed-window burst limiter for message deliveries: at most
/// `limits.max` deliveries per `limits.window`. Bounds a runaway agent loop
/// without needing round-level semantics (the relay does not see rounds).
pub(crate) struct MessageLimiter {
    limits: MessageLimits,
    /// Start of the current window (seconds since UNIX epoch).
    window_start: u64,
    /// Deliveries counted in the current window.
    count: u32,
}

impl MessageLimiter {
    pub(crate) fn new(limits: MessageLimits) -> Self {
        Self {
            limits,
            window_start: 0,
            count: 0,
        }
    }

    pub(crate) fn check(&mut self) -> bool {
        let now = unix_secs();
        if now.saturating_sub(self.window_start) >= self.limits.window.as_secs() {
            self.window_start = now;
            self.count = 0;
        }
        if self.count >= self.limits.max {
            return false;
        }
        self.count += 1;
        true
    }
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
