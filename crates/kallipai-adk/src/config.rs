use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::context::KeepCaps;
use crate::env_util::{DEFAULT_CONTEXT_WINDOW_TOKENS, parse_env, parse_env_list};
use crate::retry::decision::{RETRY_JITTER_MAX, backoff_schedule};
use crate::retry::{RetryPolicy, max_sends};
use kallipai_common::AgentId;
use kallipai_common::protocol;

mod defaults;
use defaults::{
    CALL_WORST_ASSUMED, DEFAULT_CONTEXT_KEEP_TOKENS_RATIO, DEFAULT_CONTEXT_KEEP_TURNS,
    DEFAULT_CONTEXT_THRESHOLDS, DEFAULT_MAX_HEARTBEAT_ROUNDS, DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TOOL_ROUNDS, DEFAULT_MAX_TRANSIENT_RETRIES, DEFAULT_OUTPUT_RESERVE_TOKENS,
    DEFAULT_PINNED_BUDGET_RATIO, DEFAULT_RETRY_BASE_DELAY_SECS, DEFAULT_RETRY_MAX_DELAY_SECS,
    DEFAULT_RETRY_TIMEOUT_SECS, DEFAULT_SUMMARY_MAX_TOKENS, DEFAULT_SYSTEM_PROMPT,
    DEFAULT_TOKEN_BUDGET_WARNINGS, DEFAULT_TOOL_TIMEOUT_SECS, MAX_RETRIES_LIMIT,
    RETRY_DELAY_SECS_LIMIT, RETRY_TIMEOUT_SECS_LIMIT,
};
pub(crate) use defaults::{
    DEFAULT_MESSAGE_FULL_TOKENS, DEFAULT_TOOL_RESULT_FULL_TOKENS,
    DEFAULT_TOOL_RESULT_LINE_MAX_TOKENS, DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS,
};
mod env;
mod exec_hooks;
pub use env::{permission_class_from_env, policy_preset_from_env};
pub use exec_hooks::{builtin_exec_hook_rules, load_exec_hook_rules};
mod permissions;
pub use permissions::{
    DEFAULT_MAX_DEPTH, DelegationMode, ParsePermissionClassError, PermissionClass,
    PermissionProfile,
};
/// Runtime configuration for `kallip`.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub prompt: Option<String>,
    pub system_prompt: String,
    pub max_tool_rounds: usize,
    /// Max consecutive heartbeat rounds (bare-assistant re-loops) before the
    /// harness force-idles. The sole bound on bare-assistant storms —
    /// `max_tool_rounds` does not bound heartbeats (each heartbeat re-enters the
    /// round loop fresh). Default 3 (`KALLIPAI_MAX_HEARTBEAT_ROUNDS`).
    pub max_heartbeat_rounds: u32,
    /// Max consecutive transient (failover-chain-exhausted) parks that earn a
    /// timed retry before the agent hard-parks and surfaces. Default 3
    /// (`KALLIPAI_MAX_TRANSIENT_RETRIES`).
    pub max_transient_retries: u32,
    pub workspace_root: PathBuf,
    pub context_window_tokens: usize,
    pub output_reserve_tokens: usize,
    pub summary_max_tokens: u32,
    pub tool_timeout_secs: u64,
    pub skills: Vec<String>,
    pub retry_policy: RetryPolicy,
    pub pinned_budget_ratio: f64,
    /// Conversation turns a manual eviction keeps. `0` means none: the whole
    /// conversation goes.
    pub context_keep_turns: usize,
    /// Share of the effective budget those turns may spend, derived into
    /// [`Self::context_keep_tokens`] so a profile that swaps the window moves
    /// the cap with it instead of pinning an absolute.
    pub context_keep_tokens_ratio: f64,
    pub context_thresholds: Vec<u8>,
    /// Thresholds (as percentages 1-99) at which to warn the LLM about
    /// approaching token budget exhaustion.
    pub token_budget_warnings: Vec<u8>,
    pub agent_id: Option<AgentId>,
    pub created_by: Option<AgentId>,
    pub permissions: PermissionProfile,
    /// The profile set this agent resolves against (by name). `None` marks a
    /// record written before set binding existed — restore tolerates it
    /// (the agent surfaces as unspecified), but every spawn sets it.
    pub profile_set: Option<String>,
    /// FS-access permission class (Guest readonly / Normal home-rw) — the static
    /// baseline axis of the sandbox. Defaults to Normal; spawn requires it
    /// explicitly (never above the supervisor's own class) and restore re-validates
    /// it against the supervisor chain. Unlike `role`/`description`, this is a
    /// safety invariant, not display metadata.
    ///
    /// Spelled `permissions_class` (plural) here for historical reasons; the
    /// wire/protocol field that sets it on a subagent spawn is the singular
    /// `permission_class` on `CreateAgentRequest` — same value, two names by
    /// layer (internal config vs client-facing wire form).
    pub permissions_class: PermissionClass,
    /// Short display label ("researcher"). Supervisor-owned; set at spawn (and
    /// via `PUT /agents/{id}/metadata`), persisted in `AgentMeta`. Required
    /// non-empty for subagent spawns. Not read by the runtime — pure display
    /// metadata, grouped here with the other identity fields (`agent_id`,
    /// `created_by`) per the `AgentMeta` precedent.
    pub role: String,
    /// Longer prose ("gathers sources for the plan"). Supervisor-owned, optional.
    pub description: String,
    /// How this subagent relates to its supervisor's workspace write-lock
    /// (`CarveOut` subdir vs `FullHandoff` whole-workspace). Defaults to
    /// `CarveOut`; set at spawn from the request, persisted in `AgentMeta`,
    /// re-read on restore. Root is always `CarveOut`.
    pub delegation_mode: DelegationMode,
}

