//! Default values and bounds for the runtime configuration knobs.
//!
//! Plain defaults for every env-tunable knob plus the upper-bound guards
//! `config::load` enforces. Imported back into `config` with a private
//! `use` — visible within config's tree only, not public API.
use std::time::Duration;

pub(crate) const DEFAULT_SYSTEM_PROMPT: &str = concat!(
    "# Posture\n\n",
    "Keep answers concise. Prefer the least risky tool that accomplishes the task; ",
    "each tool's own description explains its usage.\n\n",
    "# Tool and round model\n\n",
    "Some tool actions are asynchronous — a backgrounded task or a deferred ",
    "(pending-approval) action completes later and surfaces a notice in context; ",
    "read the notice and follow its instruction. Tool calls within one round run ",
    "in order; if a call does not succeed cleanly (non-zero exit, denied, timed ",
    "out, or deferred pending approval) the remaining calls in that round are ",
    "skipped and returned as errors — re-issue them after reviewing what happened.",
);
/// Effectively unlimited — the real safety net is the instance-wide token budget.
/// Individual rounds are bounded by LLM response length; the loop as a whole is
/// bounded by token consumption. This constant only serves as a last-resort
/// guard against a degenerate "tool calls with no progress" loop.
pub(crate) const DEFAULT_MAX_TOOL_ROUNDS: usize = usize::MAX;
/// Default cap on consecutive heartbeat rounds (bare-assistant re-loops) before
/// the harness force-idles the agent. Bounds "self-monologue" token burn; the
/// instance-wide token budget remains the overall hard ceiling. Three is a firm
/// nudge: one accidental bare response, a reminder, then a stop.
pub(crate) const DEFAULT_MAX_HEARTBEAT_ROUNDS: u32 = 3;
/// Default cap on consecutive transient (failover-chain-exhausted) failures that
/// earn a timed re-run. Each re-run opens a fresh retry episode, so the retry
/// policy's `retry_timeout` does not bound the whole sequence: this counter is
/// the only cap. Past it the agent parks with `TransientRetryExhausted` and
/// surfaces to the operator instead of re-hammering a downed provider.
pub(crate) const DEFAULT_MAX_TRANSIENT_RETRIES: u32 = 3;
pub(crate) const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 1_200;
pub(crate) const DEFAULT_OUTPUT_RESERVE_TOKENS: usize = 8_192;
pub(crate) const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
pub(crate) const DEFAULT_MAX_RETRIES: u32 = 10;
pub(crate) const DEFAULT_RETRY_BASE_DELAY_SECS: u64 = 1;
pub(crate) const DEFAULT_RETRY_MAX_DELAY_SECS: u64 = 60;
pub(crate) const DEFAULT_RETRY_TIMEOUT_SECS: u64 = 300;

/// Assumed worst-case wall clock of one LLM call, in seconds. The retry-budget
/// check charges `(max_retries + 1) x` this value. It is a design assumption,
/// not a system guarantee: a call has no total-duration timeout (the transport
/// caps a connect at 30s and an idle read at 120s, while a streaming body has no
/// bound at all). The qualitative conclusion — the factory set is cut by its own
/// deadline — holds for any non-negative value.
pub(crate) const CALL_WORST_ASSUMED: Duration = Duration::from_secs(60);
/// Upper bound for `KALLIPAI_MAX_RETRIES` (default is 10). The attempt arithmetic saturates
/// (`max_sends` adds one to the count), so the bound is a sanity cap on the knob rather than
/// an overflow guard: 1000 retries at the 1s floor delay is already 16+ minutes of pure
/// waiting, beyond any operational scenario, with ample headroom over the default.
pub(crate) const MAX_RETRIES_LIMIT: u32 = 1_000;
/// Upper bound for the two delay knobs (base default 1s, cap default 60s).
/// The schedule is the only thing this bound has to hold: a wait adds the
/// core's own jitter cap, which does not grow with the base delay. A single
/// backoff over an hour under the 24h total window is a unit error, not a
/// scenario.
pub(crate) const RETRY_DELAY_SECS_LIMIT: u64 = 3_600;
/// Upper bound for `KALLIPAI_RETRY_TIMEOUT_SECS` (default 300). The deadline is
/// built as `Instant::now() + Duration`; an extreme value panics on addition.
/// 24h covers leave-it-overnight operations.
pub(crate) const RETRY_TIMEOUT_SECS_LIMIT: u64 = 86_400;
pub(crate) const DEFAULT_PINNED_BUDGET_RATIO: f64 = 0.25;
pub(crate) const DEFAULT_CONTEXT_THRESHOLDS: &[u8] = &[50, 60, 70, 80];
pub(crate) const DEFAULT_TOKEN_BUDGET_WARNINGS: &[u8] = &[80, 95];

/// Tool results at or under this estimate enter context untouched.
pub(crate) const DEFAULT_TOOL_RESULT_FULL_TOKENS: usize = 30_000;
/// Above the full-result threshold a result is cut to roughly this many
/// tokens (head+tail slice plus a banner describing what was kept). Both
/// caps are system safety parameters, not user knobs: they are validated
/// against the summarizer input budget in `config::check_context_budget`
/// (startup and failover), because a cap loose enough to survive that
/// budget would re-create the oversized-turn compaction wedge.
pub(crate) const DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS: usize = 10_000;
/// A line at or under this estimate is kept whole when a slice cut lands
/// inside it (line-aligned slicing in `text_slice`); bigger lines degrade to
/// char cuts. Token-based, not char-based, so the whole-line slack on the cut
/// is bounded in budget terms for both densities — the worst alignment
/// overshoot stays a fraction of the truncated cap.
pub(crate) const DEFAULT_TOOL_RESULT_LINE_MAX_TOKENS: usize = 2_000;

/// External messages (peer prompts, inbox deliveries) entering the turn
/// history at or under this estimated-token size pass through byte-for-byte;
/// anything larger is cut like an oversized tool result and its full
/// original spilled to disk. Same budget family as the tool caps: validated
/// against the same summarizer input budget, same wedge math.
pub(crate) const DEFAULT_MESSAGE_FULL_TOKENS: usize = DEFAULT_TOOL_RESULT_FULL_TOKENS;
