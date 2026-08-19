//! Default values and bounds for the runtime configuration knobs.
//!
//! Plain defaults for every env-tunable knob plus the upper-bound guards
//! `config::load` enforces. Imported back into `config` with a private
//! `use` — visible within config's tree only, not public API.
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
/// Effectively unlimited — the real safety net is the tagma-wide token budget.
/// Individual rounds are bounded by LLM response length; the loop as a whole is
/// bounded by token consumption. This constant only serves as a last-resort
/// guard against a degenerate "tool calls with no progress" loop.
pub(crate) const DEFAULT_MAX_TOOL_ROUNDS: usize = usize::MAX;
/// Default cap on consecutive heartbeat rounds (bare-assistant re-loops) before
/// the harness force-idles the agent. Bounds "self-monologue" token burn; the
/// tagma-wide token budget remains the overall hard ceiling. Three is a firm
/// nudge: one accidental bare response, a reminder, then a stop.
pub(crate) const DEFAULT_MAX_HEARTBEAT_ROUNDS: u32 = 3;
/// Default cap on consecutive transient (failover-chain-exhausted) parks that get
/// a timed retry. After this (or the `retry_timeout` wall clock), the agent hard-
/// parks and surfaces to the operator instead of re-hammering a downed provider.
pub(crate) const DEFAULT_MAX_TRANSIENT_RETRIES: u32 = 3;
pub(crate) const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 1_200;
pub(crate) const DEFAULT_OUTPUT_RESERVE_TOKENS: usize = 8_192;
pub(crate) const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
pub(crate) const DEFAULT_MAX_RETRIES: u32 = 10;
pub(crate) const DEFAULT_RETRY_BASE_DELAY_SECS: u64 = 1;
pub(crate) const DEFAULT_RETRY_MAX_DELAY_SECS: u64 = 60;
pub(crate) const DEFAULT_RETRY_TIMEOUT_SECS: u64 = 300;
/// Upper bound for `KALLIP_MAX_RETRIES` (default is 10). Closes the overflow in
/// retry.rs's `saturating_sub(prior) + 1`: at `u32::MAX` the release build wraps
/// to zero attempts — a silent zero-send inversion. 1000 retries at the 1s
/// floor delay is already 16+ minutes of pure waiting, beyond any operational
/// scenario, with ample headroom over the default.
pub(crate) const MAX_RETRIES_LIMIT: u32 = 1_000;
/// Upper bound for the two delay knobs (base default 1s, cap default 60s).
/// The base delay's jitter computes `as_nanos() as u64`; at 3600s the value
/// is 3.6e12 ns — well under u64::MAX (~1.8e19), so the u128->u64 cast is
/// exact; combined with the existing >0 bail, the jitter's modulo divisor
/// is never zero. A single backoff over an hour under the 24h total window
/// is a unit error, not a scenario.
pub(crate) const RETRY_DELAY_SECS_LIMIT: u64 = 3_600;
/// Upper bound for `KALLIP_RETRY_TIMEOUT_SECS` (default 300). The deadline is
/// built as `Instant::now() + Duration`; an extreme value panics on addition.
/// 24h covers leave-it-overnight operations.
pub(crate) const RETRY_TIMEOUT_SECS_LIMIT: u64 = 86_400;
pub(crate) const DEFAULT_PINNED_BUDGET_RATIO: f64 = 0.25;
pub(crate) const DEFAULT_CONTEXT_THRESHOLDS: &[u8] = &[50, 60, 70, 80];
pub(crate) const DEFAULT_TOKEN_BUDGET_WARNINGS: &[u8] = &[80, 95];