impl Default for AgentConfig {
    /// Field defaults mirroring the env-unset branches of [`Self::load`]. Used to
    /// construct a placeholder config for a faulted registry entry (which never
    /// runs, so the runtime knobs are irrelevant) and to keep test literals small.
    /// The identity fields (`agent_id`, `created_by`, `role`, `description`,
    /// `workspace_root`, `permissions_class`, `profile_set`) default to
    /// empty/None and are overwritten by the caller.
    fn default() -> Self {
        Self {
            prompt: None,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
            max_heartbeat_rounds: DEFAULT_MAX_HEARTBEAT_ROUNDS,
            max_transient_retries: DEFAULT_MAX_TRANSIENT_RETRIES,
            workspace_root: PathBuf::new(),
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            output_reserve_tokens: DEFAULT_OUTPUT_RESERVE_TOKENS,
            summary_max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
            tool_timeout_secs: DEFAULT_TOOL_TIMEOUT_SECS,
            skills: Vec::new(),
            retry_policy: RetryPolicy::default(),
            pinned_budget_ratio: DEFAULT_PINNED_BUDGET_RATIO,
            context_keep_turns: DEFAULT_CONTEXT_KEEP_TURNS,
            context_keep_tokens_ratio: DEFAULT_CONTEXT_KEEP_TOKENS_RATIO,
            context_thresholds: DEFAULT_CONTEXT_THRESHOLDS.to_vec(),
            token_budget_warnings: DEFAULT_TOKEN_BUDGET_WARNINGS.to_vec(),
            agent_id: None,
            created_by: None,
            permissions: PermissionProfile::new(PathBuf::new()),
            profile_set: None,
            permissions_class: PermissionClass::Normal,
            role: String::new(),
            description: String::new(),
            delegation_mode: DelegationMode::CarveOut,
        }
    }
}

