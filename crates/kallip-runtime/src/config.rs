use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::env_util::{DEFAULT_CONTEXT_WINDOW_TOKENS, parse_env, parse_env_list};
use crate::retry::RetryPolicy;
use crate::tools::context::{
    ContextEvictTool, ContextPinTool, ContextStatusTool, ContextUnpinTool, ExecPolicyTool,
};
use crate::tools::skill::FilePinTool;
use kallip_common::AgentId;
use kallip_common::policy::{PolicyDecision, ToolPolicy};
use kallip_shell::tools::names;

const DEFAULT_SYSTEM_PROMPT: &str = concat!(
    "You are a minimal coding agent. ",
    "Keep answers concise and prefer the least risky tool that accomplishes the task; ",
    "each tool's own description explains its usage. ",
    "Some tool actions are asynchronous — a backgrounded bash task or a deferred ",
    "(pending-approval) action completes later and surfaces a notice in context; ",
    "read the notice and follow its instruction.",
);
/// Effectively unlimited — the real safety net is the daemon-wide token budget.
/// Individual rounds are bounded by LLM response length; the loop as a whole is
/// bounded by token consumption. This constant only serves as a last-resort
/// guard against a degenerate "tool calls with no progress" loop.
const DEFAULT_MAX_TOOL_ROUNDS: usize = usize::MAX;
const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 1_200;
const DEFAULT_OUTPUT_RESERVE_TOKENS: usize = 8_192;
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_RETRIES: u32 = 3;
const DEFAULT_RETRY_BASE_DELAY_SECS: u64 = 1;
const DEFAULT_PINNED_BUDGET_RATIO: f64 = 0.25;
const DEFAULT_CONTEXT_THRESHOLDS: &[u8] = &[50, 60, 70, 80];
const DEFAULT_TOKEN_BUDGET_WARNINGS: &[u8] = &[80, 95];

/// Named policy preset selectable via `KALLIP_POLICY_PRESET`.
///
/// Each variant maps to a concrete [`ToolPolicy`]. When the env var is unset,
/// the existing `KALLIP_ALLOW_TOOLS` / hardcoded default logic applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PolicyPreset {
    /// All tools allowed — `default: Allow`, no per-tool overrides.
    AllowAll,
    /// All tools require approval — `default: Ask`, no per-tool overrides.
    AskAll,
}

impl std::fmt::Display for PolicyPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AllowAll => "allow-all",
            Self::AskAll => "ask-all",
        })
    }
}

impl std::str::FromStr for PolicyPreset {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow-all" => Ok(Self::AllowAll),
            "ask-all" => Ok(Self::AskAll),
            _ => Err(format!(
                "invalid policy preset '{s}' (expected allow-all or ask-all)"
            )),
        }
    }
}

/// Default tool policy matching the current hardcoded behavior.
///
/// Tool names are referenced via each tool's `NAME` constant (context/skill
/// tools, local) and the `kallip_shell::tools::names` module (shell tools)
/// rather than duplicated string literals, keeping this map in lockstep with
/// the tool registry.
pub fn default_tool_policy() -> ToolPolicy {
    use std::collections::BTreeMap;
    let mut tools = BTreeMap::new();
    tools.insert(names::BG_READ.into(), PolicyDecision::Allow);
    // The agent only kills background tasks it spawned itself (task_id-scoped),
    // so there is no cross-agent or user-workspace risk worth gating on.
    tools.insert(names::BG_KILL.into(), PolicyDecision::Allow);
    tools.insert(names::BASH_EXEC.into(), PolicyDecision::Classify);
    tools.insert(ContextPinTool::NAME.into(), PolicyDecision::Allow);
    tools.insert(ContextUnpinTool::NAME.into(), PolicyDecision::Allow);
    tools.insert(ContextStatusTool::NAME.into(), PolicyDecision::Allow);
    tools.insert(ContextEvictTool::NAME.into(), PolicyDecision::Allow);
    tools.insert(FilePinTool::NAME.into(), PolicyDecision::Allow);
    tools.insert(ExecPolicyTool::NAME.into(), PolicyDecision::Allow);
    ToolPolicy {
        default: PolicyDecision::Ask,
        tools,
    }
}

