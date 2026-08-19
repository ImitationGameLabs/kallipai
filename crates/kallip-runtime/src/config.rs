use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::env_util::{DEFAULT_CONTEXT_WINDOW_TOKENS, parse_env, parse_env_list};
use crate::retry::RetryPolicy;
use kallip_common::AgentId;

mod defaults;
use defaults::{
    DEFAULT_CONTEXT_THRESHOLDS, DEFAULT_MAX_HEARTBEAT_ROUNDS, DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TOOL_ROUNDS, DEFAULT_MAX_TRANSIENT_RETRIES, DEFAULT_OUTPUT_RESERVE_TOKENS,
    DEFAULT_PINNED_BUDGET_RATIO, DEFAULT_RETRY_BASE_DELAY_SECS, DEFAULT_RETRY_MAX_DELAY_SECS,
    DEFAULT_RETRY_TIMEOUT_SECS, DEFAULT_SUMMARY_MAX_TOKENS, DEFAULT_SYSTEM_PROMPT,
    DEFAULT_TOKEN_BUDGET_WARNINGS, DEFAULT_TOOL_TIMEOUT_SECS, MAX_RETRIES_LIMIT,
    RETRY_DELAY_SECS_LIMIT, RETRY_TIMEOUT_SECS_LIMIT,
};
mod exec_hooks;
pub use exec_hooks::load_exec_hook_rules;
mod env;
pub use env::{permission_class_from_env, policy_preset_from_env};
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
    /// round loop fresh). Default 3 (`KALLIP_MAX_HEARTBEAT_ROUNDS`).
    pub max_heartbeat_rounds: u32,
    /// Max consecutive transient (failover-chain-exhausted) parks that earn a
    /// timed retry before the agent hard-parks and surfaces. Default 3
    /// (`KALLIP_MAX_TRANSIENT_RETRIES`).
    pub max_transient_retries: u32,
    pub workspace_root: PathBuf,
    pub context_window_tokens: usize,
    pub output_reserve_tokens: usize,
    pub summary_max_tokens: u32,
    pub tool_timeout_secs: u64,
    pub skills: Vec<String>,
    pub retry_policy: RetryPolicy,
    pub pinned_budget_ratio: f64,
    pub context_thresholds: Vec<u8>,
    /// Thresholds (as percentages 1-99) at which to warn the LLM about
    /// approaching token budget exhaustion.
    pub token_budget_warnings: Vec<u8>,
    pub agent_id: Option<AgentId>,
    pub created_by: Option<AgentId>,
    pub permissions: PermissionProfile,
    /// FS-access permission class (Guest readonly / Normal home-rw) — the static
    /// baseline axis of the sandbox (§2.3). Defaults to Normal; the tagma clamps
    /// it to the model tier's ceiling at spawn and re-validates on restore. Unlike
    /// `role`/`description`, this is a safety invariant, not display metadata.
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
    /// `workspace_root`, `permissions_class`) default to empty/None and are
    /// overwritten by the caller.
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
            context_thresholds: DEFAULT_CONTEXT_THRESHOLDS.to_vec(),
            token_budget_warnings: DEFAULT_TOKEN_BUDGET_WARNINGS.to_vec(),
            agent_id: None,
            created_by: None,
            permissions: PermissionProfile::new(PathBuf::new()),
            permissions_class: PermissionClass::Normal,
            role: String::new(),
            description: String::new(),
            delegation_mode: DelegationMode::CarveOut,
        }
    }
}

