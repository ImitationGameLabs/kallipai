use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "just-agent", about = "Agent CLI: daemon client")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start a new agent via daemon
    Start(StartArgs),
    /// Send prompt to agent and wait for result
    Send(SendArgs),
    /// List all agents
    List(DaemonArgs),
    /// Stop an agent
    Stop(IdArgs),
    /// Stream agent events
    Events(IdArgs),
    /// Show agent context usage
    Status(IdArgs),
    /// Interrupt current agent operation
    Interrupt(IdArgs),
    /// Respond to approval request
    Approve(ApproveArgs),
}

#[derive(Args)]
pub struct DaemonArgs {
    /// Daemon URL.
    #[arg(long, env = "JUST_AGENT_DAEMON_URL", default_value = "http://localhost:3000")]
    pub daemon_url: String,
}

#[derive(Args)]
pub struct StartArgs {
    #[command(flatten)]
    pub daemon: DaemonArgs,
    /// Working directory for the agent.
    #[arg(long)]
    pub workspace_root: Option<String>,
    /// Activate a skill by name (repeatable).
    #[arg(long = "skill", value_delimiter = ',')]
    pub skills: Vec<String>,
    /// Optional initial prompt for the agent.
    #[arg(long)]
    pub prompt: Option<String>,
}

#[derive(Args)]
pub struct SendArgs {
    #[command(flatten)]
    pub daemon: DaemonArgs,
    /// Agent ID.
    pub id: String,
    /// Prompt text to send.
    pub prompt: String,
    /// Timeout in seconds.
    #[arg(long, default_value = "300")]
    pub timeout: u64,
}

#[derive(Args)]
pub struct IdArgs {
    #[command(flatten)]
    pub daemon: DaemonArgs,
    /// Agent ID.
    pub id: String,
}

#[derive(Args)]
pub struct ApproveArgs {
    #[command(flatten)]
    pub daemon: DaemonArgs,
    /// Agent ID.
    pub id: String,
    /// Request ID of the deferred action.
    pub request_id: String,
    /// Approval decision: approve or deny.
    pub decision: String,
}