/// Resolve a [`PolicyPreset`] to a concrete [`ToolPolicy`].
fn tool_policy_from_preset(preset: PolicyPreset) -> ToolPolicy {
    match preset {
        PolicyPreset::AllowAll => ToolPolicy::new(PolicyDecision::Allow),
        PolicyPreset::AskAll => ToolPolicy::new(PolicyDecision::Ask),
    }
}

/// Returns the tool policy using `KALLIP_ALLOW_TOOLS` (legacy) or the
/// hardcoded [`default_tool_policy`].
///
/// See [`tool_policy_from_env`] for the full resolution chain that includes
/// `KALLIP_POLICY_PRESET` support.
fn tool_policy_from_env_inner() -> ToolPolicy {
    let Ok(raw) = std::env::var("KALLIP_ALLOW_TOOLS") else {
        return default_tool_policy();
    };
    if raw.trim().is_empty() {
        return default_tool_policy();
    }
    let known = default_tool_policy();
    let mut tools = std::collections::BTreeMap::new();
    for name in raw.split(',') {
        let name = name.trim();
        if !name.is_empty() {
            if !known.tools.contains_key(name) {
                tracing::warn!(
                    "KALLIP_ALLOW_TOOLS: unknown tool name '{name}' \
                     — not in default tool policy, may be a typo"
                );
            }
            tools.insert(name.to_owned(), PolicyDecision::Allow);
        }
    }
    ToolPolicy {
        default: PolicyDecision::Ask,
        tools,
    }
}

/// Returns the tool policy for a root agent, checking env vars in priority
/// order:
///
/// 1. `KALLIP_POLICY_PRESET` — named preset (`allow-all` or `ask-all`).
/// 2. `KALLIP_ALLOW_TOOLS` — legacy comma-separated allow list.
/// 3. Hardcoded [`default_tool_policy`] — fallback.
///
/// `Classify` is not expressible via presets — it is a per-tool behavior
/// unique to the default policy. When a preset is active, `bash_exec`
/// resolves to the preset's default decision instead.
///
/// Only affects root agents at creation time. Subagents inherit their
/// supervisor's policy.
pub fn tool_policy_from_env() -> ToolPolicy {
    // Priority 1: named preset.
    let Ok(raw) = std::env::var("KALLIP_POLICY_PRESET") else {
        return tool_policy_from_env_inner();
    };

    let raw = raw.trim();
    if raw.is_empty() {
        return tool_policy_from_env_inner();
    }

    let preset = raw.parse::<PolicyPreset>().unwrap_or_else(|e| {
        panic!("KALLIP_POLICY_PRESET: {e}");
    });

    if std::env::var("KALLIP_ALLOW_TOOLS").is_ok_and(|v| !v.trim().is_empty()) {
        tracing::info!(
            "KALLIP_POLICY_PRESET={preset} takes precedence \
             over KALLIP_ALLOW_TOOLS"
        );
    }

    tool_policy_from_preset(preset)
}

/// Resolve the root agent's permission class from `KALLIP_ROOT_AGENT_PERMISSION_CLASS`.
///
/// Root-only test knob, parallel to [`tool_policy_from_env`]: read in the
/// daemon's root-create branch, never on the subagent or restore paths
/// (subagents derive their class from `ceiling_for_tier`; restore uses the
/// persisted `meta.json`). Accepts lowercase `"normal"` / `"guest"` — the env-var
/// convention, distinct from the PascalCase serde form persisted in `meta.json`.
/// Panics on an invalid value, matching [`tool_policy_from_env`]'s misconfig behavior.
pub fn permission_class_from_env() -> PermissionClass {
    let Ok(raw) = std::env::var("KALLIP_ROOT_AGENT_PERMISSION_CLASS") else {
        return PermissionClass::default();
    };
    // Trim here, not inside FromStr: the wire/env convention trims surrounding
    // whitespace, but FromStr stays trim-free so the daemon rejects untrimmed
    // client input verbatim.
    let raw = raw.trim();
    match raw.parse::<PermissionClass>() {
        Ok(class) => class,
        Err(_) => panic!(
            "KALLIP_ROOT_AGENT_PERMISSION_CLASS: invalid permission class '{raw}' (expected normal or guest)"
        ),
    }
}