impl AgentConfig {
    /// Whether this is the tagma-owned root agent — the unique agent minted
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
        let system_prompt =
            std::env::var("KALLIP_SYSTEM_PROMPT").unwrap_or_else(|_| DEFAULT_SYSTEM_PROMPT.into());
        let max_tool_rounds =
            parse_env::<usize>("KALLIP_MAX_TOOL_ROUNDS")?.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS);
        let max_heartbeat_rounds = parse_env::<u32>("KALLIP_MAX_HEARTBEAT_ROUNDS")?
            .unwrap_or(DEFAULT_MAX_HEARTBEAT_ROUNDS);
        let max_transient_retries = parse_env::<u32>("KALLIP_MAX_TRANSIENT_RETRIES")?
            .unwrap_or(DEFAULT_MAX_TRANSIENT_RETRIES);
        let workspace_root = workspace_root
            .or_else(|| {
                std::env::var("KALLIP_WORKSPACE_ROOT")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?);
        let context_window_tokens = parse_env::<usize>("KALLIP_CONTEXT_WINDOW_TOKENS")?
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        let output_reserve_tokens = parse_env::<usize>("KALLIP_OUTPUT_RESERVE_TOKENS")?
            .unwrap_or(DEFAULT_OUTPUT_RESERVE_TOKENS);
        let summary_max_tokens =
            parse_env::<u32>("KALLIP_SUMMARY_MAX_TOKENS")?.unwrap_or(DEFAULT_SUMMARY_MAX_TOKENS);
        let tool_timeout_secs =
            parse_env::<u64>("KALLIP_TOOL_TIMEOUT_SECS")?.unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS);

        let pinned_budget_ratio =
            parse_env::<f64>("KALLIP_PINNED_BUDGET_RATIO")?.unwrap_or(DEFAULT_PINNED_BUDGET_RATIO);
        let context_thresholds = parse_env_list::<u8>("KALLIP_CONTEXT_THRESHOLDS")?
            .unwrap_or_else(|| DEFAULT_CONTEXT_THRESHOLDS.to_vec());
        let token_budget_warnings = parse_env_list::<u8>("KALLIP_TOKEN_BUDGET_WARNINGS")?
            .unwrap_or_else(|| DEFAULT_TOKEN_BUDGET_WARNINGS.to_vec());
        let max_retries = parse_env::<u32>("KALLIP_MAX_RETRIES")?.unwrap_or(DEFAULT_MAX_RETRIES);
        if max_retries > MAX_RETRIES_LIMIT {
            bail!("KALLIP_MAX_RETRIES must be at most {MAX_RETRIES_LIMIT}");
        }
        let retry_base_delay_secs = parse_env::<u64>("KALLIP_RETRY_BASE_DELAY_SECS")?
            .unwrap_or(DEFAULT_RETRY_BASE_DELAY_SECS);
        if retry_base_delay_secs == 0 {
            bail!("KALLIP_RETRY_BASE_DELAY_SECS must be greater than zero");
        }
        if retry_base_delay_secs > RETRY_DELAY_SECS_LIMIT {
            bail!("KALLIP_RETRY_BASE_DELAY_SECS must be at most {RETRY_DELAY_SECS_LIMIT}");
        }
        let retry_max_delay_secs = parse_env::<u64>("KALLIP_RETRY_MAX_DELAY_SECS")?
            .unwrap_or(DEFAULT_RETRY_MAX_DELAY_SECS);
        if retry_max_delay_secs == 0 {
            bail!("KALLIP_RETRY_MAX_DELAY_SECS must be greater than zero");
        }
        if retry_max_delay_secs > RETRY_DELAY_SECS_LIMIT {
            bail!("KALLIP_RETRY_MAX_DELAY_SECS must be at most {RETRY_DELAY_SECS_LIMIT}");
        }
        let retry_timeout_secs =
            parse_env::<u64>("KALLIP_RETRY_TIMEOUT_SECS")?.unwrap_or(DEFAULT_RETRY_TIMEOUT_SECS);
        if retry_timeout_secs == 0 {
            bail!("KALLIP_RETRY_TIMEOUT_SECS must be greater than zero");
        }
        if retry_timeout_secs > RETRY_TIMEOUT_SECS_LIMIT {
            bail!("KALLIP_RETRY_TIMEOUT_SECS must be at most {RETRY_TIMEOUT_SECS_LIMIT}");
        }
        // All four retry knobs are env-tunable: sustained rate-limit parking (429 retries
        // while queued) proved operators need to reshape the retry window without a rebuild.
        let retry_policy = RetryPolicy {
            max_retries,
            base_delay: std::time::Duration::from_secs(retry_base_delay_secs),
            max_delay: std::time::Duration::from_secs(retry_max_delay_secs),
            retry_timeout: std::time::Duration::from_secs(retry_timeout_secs),
        };

        let workspace_root = workspace_root.canonicalize().with_context(|| {
            format!(
                "failed to resolve workspace root {}",
                workspace_root.display()
            )
        })?;

        if summary_max_tokens == 0 {
            bail!("KALLIP_SUMMARY_MAX_TOKENS must be greater than zero");
        }
        if max_tool_rounds == 0 {
            bail!("KALLIP_MAX_TOOL_ROUNDS must be greater than zero");
        }
        if max_heartbeat_rounds == 0 {
            bail!("KALLIP_MAX_HEARTBEAT_ROUNDS must be greater than zero");
        }
        if max_transient_retries == 0 {
            bail!("KALLIP_MAX_TRANSIENT_RETRIES must be greater than zero");
        }
        if !(0.0..1.0).contains(&pinned_budget_ratio) {
            bail!("KALLIP_PINNED_BUDGET_RATIO must be between 0.0 and 1.0 (exclusive)");
        }
        check_context_budget(
            context_window_tokens,
            output_reserve_tokens,
            summary_max_tokens,
            pinned_budget_ratio,
        )?;
        if context_thresholds.len() < 2 {
            bail!(
                "KALLIP_CONTEXT_THRESHOLDS must have at least 2 values (warnings + auto-compact)"
            );
        }
        if !context_thresholds.is_sorted() {
            bail!("KALLIP_CONTEXT_THRESHOLDS must be sorted ascending");
        }
        if context_thresholds.iter().any(|&t| !(1..=99).contains(&t)) {
            bail!("KALLIP_CONTEXT_THRESHOLDS values must be 1-99");
        }
        if token_budget_warnings.is_empty() {
            bail!("KALLIP_TOKEN_BUDGET_WARNINGS must have at least 1 value");
        }
        if !token_budget_warnings.is_sorted() {
            bail!("KALLIP_TOKEN_BUDGET_WARNINGS must be sorted ascending");
        }
        if token_budget_warnings
            .iter()
            .any(|&t| !(1..=99).contains(&t))
        {
            bail!("KALLIP_TOKEN_BUDGET_WARNINGS values must be 1-99");
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
            context_thresholds,
            token_budget_warnings,
            agent_id: None,
            created_by: None,
            permissions: PermissionProfile::new(workspace_root),
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
    /// at spawn (tagma) and on within-tier failover (runtime). The private `check_context_budget`
    /// recomputes the same value from raw args because it runs before an `AgentConfig` exists.
    pub fn pinned_budget(&self) -> usize {
        (self.effective_budget() as f64 * self.pinned_budget_ratio) as usize
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

    /// Install `tokens` as the active context window after validating the window-dependent budget
    /// invariants. The single installer: every window — including the implicit env profile's
    /// (`profile::from_env` reads `KALLIP_CONTEXT_WINDOW_TOKENS` into `max_context_window`) —
    /// flows through here at spawn, and within-tier failover re-applies the advanced profile's
    /// window via `acquisition::reapply_window`. `context_window_tokens` is thus a derived snapshot of
    /// the active profile's declared window, not an independent config knob.
    ///
    /// Validates **before** mutating: on an invariant violation the field is left untouched and
    /// `Err` is returned, so a caller that treats the failure as "keep the prior window" gets
    /// exactly that. Failover pre-checks with `try_context_window` instead and *skips* an
    /// infeasible candidate before committing the advance (see `acquisition::advance_failover`).
    pub fn set_context_window(&mut self, tokens: usize) -> Result<()> {
        self.try_context_window(tokens)?;
        self.context_window_tokens = tokens;
        Ok(())
    }

    /// Check whether `tokens` would satisfy the window-dependent budget invariants **without
    /// mutating**. The pre-advance probe used by within-tier failover: `advance_to` is forward-only
    /// and cannot roll back, so an infeasible candidate must be rejected *before* committing. The
    /// same invariants as [`set_context_window`](Self::set_context_window) / [`load`](Self::load),
    /// via the shared private `check_context_budget`.
    pub(crate) fn try_context_window(&self, tokens: usize) -> Result<()> {
        check_context_budget(
            tokens,
            self.output_reserve_tokens,
            self.summary_max_tokens,
            self.pinned_budget_ratio,
        )
    }
}

/// Validate the context-window-dependent budget invariants. Shared by [`AgentConfig::load`]
/// (env values) and [`AgentConfig::set_context_window`] (profile override) so the two paths
/// cannot drift. `pinned_budget` is recomputed locally here because `ContextStore`'s
/// `set_pinned_budget` runs later and independently (at spawn via the tagma, and on within-tier
/// failover via `acquisition::reapply_window`).
fn check_context_budget(
    context_window_tokens: usize,
    output_reserve_tokens: usize,
    summary_max_tokens: u32,
    pinned_budget_ratio: f64,
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
    Ok(())
}

#[cfg(test)]
mod tests;
