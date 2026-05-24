use std::path::PathBuf;

use anyhow::{Context, Result, bail};

const DEFAULT_SYSTEM_PROMPT: &str = "You are a minimal coding agent. Use shell_session_exec for shell commands. Use shell_session_create to create persistent shell sessions, shell_session_list to inspect them, shell_session_capture to inspect recent output, and shell_session_restart or shell_session_kill when session lifecycle control is necessary. Keep answers concise and prefer the least risky tool that can accomplish the task.\n\nWhen a tool returns {\"deferred\": true, \"request_id\": \"...\"}, the action was NOT executed and is pending approval. Continue with other work. When you see an approval notification in context, call approval_redeem with the request_id to execute. Call approval_list to check status, approval_cancel if you no longer need a pending action.";
const DEFAULT_MAX_TOOL_ROUNDS: usize = 32;
const DEFAULT_COMPACT_MAX_TOKENS: u32 = 1_200;
const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 128_000;
const DEFAULT_OUTPUT_RESERVE_TOKENS: usize = 8_192;
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;

/// Runtime configuration for `just-agent`.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub prompt: Option<String>,
    pub system_prompt: String,
    pub max_tool_rounds: usize,
    pub workspace_root: PathBuf,
    pub context_window_tokens: usize,
    pub output_reserve_tokens: usize,
    pub compact_max_tokens: u32,
    pub tool_timeout_secs: u64,
    pub skills: Vec<String>,
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
        let compact_max_tokens = parse_env::<u32>("JUST_AGENT_COMPACT_MAX_TOKENS")?
            .unwrap_or(DEFAULT_COMPACT_MAX_TOKENS);
        let tool_timeout_secs =
            parse_env::<u64>("JUST_AGENT_TOOL_TIMEOUT_SECS")?.unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS);

        let workspace_root = workspace_root.canonicalize().with_context(|| {
            format!(
                "failed to resolve workspace root {}",
                workspace_root.display()
            )
        })?;

        if compact_max_tokens == 0 {
            bail!("JUST_AGENT_COMPACT_MAX_TOKENS must be greater than zero");
        }
        if max_tool_rounds == 0 {
            bail!("JUST_AGENT_MAX_TOOL_ROUNDS must be greater than zero");
        }
        if context_window_tokens == 0 {
            bail!("JUST_AGENT_CONTEXT_WINDOW_TOKENS must be greater than zero");
        }

        Ok(Self {
            prompt,
            system_prompt,
            max_tool_rounds,
            workspace_root,
            context_window_tokens,
            output_reserve_tokens,
            compact_max_tokens,
            tool_timeout_secs,
            skills,
        })
    }
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