impl AgentConfig {
    /// Whether this is the host-owned root agent — the unique agent minted
    /// with no creator (`created_by == None`). Root is the sole role allowed
    /// to author shared skills (the landlock carve in `build_tool_dispatch`
    /// keys off this), and is the agent looked up by `root_agent()` for
    /// relay/restore routing. One predicate so the definition of "root"
    /// stays in one place.
    pub fn is_root(&self) -> bool {
        self.created_by.is_none()
    }

    /// Loads configuration from CLI arguments and environment variables.
    pub fn load(
        prompt: Option<String>,
        skills: Vec<String>,
        workspace_root: Option<PathBuf>,
    ) -> Result<Self> {
        let system_prompt = std::env::var("KALLIPAI_SYSTEM_PROMPT")
            .unwrap_or_else(|_| DEFAULT_SYSTEM_PROMPT.into());
        let max_tool_rounds =
            parse_env::<usize>("KALLIPAI_MAX_TOOL_ROUNDS")?.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS);
        let max_heartbeat_rounds = parse_env::<u32>("KALLIPAI_MAX_HEARTBEAT_ROUNDS")?
            .unwrap_or(DEFAULT_MAX_HEARTBEAT_ROUNDS);
        let max_transient_retries = parse_env::<u32>("KALLIPAI_MAX_TRANSIENT_RETRIES")?
            .unwrap_or(DEFAULT_MAX_TRANSIENT_RETRIES);
        let workspace_root = workspace_root
            .or_else(|| {
                std::env::var("KALLIPAI_WORKSPACE_ROOT")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?);
        // Shape-check anchor: the active profile's declared window is installed at spawn
        let context_window_tokens = DEFAULT_CONTEXT_WINDOW_TOKENS;
        let output_reserve_tokens = parse_env::<usize>("KALLIPAI_OUTPUT_RESERVE_TOKENS")?
            .unwrap_or(DEFAULT_OUTPUT_RESERVE_TOKENS);
        let summary_max_tokens =
            parse_env::<u32>("KALLIPAI_SUMMARY_MAX_TOKENS")?.unwrap_or(DEFAULT_SUMMARY_MAX_TOKENS);
        let tool_timeout_secs =
            parse_env::<u64>("KALLIPAI_TOOL_TIMEOUT_SECS")?.unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS);

        let pinned_budget_ratio = parse_env::<f64>("KALLIPAI_PINNED_BUDGET_RATIO")?
            .unwrap_or(DEFAULT_PINNED_BUDGET_RATIO);
        let context_keep_turns = parse_env::<usize>("KALLIPAI_CONTEXT_KEEP_TURNS")?
            .unwrap_or(DEFAULT_CONTEXT_KEEP_TURNS);
        let context_keep_tokens_ratio = parse_env::<f64>("KALLIPAI_CONTEXT_KEEP_TOKENS_RATIO")?
            .unwrap_or(DEFAULT_CONTEXT_KEEP_TOKENS_RATIO);
        let context_thresholds = parse_env_list::<u8>("KALLIPAI_CONTEXT_THRESHOLDS")?
            .unwrap_or_else(|| DEFAULT_CONTEXT_THRESHOLDS.to_vec());
        let token_budget_warnings = parse_env_list::<u8>("KALLIPAI_TOKEN_BUDGET_WARNINGS")?
            .unwrap_or_else(|| DEFAULT_TOKEN_BUDGET_WARNINGS.to_vec());
        let max_retries = parse_env::<u32>("KALLIPAI_MAX_RETRIES")?.unwrap_or(DEFAULT_MAX_RETRIES);
        if max_retries > MAX_RETRIES_LIMIT {
            bail!("KALLIPAI_MAX_RETRIES must be at most {MAX_RETRIES_LIMIT}");
        }
        let retry_base_delay_secs = parse_env::<u64>("KALLIPAI_RETRY_BASE_DELAY_SECS")?
            .unwrap_or(DEFAULT_RETRY_BASE_DELAY_SECS);
        if retry_base_delay_secs == 0 {
            bail!("KALLIPAI_RETRY_BASE_DELAY_SECS must be greater than zero");
        }
        if retry_base_delay_secs > RETRY_DELAY_SECS_LIMIT {
            bail!("KALLIPAI_RETRY_BASE_DELAY_SECS must be at most {RETRY_DELAY_SECS_LIMIT}");
        }
        let retry_max_delay_secs = parse_env::<u64>("KALLIPAI_RETRY_MAX_DELAY_SECS")?
            .unwrap_or(DEFAULT_RETRY_MAX_DELAY_SECS);
        if retry_max_delay_secs == 0 {
            bail!("KALLIPAI_RETRY_MAX_DELAY_SECS must be greater than zero");
        }
        if retry_max_delay_secs > RETRY_DELAY_SECS_LIMIT {
            bail!("KALLIPAI_RETRY_MAX_DELAY_SECS must be at most {RETRY_DELAY_SECS_LIMIT}");
        }
        let retry_timeout_secs =
            parse_env::<u64>("KALLIPAI_RETRY_TIMEOUT_SECS")?.unwrap_or(DEFAULT_RETRY_TIMEOUT_SECS);
        if retry_timeout_secs == 0 {
            bail!("KALLIPAI_RETRY_TIMEOUT_SECS must be greater than zero");
        }
        if retry_timeout_secs > RETRY_TIMEOUT_SECS_LIMIT {
            bail!("KALLIPAI_RETRY_TIMEOUT_SECS must be at most {RETRY_TIMEOUT_SECS_LIMIT}");
        }