/// Hard-coded maximum delegation depth for top-level agents.
///
/// Not configurable — hard-coding avoids the complexity of persisting and
/// re-validating a dynamic value across restarts. The depth is recomputed
/// from the `created_by` chain on restore (depth = Self - chain length),
/// eliminating any attack surface from tampered `meta.json`. A future
/// increase to this constant will cover all reasonable delegation needs
/// once the chain-walking restore path is sufficiently tested.
pub const DEFAULT_MAX_DEPTH: u8 = 3;

/// FS-access permission class — the static baseline axis of the agent sandbox
/// (`.draft/design/agent-sandbox.md` §2.3).
///
/// Independent of model tier: tier only sets the *ceiling* via
/// [`PermissionClass::ceiling_for_tier`]. `Ord` is derived (`Guest < Normal`) so the
/// ceiling invariants `granted <= ceiling(tier)` and `ceiling(child) <=
/// ceiling(parent)` are plain comparisons. Persisted on `AgentMeta` and
/// re-validated on restore (a safety invariant, unlike display fields).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum PermissionClass {
    /// Guest: readonly — workspace RO, secret zero-access, no home write.
    Guest,
    /// Normal: home broad-write + workspace write. Default for root agents.
    #[default]
    Normal,
}

impl PermissionClass {
    /// Ceiling table: depth 0/1 -> Normal, depth 2/3 -> Guest (§2.3). Depths
    /// beyond the table clamp to the last entry (Guest), mirroring
    /// `ProfileRegistry::select_profile`.
    /// NOTE: depth monotonicity does NOT imply ceiling monotonicity (the 0/1 and
    /// 2/3 plateaus), so `ceiling(child) <= ceiling(parent)` must be enforced
    /// explicitly at spawn/restore — not derived from depth.
    pub fn ceiling_for_tier(depth: usize) -> Self {
        const CEILINGS: [PermissionClass; (DEFAULT_MAX_DEPTH as usize) + 1] = [
            PermissionClass::Normal, // depth 0 (root)
            PermissionClass::Normal, // depth 1
            PermissionClass::Guest,  // depth 2
            PermissionClass::Guest,  // depth 3
        ];
        CEILINGS[depth.min(CEILINGS.len() - 1)]
    }
}

/// Error returned when a [`PermissionClass`] cannot be parsed from its lowercase
/// wire/env spelling. Surfaced by the daemon as a `400 Bad Request` body, so the
/// message stays client-readable and stable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePermissionClassError(pub String);

impl std::fmt::Display for ParsePermissionClassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid permission class '{}' (expected \"normal\" or \"guest\")",
            self.0
        )
    }
}

impl std::error::Error for ParsePermissionClassError {}

/// Lowercase wire/env spelling: `"normal"` / `"guest"`. This is the inverse of
/// [`PermissionClass`]'s [`std::fmt::Display`] and matches the
/// `KALLIP_ROOT_AGENT_PERMISSION_CLASS` env-var convention — distinct from the
/// PascalCase serde form persisted in `meta.json`. Parsing is intentionally
/// trim-free; callers decide whether to trim surrounding whitespace.
impl std::str::FromStr for PermissionClass {
    type Err = ParsePermissionClassError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "normal" => Ok(PermissionClass::Normal),
            "guest" => Ok(PermissionClass::Guest),
            other => Err(ParsePermissionClassError(other.to_owned())),
        }
    }
}

/// Lowercase wire/env spelling (`"normal"` / `"guest"`), the inverse of
/// [`std::str::FromStr`]. Used by the permissions endpoint and by client-facing
/// error messages so they stay consistent with the wire form (rather than the
/// PascalCase `Debug`/serde form).
impl std::fmt::Display for PermissionClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PermissionClass::Guest => f.write_str("guest"),
            PermissionClass::Normal => f.write_str("normal"),
        }
    }
}

