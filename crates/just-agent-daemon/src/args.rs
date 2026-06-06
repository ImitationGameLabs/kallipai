use clap::Parser;

/// Upper bound for max_agents.
pub(crate) const MAX_AGENTS_LIMIT: usize = 1000;
/// Upper bound for max_subagents.
pub(crate) const MAX_SUBAGENTS_LIMIT: usize = 100;

/// CLI arguments for just-agent-daemon.
#[derive(Parser)]
#[command(
    name = "just-agent-daemon",
    about = "HTTP API server hosting multiple agent instances"
)]
pub struct Args {
    /// Address to listen on.
    #[arg(long, env = "JUST_AGENT_DAEMON_ADDR", default_value = "127.0.0.1:3000")]
    pub listen_addr: String,
    /// URL that agents use to reach this daemon (injected into PTY env).
    #[arg(
        long,
        env = "JUST_AGENT_ADVERTISE_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    pub advertise_url: String,
    /// Max queued messages per agent (message channel capacity). Must be >= 1.
    #[arg(long, env = "JUST_AGENT_PROMPT_QUEUE_SIZE", default_value = "5")]
    pub prompt_queue_size: usize,
    /// Max concurrent agents. Range: 1..=1000.
    #[arg(long, env = "JUST_AGENT_MAX_AGENTS", default_value = "50")]
    pub max_agents: usize,
    /// Max direct subagents per agent. Range: 1..=100.
    #[arg(long, env = "JUST_AGENT_MAX_SUBAGENTS", default_value = "20")]
    pub max_subagents: usize,
    /// Max HTTP request body size in kilobytes. 0 = axum default (2 MB).
    #[arg(long, env = "JUST_AGENT_MAX_BODY_SIZE_KB", default_value = "1024")]
    pub max_body_size_kb: usize,
}