        // Single-value checks cannot catch a combination that is legal per knob
        // but self-contradictory. Both pairs are rejected at load time on purpose:
        // a deployment carrying one must stop booting rather than run behind a
        // warning nobody reads.
        if retry_base_delay_secs > retry_max_delay_secs {
            bail!(
                "KALLIPAI_RETRY_BASE_DELAY_SECS ({retry_base_delay_secs}s) is greater than KALLIPAI_RETRY_MAX_DELAY_SECS ({retry_max_delay_secs}s): the backoff cap would sit below the sequence's own starting point. Ranges: KALLIPAI_RETRY_BASE_DELAY_SECS > 0 and <= {RETRY_DELAY_SECS_LIMIT}; KALLIPAI_RETRY_MAX_DELAY_SECS > 0 and <= {RETRY_DELAY_SECS_LIMIT}. A working combination: KALLIPAI_RETRY_BASE_DELAY_SECS={DEFAULT_RETRY_BASE_DELAY_SECS} KALLIPAI_RETRY_MAX_DELAY_SECS={DEFAULT_RETRY_MAX_DELAY_SECS} KALLIPAI_RETRY_TIMEOUT_SECS={DEFAULT_RETRY_TIMEOUT_SECS}"
            );
        }
        if retry_max_delay_secs >= retry_timeout_secs {
            bail!(
                "KALLIPAI_RETRY_MAX_DELAY_SECS ({retry_max_delay_secs}s) is at or above KALLIPAI_RETRY_TIMEOUT_SECS ({retry_timeout_secs}s): one backoff would consume the whole retry deadline, which reads as a misconfiguration. Ranges: KALLIPAI_RETRY_MAX_DELAY_SECS > 0 and <= {RETRY_DELAY_SECS_LIMIT}; KALLIPAI_RETRY_TIMEOUT_SECS > 0 and <= {RETRY_TIMEOUT_SECS_LIMIT}. A working combination: KALLIPAI_RETRY_BASE_DELAY_SECS={DEFAULT_RETRY_BASE_DELAY_SECS} KALLIPAI_RETRY_MAX_DELAY_SECS={DEFAULT_RETRY_MAX_DELAY_SECS} KALLIPAI_RETRY_TIMEOUT_SECS={DEFAULT_RETRY_TIMEOUT_SECS}"
            );
        }
        // All four retry knobs are env-tunable: reshaping the retry policy is an operator
        // decision that should not need a rebuild.
        let retry_policy = RetryPolicy {
            max_retries,
            base_delay: std::time::Duration::from_secs(retry_base_delay_secs),
            max_delay: std::time::Duration::from_secs(retry_max_delay_secs),
            retry_timeout: std::time::Duration::from_secs(retry_timeout_secs),
        };

