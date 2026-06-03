use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::retry::RetryPolicy;
use just_agent_common::types::{AgentId, PolicyDecision, ToolPolicy};

const DEFAULT_SYSTEM_PROMPT: &str = "You are a minimal coding agent. Use shell_session_exec for shell commands. Use shell_session_create to create persistent shell sessions, shell_session_list to inspect them, shell_session_capture to inspect recent output, and shell_session_restart or shell_session_kill when session lifecycle control is necessary. Keep answers concise and prefer the least risky tool that can accomplish the task.\n\nWhen a tool returns {\"pending_approval\": true, \"id\": \"...\"}, the action was deferred and is pending authorization. Continue with other work. When you see an approval notification in context, call approval_redeem with the id to execute. Call approval_list to check status, approval_cancel if you no longer need a pending approval.";
const DEFAULT_MAX_TOOL_ROUNDS: usize = 32;
const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 1_200;
const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 128_000;
const DEFAULT_OUTPUT_RESERVE_TOKENS: usize = 8_192;
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_RETRIES: u32 = 3;
const DEFAULT_RETRY_BASE_DELAY_SECS: u64 = 1;
const DEFAULT_PINNED_BUDGET_RATIO: f64 = 0.25;
const DEFAULT_CONTEXT_THRESHOLDS: &[u8] = &[50, 60, 70, 80];