/// Runtime configuration for `kallip`.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub prompt: Option<String>,
    pub system_prompt: String,
    pub max_tool_rounds: usize,
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
    /// baseline axis of the sandbox (§2.3). Defaults to Normal; the daemon clamps
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
        }
    }
}

impl AgentConfig {
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
        let retry_base_delay_secs = parse_env::<u64>("KALLIP_RETRY_BASE_DELAY_SECS")?
            .unwrap_or(DEFAULT_RETRY_BASE_DELAY_SECS);
        if retry_base_delay_secs == 0 {
            bail!("KALLIP_RETRY_BASE_DELAY_SECS must be greater than zero");
        }
        // max_delay and retry_timeout use defaults (30s / 120s) — intentionally
        // not exposed as env vars since they rarely need tuning.
        let retry_policy = RetryPolicy {
            max_retries,
            base_delay: std::time::Duration::from_secs(retry_base_delay_secs),
            ..RetryPolicy::default()
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
            // Set by the daemon at spawn (CreateAgentRequest) / restore (AgentMeta),
            // like `agent_id` / `created_by` above.
            role: String::new(),
            description: String::new(),
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
    /// at spawn (daemon) and on within-tier failover (runtime). The private `check_context_budget`
    /// recomputes the same value from raw args because it runs before an `AgentConfig` exists.
    pub fn pinned_budget(&self) -> usize {
        (self.effective_budget() as f64 * self.pinned_budget_ratio) as usize
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
    /// window via `runner::reapply_window`. `context_window_tokens` is thus a derived snapshot of
    /// the active profile's declared window, not an independent config knob.
    ///
    /// Validates **before** mutating: on an invariant violation the field is left untouched and
    /// `Err` is returned, so a caller that treats the failure as "keep the prior window" gets
    /// exactly that. Failover pre-checks with `try_context_window` instead and *skips* an
    /// infeasible candidate before committing the advance (see `runner::advance_failover`).
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
/// `set_pinned_budget` runs later and independently (at spawn via the daemon, and on within-tier
/// failover via `runner::reapply_window`).
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

/// Permission profile controlling agent delegation capabilities.
#[derive(Clone, Debug)]
pub struct PermissionProfile {
    /// Remaining delegation levels. Decremented for each subagent.
    pub max_depth: u8,
    /// Workspace boundary. Subagents must operate within their supervisor's workspace.
    pub workspace_root: PathBuf,
}

impl PermissionProfile {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            workspace_root,
        }
    }

    /// Create a profile for a subagent with decremented depth.
    pub fn subagent(workspace_root: PathBuf, supervisor_depth: u8) -> Self {
        Self {
            max_depth: supervisor_depth.saturating_sub(1),
            workspace_root,
        }
    }

    /// Delegation depth as a tier-selection index: root (`max_depth == DEFAULT_MAX_DEPTH`) → 0,
    /// each delegation level decrements. Single source of truth for the depth formula used by
    /// tier selection. This consumes `max_depth` (set at spawn or recomputed from the chain on
    /// restore); it does not participate in setting it.
    pub fn depth(&self) -> usize {
        DEFAULT_MAX_DEPTH.saturating_sub(self.max_depth) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_common::policy::PolicyDecision;

    #[test]
    fn permission_class_ceiling_matches_tier_table() {
        // §2.3: tier 0/1 -> Normal, tier 2/3 -> Guest (the plateaus that mean depth
        // monotonicity does NOT imply ceiling monotonicity).
        assert_eq!(
            PermissionClass::ceiling_for_tier(0),
            PermissionClass::Normal
        );
        assert_eq!(
            PermissionClass::ceiling_for_tier(1),
            PermissionClass::Normal
        );
        assert_eq!(PermissionClass::ceiling_for_tier(2), PermissionClass::Guest);
        assert_eq!(PermissionClass::ceiling_for_tier(3), PermissionClass::Guest);
        // Beyond the table clamps to the last entry (Guest), like select_profile.
        assert_eq!(
            PermissionClass::ceiling_for_tier(99),
            PermissionClass::Guest
        );
    }

    #[test]
    fn permission_class_from_str_display_round_trip() {
        use std::str::FromStr;
        // Lowercase wire/env spelling, both variants round-trip through Display.
        for class in [PermissionClass::Normal, PermissionClass::Guest] {
            let s = class.to_string();
            assert_eq!(PermissionClass::from_str(&s).unwrap(), class);
        }
        assert_eq!(PermissionClass::Normal.to_string(), "normal");
        assert_eq!(PermissionClass::Guest.to_string(), "guest");

        // FromStr is trim-free: untrimmed input is rejected (the daemon must not
        // silently accept " guest "). The env knob trims before parsing.
        assert!(PermissionClass::from_str(" guest ").is_err());
        assert!(PermissionClass::from_str("Normal").is_err());
        assert!(PermissionClass::from_str("").is_err());
        let err = PermissionClass::from_str("admin").unwrap_err();
        assert!(err.to_string().contains("admin"));
        assert!(err.to_string().contains("normal"));
    }

    #[test]
    fn default_system_prompt_stays_high_altitude() {
        // The base prompt must stay at agent altitude: identity, posture, and
        // the cross-cutting async-notice model. Tool mechanics belong in each
        // tool's `description()` and the skill system belongs in the bootstrap
        // meta-skill the daemon appends at runtime (routes/agent.rs). This guard
        // prevents tool/CLI usage from creeping back into the prompt and
        // re-duplicating those sources (drift + per-request token cost).
        let prompt = DEFAULT_SYSTEM_PROMPT;
        assert!(prompt.contains("minimal coding agent"));
        assert!(prompt.contains("asynchronous"));
        for verboten in [
            names::BASH_EXEC,
            names::BG_READ,
            names::BG_KILL,
            ContextUnpinTool::NAME,
            FilePinTool::NAME,
            // The approval family and the skill-promote CLI were migrated out
            // together; guard the whole family, not just one member.
            "approval_redeem",
            "approval_commit",
            "approval_list",
            "approval_cancel",
            "skill promote",
        ] {
            assert!(
                !prompt.contains(verboten),
                "DEFAULT_SYSTEM_PROMPT must not embed tool/CLI usage ('{verboten}'); \
                 it belongs in the tool description or the bootstrap skill"
            );
        }
    }

    #[test]
    fn check_context_budget_rejects_zero_window() {
        assert!(check_context_budget(0, 100, 50, 0.25).is_err());
    }

    #[test]
    fn check_context_budget_rejects_reserve_ge_window() {
        assert!(check_context_budget(1000, 1000, 100, 0.25).is_err()); // equal
        assert!(check_context_budget(1000, 1001, 100, 0.25).is_err()); // greater
    }

    #[test]
    fn check_context_budget_rejects_summary_exceeding_pinned() {
        // effective = 1000 − 200 = 800; pinned = 800 × 0.25 = 200.
        assert!(check_context_budget(1000, 200, 201, 0.25).is_err()); // over
        assert!(check_context_budget(1000, 200, 200, 0.25).is_ok()); // boundary ok
    }

    #[test]
    fn set_context_window_leaves_field_unchanged_on_validation_failure() {
        // A valid baseline: window 100k → pinned = (100k − 8192) × 0.25 = 22952 ≥ summary 1200.
        let mut cfg = AgentConfig {
            prompt: None,
            system_prompt: String::new(),
            max_tool_rounds: usize::MAX,
            workspace_root: PathBuf::from("/tmp"),
            context_window_tokens: 100_000,
            output_reserve_tokens: 8_192,
            summary_max_tokens: 1_200,
            tool_timeout_secs: 120,
            skills: vec![],
            retry_policy: RetryPolicy::default(),
            pinned_budget_ratio: 0.25,
            context_thresholds: vec![50, 80],
            token_budget_warnings: vec![80, 95],
            agent_id: None,
            created_by: None,
            permissions: PermissionProfile::new(PathBuf::from("/tmp")),
            permissions_class: PermissionClass::default(),
            role: String::new(),
            description: String::new(),
        };
        // A 10k window → pinned = (10k − 8192) × 0.25 = 452 < summary 1200 → rejected.
        let err = cfg.set_context_window(10_000).unwrap_err();
        assert!(
            format!("{err}").contains("summary_max_tokens"),
            "got: {err}"
        );
        // Validate-before-mutate: the rejected window must NOT have been adopted.
        assert_eq!(cfg.context_window_tokens, 100_000);
        // And a valid window is adopted.
        cfg.set_context_window(200_000).unwrap();
        assert_eq!(cfg.context_window_tokens, 200_000);
    }

    #[test]
    fn try_context_window_validates_without_mutating() {
        // The failover pre-advance probe: same rejections as `check_context_budget`, but it must
        // NOT install the window (unlike `set_context_window`). Baseline 100k is feasible.
        let cfg = AgentConfig {
            prompt: None,
            system_prompt: String::new(),
            max_tool_rounds: usize::MAX,
            workspace_root: PathBuf::from("/tmp"),
            context_window_tokens: 100_000,
            output_reserve_tokens: 8_192,
            summary_max_tokens: 1_200,
            tool_timeout_secs: 120,
            skills: vec![],
            retry_policy: RetryPolicy::default(),
            pinned_budget_ratio: 0.25,
            context_thresholds: vec![50, 80],
            token_budget_warnings: vec![80, 95],
            agent_id: None,
            created_by: None,
            permissions: PermissionProfile::new(PathBuf::from("/tmp")),
            permissions_class: PermissionClass::default(),
            role: String::new(),
            description: String::new(),
        };
        assert!(cfg.try_context_window(0).is_err(), "zero window rejected");
        assert!(
            cfg.try_context_window(8_000).is_err(),
            "output_reserve (8192) ≥ window rejected"
        );
        // 10k → pinned = (10k − 8192) × 0.25 = 452 < summary 1200 → infeasible (the failover skip).
        assert!(cfg.try_context_window(10_000).is_err());
        assert!(cfg.try_context_window(200_000).is_ok());
        assert_eq!(
            cfg.context_window_tokens, 100_000,
            "try_context_window must not install the window"
        );
    }

    #[test]
    fn pinned_budget_matches_effective_times_ratio() {
        // effective = 100000 − 8192 = 91808; pinned = 91808 × 0.25 = 22952.
        let cfg = AgentConfig {
            prompt: None,
            system_prompt: String::new(),
            max_tool_rounds: usize::MAX,
            workspace_root: PathBuf::from("/tmp"),
            context_window_tokens: 100_000,
            output_reserve_tokens: 8_192,
            summary_max_tokens: 1_200,
            tool_timeout_secs: 120,
            skills: vec![],
            retry_policy: RetryPolicy::default(),
            pinned_budget_ratio: 0.25,
            context_thresholds: vec![50, 80],
            token_budget_warnings: vec![80, 95],
            agent_id: None,
            created_by: None,
            permissions: PermissionProfile::new(PathBuf::from("/tmp")),
            permissions_class: PermissionClass::default(),
            role: String::new(),
            description: String::new(),
        };
        assert_eq!(cfg.effective_budget(), 91_808);
        assert_eq!(cfg.pinned_budget(), 22_952);
    }

    #[test]
    fn policy_preset_display_roundtrip() {
        for preset in [PolicyPreset::AllowAll, PolicyPreset::AskAll] {
            let s = preset.to_string();
            assert_eq!(s.parse::<PolicyPreset>().unwrap(), preset);
        }
    }

    #[test]
    fn policy_preset_rejects_invalid() {
        assert!("default".parse::<PolicyPreset>().is_err());
        assert!("copilot".parse::<PolicyPreset>().is_err());
        assert!("".parse::<PolicyPreset>().is_err());
    }

    #[test]
    fn preset_allow_all_sets_everything_to_allow() {
        let policy = tool_policy_from_preset(PolicyPreset::AllowAll);
        assert_eq!(policy.default, PolicyDecision::Allow);
        assert!(policy.tools.is_empty());
        // decision_for falls back to default.
        assert_eq!(policy.decision_for("bash_exec"), PolicyDecision::Allow);
        assert_eq!(policy.decision_for("unknown_tool"), PolicyDecision::Allow);
    }

    #[test]
    fn preset_ask_all_sets_everything_to_ask() {
        let policy = tool_policy_from_preset(PolicyPreset::AskAll);
        assert_eq!(policy.default, PolicyDecision::Ask);
        assert!(policy.tools.is_empty());
        assert_eq!(policy.decision_for("bash_exec"), PolicyDecision::Ask);
        assert_eq!(policy.decision_for("unknown_tool"), PolicyDecision::Ask);
    }

    #[test]
    fn tool_policy_from_env_no_vars_returns_default() {
        temp_env::with_vars_unset(["KALLIP_POLICY_PRESET", "KALLIP_ALLOW_TOOLS"], || {
            let policy = tool_policy_from_env();
            let expected = default_tool_policy();
            assert_eq!(policy.default, expected.default);
            assert_eq!(policy.tools, expected.tools);
        });
    }

    #[test]
    fn tool_policy_from_env_preset_allow_all() {
        temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("allow-all"))], || {
            let policy = tool_policy_from_env();
            assert_eq!(policy.default, PolicyDecision::Allow);
            assert!(policy.tools.is_empty());
        });
    }

    #[test]
    fn tool_policy_from_env_preset_ask_all() {
        temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("ask-all"))], || {
            let policy = tool_policy_from_env();
            assert_eq!(policy.default, PolicyDecision::Ask);
            assert!(policy.tools.is_empty());
        });
    }

    #[test]
    fn tool_policy_from_env_whitespace_padded_preset() {
        temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("  allow-all  "))], || {
            let policy = tool_policy_from_env();
            assert_eq!(policy.default, PolicyDecision::Allow);
            assert!(policy.tools.is_empty());
        });
    }

    #[test]
    fn tool_policy_from_env_preset_takes_precedence_over_allow_tools() {
        temp_env::with_vars(
            [
                ("KALLIP_POLICY_PRESET", Some("ask-all")),
                ("KALLIP_ALLOW_TOOLS", Some("bash_exec")),
            ],
            || {
                let policy = tool_policy_from_env();
                // Preset wins — should be ask-all, not the allow-tools policy.
                assert_eq!(policy.default, PolicyDecision::Ask);
                assert!(policy.tools.is_empty());
            },
        );
    }

    #[test]
    #[should_panic(expected = "KALLIP_POLICY_PRESET: invalid policy preset")]
    fn tool_policy_from_env_invalid_preset_panics() {
        temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("gibberish"))], || {
            let _ = tool_policy_from_env();
        });
    }

    #[test]
    fn tool_policy_from_env_empty_preset_falls_through() {
        temp_env::with_vars([("KALLIP_POLICY_PRESET", Some(""))], || {
            let policy = tool_policy_from_env();
            let expected = default_tool_policy();
            assert_eq!(policy.default, expected.default);
        });
    }

    #[test]
    fn tool_policy_from_env_allow_tools_still_works() {
        temp_env::with_vars(
            [
                ("KALLIP_POLICY_PRESET", None::<&str>),
                ("KALLIP_ALLOW_TOOLS", Some("bash_exec")),
            ],
            || {
                let policy = tool_policy_from_env();
                assert_eq!(policy.default, PolicyDecision::Ask);
                assert_eq!(policy.decision_for("bash_exec"), PolicyDecision::Allow);
            },
        );
    }

    #[test]
    #[should_panic(expected = "KALLIP_POLICY_PRESET: invalid policy preset")]
    fn tool_policy_from_env_invalid_preset_panics_even_with_allow_tools() {
        temp_env::with_vars(
            [
                ("KALLIP_POLICY_PRESET", Some("gibberish")),
                ("KALLIP_ALLOW_TOOLS", Some("bash_exec")),
            ],
            || {
                let _ = tool_policy_from_env();
            },
        );
    }
}