        match retry_budget_verdict(&retry_policy) {
            RetryBudgetVerdict::Fits => {}
            RetryBudgetVerdict::FactoryOverrun {
                worst_secs,
                timeout_secs,
            } => FACTORY_OVERRUN_REPORTED.call_once(|| {
                tracing::info!(
                    worst_case_secs = worst_secs,
                    retry_timeout_secs = timeout_secs,
                    "factory retry knobs: the worst-case wall clock of one episode exceeds the retry deadline, so a long episode is cut mid-backoff"
                )
            }),
            RetryBudgetVerdict::TunedOverrun {
                worst_secs,
                timeout_secs,
            } => TUNED_OVERRUN_REPORTED.call_once(|| {
                tracing::warn!(
                    worst_case_secs = worst_secs,
                    retry_timeout_secs = timeout_secs,
                    "retry knobs: the worst-case wall clock of one episode exceeds the retry deadline, so an episode is cut mid-backoff; raise KALLIPAI_RETRY_TIMEOUT_SECS or lower the delay knobs"
                )
            }),
        }

        let workspace_root = workspace_root.canonicalize().with_context(|| {
            format!(
                "failed to resolve workspace root {}",
                workspace_root.display()
            )
        })?;

        if summary_max_tokens == 0 {
            bail!("KALLIPAI_SUMMARY_MAX_TOKENS must be greater than zero");
        }
        if max_tool_rounds == 0 {
            bail!("KALLIPAI_MAX_TOOL_ROUNDS must be greater than zero");
        }
        if max_heartbeat_rounds == 0 {
            bail!("KALLIPAI_MAX_HEARTBEAT_ROUNDS must be greater than zero");
        }
        if max_transient_retries == 0 {
            bail!("KALLIPAI_MAX_TRANSIENT_RETRIES must be greater than zero");
        }
        if !(0.0..1.0).contains(&pinned_budget_ratio) {
            bail!("KALLIPAI_PINNED_BUDGET_RATIO must be between 0.0 and 1.0 (exclusive)");
        }
        if !protocol::context_keep_tokens_ratio_in_range(context_keep_tokens_ratio) {
            bail!("KALLIPAI_CONTEXT_KEEP_TOKENS_RATIO must be greater than 0.0 and at most 1.0");
        }
        check_context_budget(
            context_window_tokens,
            output_reserve_tokens,
            summary_max_tokens,
            pinned_budget_ratio,
            DEFAULT_TOOL_RESULT_FULL_TOKENS,
            DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS,
        )?;
        if context_thresholds.len() < 2 {
            bail!(
                "KALLIPAI_CONTEXT_THRESHOLDS must have at least 2 values (warnings + auto-compact)"
            );
        }
        if !context_thresholds.is_sorted() {
            bail!("KALLIPAI_CONTEXT_THRESHOLDS must be sorted ascending");
        }
        if context_thresholds.iter().any(|&t| !(1..=99).contains(&t)) {
            bail!("KALLIPAI_CONTEXT_THRESHOLDS values must be 1-99");
        }
        if token_budget_warnings.is_empty() {
            bail!("KALLIPAI_TOKEN_BUDGET_WARNINGS must have at least 1 value");
        }
        if !token_budget_warnings.is_sorted() {
            bail!("KALLIPAI_TOKEN_BUDGET_WARNINGS must be sorted ascending");
        }
        if token_budget_warnings
            .iter()
            .any(|&t| !(1..=99).contains(&t))
        {
            bail!("KALLIPAI_TOKEN_BUDGET_WARNINGS values must be 1-99");
        }