/// Default tool policy matching the current hardcoded behavior.
///
/// Lives in the runtime crate because it encodes knowledge of specific tool
/// names defined by the runtime's tool registry.
pub fn default_tool_policy() -> ToolPolicy {
    use std::collections::BTreeMap;
    let mut tools = BTreeMap::new();
    tools.insert("shell_session_list".into(), PolicyDecision::Allow);
    tools.insert("shell_session_capture".into(), PolicyDecision::Allow);
    tools.insert("shell_session_create".into(), PolicyDecision::Allow);
    tools.insert("shell_session_kill".into(), PolicyDecision::Ask);
    tools.insert("shell_session_restart".into(), PolicyDecision::Ask);
    tools.insert("shell_session_exec".into(), PolicyDecision::Classify);
    tools.insert("context_pin".into(), PolicyDecision::Allow);
    tools.insert("context_unpin".into(), PolicyDecision::Allow);
    tools.insert("context_status".into(), PolicyDecision::Allow);
    tools.insert("context_evict".into(), PolicyDecision::Allow);
    tools.insert("skill_load".into(), PolicyDecision::Allow);
    ToolPolicy {
        default: PolicyDecision::Ask,
        tools,
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

/// Runtime configuration for `just-agent`.
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
    pub agent_id: Option<AgentId>,
    pub created_by: Option<AgentId>,
    pub permissions: PermissionProfile,
}

impl AgentConfig {
    /// Loads configuration from CLI arguments and environment variables.
    pub fn load(
        prompt: Option<String>,
        skills: Vec<String>,
        workspace_root: Option<PathBuf>,
    ) -> Result<Self> {
        let system_prompt = std::env::var("JUST_AGENT_SYSTEM_PROMPT")
            .unwrap_or_else(|_| DEFAULT_SYSTEM_PROMPT.into());
        let max_tool_rounds =
            parse_env::<usize>("JUST_AGENT_MAX_TOOL_ROUNDS")?.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS);
        let workspace_root = workspace_root
            .or_else(|| {
                std::env::var("JUST_AGENT_WORKSPACE_ROOT")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?);
        let context_window_tokens = parse_env::<usize>("JUST_AGENT_CONTEXT_WINDOW_TOKENS")?
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        let output_reserve_tokens = parse_env::<usize>("JUST_AGENT_OUTPUT_RESERVE_TOKENS")?
            .unwrap_or(DEFAULT_OUTPUT_RESERVE_TOKENS);
        let summary_max_tokens = parse_env::<u32>("JUST_AGENT_SUMMARY_MAX_TOKENS")?
            .unwrap_or(DEFAULT_SUMMARY_MAX_TOKENS);
        let tool_timeout_secs =
            parse_env::<u64>("JUST_AGENT_TOOL_TIMEOUT_SECS")?.unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS);

        let pinned_budget_ratio = parse_env::<f64>("JUST_AGENT_PINNED_BUDGET_RATIO")?
            .unwrap_or(DEFAULT_PINNED_BUDGET_RATIO);
        let context_thresholds = parse_env_list::<u8>("JUST_AGENT_CONTEXT_THRESHOLDS")?
            .unwrap_or_else(|| DEFAULT_CONTEXT_THRESHOLDS.to_vec());
        let max_retries =
            parse_env::<u32>("JUST_AGENT_MAX_RETRIES")?.unwrap_or(DEFAULT_MAX_RETRIES);
        let retry_base_delay_secs = parse_env::<u64>("JUST_AGENT_RETRY_BASE_DELAY_SECS")?
            .unwrap_or(DEFAULT_RETRY_BASE_DELAY_SECS);
        if retry_base_delay_secs == 0 {
            bail!("JUST_AGENT_RETRY_BASE_DELAY_SECS must be greater than zero");
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
            bail!("JUST_AGENT_SUMMARY_MAX_TOKENS must be greater than zero");
        }
        if max_tool_rounds == 0 {
            bail!("JUST_AGENT_MAX_TOOL_ROUNDS must be greater than zero");
        }
        if context_window_tokens == 0 {
            bail!("JUST_AGENT_CONTEXT_WINDOW_TOKENS must be greater than zero");
        }
        if output_reserve_tokens >= context_window_tokens {
            bail!(
                "JUST_AGENT_OUTPUT_RESERVE_TOKENS ({output_reserve_tokens}) must be less than \
                 JUST_AGENT_CONTEXT_WINDOW_TOKENS ({context_window_tokens})"
            );
        }
        if !(0.0..1.0).contains(&pinned_budget_ratio) {
            bail!("JUST_AGENT_PINNED_BUDGET_RATIO must be between 0.0 and 1.0 (exclusive)");
        }
        let effective_budget = context_window_tokens.saturating_sub(output_reserve_tokens);
        let pinned_budget = (effective_budget as f64 * pinned_budget_ratio) as usize;
        if summary_max_tokens as usize > pinned_budget {
            bail!(
                "JUST_AGENT_SUMMARY_MAX_TOKENS ({summary_max_tokens}) exceeds pinned budget \
                 ({pinned_budget} = effective_budget {effective_budget} × ratio {pinned_budget_ratio}). \
                 Increase PINNED_BUDGET_RATIO or CONTEXT_WINDOW_TOKENS, or reduce SUMMARY_MAX_TOKENS."
            );
        }
        if context_thresholds.len() < 2 {
            bail!(
                "JUST_AGENT_CONTEXT_THRESHOLDS must have at least 2 values (warnings + auto-compact)"
            );
        }
        if !context_thresholds.is_sorted() {
            bail!("JUST_AGENT_CONTEXT_THRESHOLDS must be sorted ascending");
        }
        if context_thresholds.iter().any(|&t| !(1..=99).contains(&t)) {
            bail!("JUST_AGENT_CONTEXT_THRESHOLDS values must be 1-99");
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
            agent_id: None,
            created_by: None,
            permissions: PermissionProfile::new(workspace_root),
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
}

/// Parse a comma-separated list env var into a Vec<T>.
fn parse_env_list<T: std::str::FromStr>(name: &str) -> Result<Option<Vec<T>>> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let items: Result<Vec<T>, _> = value.split(',').map(|s| s.trim().parse()).collect();
    let items = items.map_err(|_| {
        anyhow::anyhow!(
            "{name} must be a comma-separated list of {}",
            std::any::type_name::<T>()
        )
    })?;
    Ok(Some(items))
}

fn parse_env<T: std::str::FromStr>(name: &str) -> Result<Option<T>> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value.parse::<T>().map_err(|_| {
                anyhow::anyhow!("{name} must be a valid {}", std::any::type_name::<T>())
            })
        })
        .transpose()
}
