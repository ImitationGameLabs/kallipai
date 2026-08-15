use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::env_util::{DEFAULT_CONTEXT_WINDOW_TOKENS, parse_env, parse_env_list};
use crate::retry::RetryPolicy;
use kallip_common::AgentId;
use kallip_common::policy::PolicyPreset;

const DEFAULT_SYSTEM_PROMPT: &str = concat!(
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
const DEFAULT_MAX_TOOL_ROUNDS: usize = usize::MAX;
/// Default cap on consecutive heartbeat rounds (bare-assistant re-loops) before
/// the harness force-idles the agent. Bounds "self-monologue" token burn; the
/// tagma-wide token budget remains the overall hard ceiling. Three is a firm
/// nudge: one accidental bare response, a reminder, then a stop.
const DEFAULT_MAX_HEARTBEAT_ROUNDS: u32 = 3;
/// Default cap on consecutive transient (failover-chain-exhausted) parks that get
/// a timed retry. After this (or the `retry_timeout` wall clock), the agent hard-
/// parks and surfaces to the operator instead of re-hammering a downed provider.
const DEFAULT_MAX_TRANSIENT_RETRIES: u32 = 3;
const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 1_200;
const DEFAULT_OUTPUT_RESERVE_TOKENS: usize = 8_192;
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_RETRIES: u32 = 3;
const DEFAULT_RETRY_BASE_DELAY_SECS: u64 = 1;
const DEFAULT_PINNED_BUDGET_RATIO: f64 = 0.25;
const DEFAULT_CONTEXT_THRESHOLDS: &[u8] = &[50, 60, 70, 80];
const DEFAULT_TOKEN_BUDGET_WARNINGS: &[u8] = &[80, 95];

/// Resolve the tagma-global `bash_exec` classify preset from
/// `KALLIP_POLICY_PRESET`.
///
/// Unset or empty → [`PolicyPreset::Default`] (strict). Accepts `default`, `auto`,
/// and `allow-all`. An unrecognized value is a fatal misconfiguration (the preset
/// is structural to the sandbox), so it panics — matching the env-knob convention
/// of [`permission_class_from_env`]. Only read once at tagma startup; the preset
/// is immutable for the tagma's lifetime.
pub fn policy_preset_from_env() -> PolicyPreset {
    let Ok(raw) = std::env::var("KALLIP_POLICY_PRESET") else {
        return PolicyPreset::Default;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return PolicyPreset::Default;
    }
    raw.parse::<PolicyPreset>().unwrap_or_else(|e| {
        panic!("KALLIP_POLICY_PRESET: {e}");
    })
}

mod exec_hooks;
pub use exec_hooks::load_exec_hook_rules;
/// Resolve the root agent's permission class from `KALLIP_ROOT_AGENT_PERMISSION_CLASS`.
///
/// Root-only test knob, parallel to [`policy_preset_from_env`]: read in the
/// tagma's root-create branch, never on the subagent or restore paths
/// (subagents derive their class from `ceiling_for_tier`; restore uses the
/// persisted `meta.json`). Accepts lowercase `"normal"` / `"guest"` — the env-var
/// convention, distinct from the PascalCase serde form persisted in `meta.json`.
/// Panics on an invalid value, matching [`policy_preset_from_env`]'s misconfig behavior.
pub fn permission_class_from_env() -> PermissionClass {
    let Ok(raw) = std::env::var("KALLIP_ROOT_AGENT_PERMISSION_CLASS") else {
        return PermissionClass::default();
    };
    // Trim here, not inside FromStr: the wire/env convention trims surrounding
    // whitespace, but FromStr stays trim-free so the tagma rejects untrimmed
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

/// How a subagent relates to its supervisor's workspace write-lock.
///
/// Serialized `snake_case` for both the wire (`CreateAgentRequest`) and the
/// persisted (`AgentMeta`) form. This intentionally diverges from
/// [`PermissionClass`], which keeps a PascalCase persisted form distinct from its
/// lowercase wire/env spelling: `DelegationMode` is newer and has no env-var
/// spelling, so one shared lowercase form is simpler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationMode {
    /// The subagent scopes into a proper subdirectory of the supervisor's
    /// workspace (the default). The supervisor keeps its root write-lock; the
    /// subdirectory becomes a readonly hole to the supervisor via the delegation
    /// carve-out.
    #[default]
    CarveOut,
    /// The subagent takes the supervisor's *entire* workspace: the supervisor's
    /// root write-lock is transferred to the child at spawn and transferred back
    /// on removal, so the supervisor's next shell loses workspace write until the
    /// child is gone.
    ///
    /// Exclusive: a supervisor with a `FullHandoff` child may have no other
    /// child (CarveOut or FullHandoff). Enforced at spawn; a legacy/corrupt
    /// on-disk tree that violates it may either fault on restore (one
    /// interleaving) or silently overlap (the other) -- normal operation never
    /// produces such a tree, so this is a defense note, not a live concern.
    ///
    /// Reactivation: while a `FullHandoff` child is Live the workspace write-lock
    /// is reassigned to the child, so `release_all(supervisor)` is a no-op and
    /// the supervisor's reactivation `try_acquire_workspace_lock` returns
    /// `Busy { holder: child }`. The supervisor therefore cannot reactivate
    /// (restart its task) until the child is removed -- the supervisor genuinely
    /// cannot write its workspace while the child holds the lock.
    FullHandoff,
}

impl std::str::FromStr for DelegationMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            kallip_common::protocol::DELEGATION_CARVE_OUT => Ok(Self::CarveOut),
            kallip_common::protocol::DELEGATION_FULL_HANDOFF => Ok(Self::FullHandoff),
            other => Err(format!(
                "unknown delegation_mode '{other}' (expected '{}' or '{}')",
                kallip_common::protocol::DELEGATION_CARVE_OUT,
                kallip_common::protocol::DELEGATION_FULL_HANDOFF
            )),
        }
    }
}

impl std::fmt::Display for DelegationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CarveOut => f.write_str(kallip_common::protocol::DELEGATION_CARVE_OUT),
            Self::FullHandoff => f.write_str(kallip_common::protocol::DELEGATION_FULL_HANDOFF),
        }
    }
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
/// wire/env spelling. Surfaced by the tagma as a `400 Bad Request` body, so the
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
/// `set_pinned_budget` runs later and independently (at spawn via the tagma, and on within-tier
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
mod tests;