        Ok(Self {
            prompt,
            system_prompt,
            max_tool_rounds,
            max_heartbeat_rounds,
            max_transient_retries,
            workspace_root: workspace_root.clone(),
            context_window_tokens,
            output_reserve_tokens,
            summary_max_tokens,
            tool_timeout_secs,
            skills,
            retry_policy,
            pinned_budget_ratio,
            context_keep_turns,
            context_keep_tokens_ratio,
            context_thresholds,
            token_budget_warnings,
            agent_id: None,
            created_by: None,
            permissions: PermissionProfile::new(workspace_root),
            profile_set: None,
            permissions_class: PermissionClass::default(),
            // Set by the tagma at spawn (CreateAgentRequest) / restore (AgentMeta),
            // like `agent_id` / `created_by` above.
            role: String::new(),
            description: String::new(),
            delegation_mode: DelegationMode::CarveOut,
        })
    }

    /// Warning thresholds: all elements except the last.
    pub fn warning_thresholds(&self) -> &[u8] {
        // Last element is the auto-compact trigger, not a warning.
        &self.context_thresholds[..self.context_thresholds.len().saturating_sub(1)]
    }

    /// Auto-compact trigger: the last (highest) threshold.
    pub fn auto_compact_threshold(&self) -> u8 {
        *self.context_thresholds.last().unwrap_or(&80)
    }

    /// Effective token budget: context window minus output reserve.
    pub fn effective_budget(&self) -> usize {
        self.context_window_tokens
            .saturating_sub(self.output_reserve_tokens)
    }

    /// Pinned-context budget: the slice of [`effective_budget`](Self::effective_budget) reserved
    /// for pinned items, per `pinned_budget_ratio`. Single source of truth for the formula used
    /// at spawn (tagma) and on within-set failover (runtime). The private `check_context_budget`
    /// recomputes the same value from raw args because it runs before an `AgentConfig` exists.
    pub fn pinned_budget(&self) -> usize {
        (self.effective_budget() as f64 * self.pinned_budget_ratio) as usize
    }

    /// Keep-window size in tokens: the slice of
    /// [`effective_budget`](Self::effective_budget) a manual eviction may carry
    /// forward in the newest turns. Derived, so it follows the active window.
    ///
    /// Never rounds down to zero: a positive share must produce a positive cap,
    /// or a legal ratio would behave like the zero share the checks reject. The
    /// floor only bites on budgets a few tokens wide, where any real turn
    /// exceeds the cap anyway and the eviction reports the token cap.
    pub fn context_keep_tokens(&self) -> usize {
        ((self.effective_budget() as f64 * self.context_keep_tokens_ratio) as usize).max(1)
    }

    /// Tail-recovery budget: the token slice a manifest-loss rebuild may spend
    /// rehydrating conversation turns from the history tail (see
    /// `persistence::rebuild_window_from_tail`). A quarter of the effective
    /// budget — small enough to leave room for pins, fresh turns, and output
    /// within the window; large enough that a degraded agent still boots with
    /// recent context.
    pub fn tail_recovery_budget(&self) -> usize {
        self.effective_budget() / 4
    }

    /// Override `max_tool_rounds` with a per-request value.
    ///
    /// Takes precedence over both the default and the env var.
    /// Silently ignores zero (falls back to the loaded value).
    pub fn set_max_tool_rounds(&mut self, value: usize) {
        if value > 0 {
            self.max_tool_rounds = value;
        }
    }

    /// Override the keep window's turn cap with a per-spawn value.
    ///
    /// Takes precedence over the default and the env var; `0` is a legal
    /// request (the caller wants every turn condensed) and is applied as given.
    pub fn set_context_keep_turns(&mut self, turns: usize) {
        self.context_keep_turns = turns;
    }

    /// Override the keep window's token share with a per-spawn value.
    ///
    /// Validates **before** mutating, so a caller that treats `Err` as "reject
    /// the spawn" leaves the loaded value untouched. The bound is the same as
    /// the env knob's: a zero share would keep nothing and report a token-cap
    /// degradation, so it is out of range rather than a legal extreme.
    pub fn set_context_keep_tokens_ratio(&mut self, ratio: f64) -> Result<()> {
        if !protocol::context_keep_tokens_ratio_in_range(ratio) {
            bail!("context_keep_tokens_ratio must be greater than 0.0 and at most 1.0");
        }
        self.context_keep_tokens_ratio = ratio;
        Ok(())
    }

    /// The two caps a manual eviction keeps within: the configured turn count
    /// and the token slice of the effective budget.
    ///
    /// The one derivation point: the spawn installs this pair into the store,
    /// and every later window move (failover reapply, profile reset) installs it
    /// again, so a tool call always sees the caps the active configuration implies.
    pub fn eviction_caps(&self) -> KeepCaps {
        KeepCaps::new(self.context_keep_turns, self.context_keep_tokens())
    }

    /// Install `tokens` as the active context window after validating the window-dependent budget
    /// invariants. The single installer: every window — the config-file profile's and the
    /// sentinel set's alike — flows through here at spawn, and within-set failover re-applies
    /// the advanced profile's window via `acquisition::failover::reapply_window`.
    /// `context_window_tokens` is thus a derived snapshot of the active profile's
    /// declared window, not an independent config knob.
    /// Validates **before** mutating: on an invariant violation the field is left untouched and
    /// `Err` is returned, so a caller that treats the failure as "keep the prior window" gets
    /// exactly that. Failover pre-checks with `try_context_window` instead and *skips* an
    /// infeasible candidate before committing the advance (see `acquisition::failover::advance_failover`).
    pub fn set_context_window(&mut self, tokens: usize) -> Result<()> {
        self.try_context_window(tokens)?;
        self.context_window_tokens = tokens;
        Ok(())
    }

    /// Check whether `tokens` would satisfy the window-dependent budget invariants **without
    /// mutating**. The pre-advance probe used by within-set failover: `advance_to` is forward-only
    /// and cannot roll back, so an infeasible candidate must be rejected *before* committing. The
    /// same invariants as [`set_context_window`](Self::set_context_window) / [`load`](Self::load),
    /// via the shared private `check_context_budget`.
    pub(crate) fn try_context_window(&self, tokens: usize) -> Result<()> {
        check_context_budget(
            tokens,
            self.output_reserve_tokens,
            self.summary_max_tokens,
            self.pinned_budget_ratio,
            DEFAULT_TOOL_RESULT_FULL_TOKENS,
            DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS,
        )
    }
}

/// Validate the context-window-dependent budget invariants. Shared by [`AgentConfig::load`]
/// (env values) and [`AgentConfig::set_context_window`] (profile override) so the two paths
/// cannot drift. `pinned_budget` is recomputed locally here because `ContextStore`'s
/// `set_pinned_budget` runs later and independently (at spawn via the tagma, and on within-set
/// failover via `acquisition::failover::reapply_window`).
fn check_context_budget(
    context_window_tokens: usize,
    output_reserve_tokens: usize,
    summary_max_tokens: u32,
    pinned_budget_ratio: f64,
    tool_result_full_tokens: usize,
    tool_result_truncated_tokens: usize,
) -> Result<()> {
    if context_window_tokens == 0 {
        bail!("context_window_tokens must be greater than zero");
    }
    if output_reserve_tokens >= context_window_tokens {
        bail!(
            "output_reserve_tokens ({output_reserve_tokens}) must be less than \
             context_window_tokens ({context_window_tokens})"
        );
    }
    let effective_budget = context_window_tokens.saturating_sub(output_reserve_tokens);
    let pinned_budget = (effective_budget as f64 * pinned_budget_ratio) as usize;
    if summary_max_tokens as usize > pinned_budget {
        bail!(
            "summary_max_tokens ({summary_max_tokens}) exceeds pinned budget ({pinned_budget} = \
             effective_budget {effective_budget} × ratio {pinned_budget_ratio}). \
             Increase the context window or pinned_budget_ratio, or reduce summary_max_tokens."
        );
    }
    let summarizer_input_budget = effective_budget.saturating_sub(summary_max_tokens as usize);
    if tool_result_truncated_tokens * 2 >= summarizer_input_budget {
        bail!(concat!(
            "tool result truncated cap ({tool_result_truncated_tokens}) must stay well below ",
            "the summarizer input budget ({summarizer_input_budget} = effective_budget ",
            "{effective_budget} − summary_max_tokens {summary_max_tokens}): a cap this ",
            "large re-creates the oversized-turn compaction wedge the cap exists to prevent"
        ));
    }
    if tool_result_full_tokens <= tool_result_truncated_tokens {
        bail!(concat!(
            "tool result full cap ({tool_result_full_tokens}) must be greater than the ",
            "truncated cap ({tool_result_truncated_tokens})"
        ));
    }
    Ok(())
}

/// Worst-case wall clock of one episode: the sum of the retry core's own backoff
/// schedule over the waits an episode can make, plus one call per send at
/// `CALL_WORST_ASSUMED`. Both the schedule and the send count come from the core,
/// so this cannot drift from what the shells perform; the per-call term is the
/// assumed one.
fn retry_worst_case(policy: &RetryPolicy) -> Duration {
    let waits = (0..policy.max_retries).fold(Duration::ZERO, |waits, exponent| {
        waits.saturating_add(backoff_schedule(policy, exponent, RETRY_JITTER_MAX))
    });
    waits.saturating_add(CALL_WORST_ASSUMED.saturating_mul(max_sends(policy)))
}

/// The two retry-budget reports are process-wide facts: the knobs come from the
/// process environment, so every agent loaded in this process would repeat the
/// same line. Each tier reports once.
static FACTORY_OVERRUN_REPORTED: Once = Once::new();
static TUNED_OVERRUN_REPORTED: Once = Once::new();

/// What the startup check has to say about the retry knobs' worst-case wall
/// clock.
#[derive(Debug)]
enum RetryBudgetVerdict {
    /// The worst case fits inside `retry_timeout`: nothing to report.
    Fits,
    /// The worst case overruns the deadline and every knob still holds its
    /// factory value. The factory set is itself a cut configuration, so this is
    /// one explanatory line rather than a warning.
    FactoryOverrun { worst_secs: u64, timeout_secs: u64 },
    /// The worst case overruns the deadline and at least one knob differs from
    /// the factory set: warn, and name both reals.
    TunedOverrun { worst_secs: u64, timeout_secs: u64 },
}

/// Verdict for the retry knobs' worst-case wall clock, judged on the values
/// alone: the check cannot know who set them.
fn retry_budget_verdict(policy: &RetryPolicy) -> RetryBudgetVerdict {
    let worst_secs = retry_worst_case(policy).as_secs();
    let timeout_secs = policy.retry_timeout.as_secs();
    if worst_secs <= timeout_secs {
        RetryBudgetVerdict::Fits
    } else if retry_knobs_are_factory(policy) {
        RetryBudgetVerdict::FactoryOverrun {
            worst_secs,
            timeout_secs,
        }
    } else {
        RetryBudgetVerdict::TunedOverrun {
            worst_secs,
            timeout_secs,
        }
    }
}

/// True when all four retry knobs still hold their factory values.
fn retry_knobs_are_factory(policy: &RetryPolicy) -> bool {
    policy.max_retries == DEFAULT_MAX_RETRIES
        && policy.base_delay.as_secs() == DEFAULT_RETRY_BASE_DELAY_SECS
        && policy.max_delay.as_secs() == DEFAULT_RETRY_MAX_DELAY_SECS
        && policy.retry_timeout.as_secs() == DEFAULT_RETRY_TIMEOUT_SECS
}

#[cfg(test)]
mod tests;
