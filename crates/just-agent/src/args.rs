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
    /// Send message to agent
    Send(SendArgs),
    /// List all agents
    List,
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
pub struct StartArgs {
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
    /// Agent ID.
    pub id: String,
    /// Message to send.
    pub message: String,
}

#[derive(Args)]
pub struct IdArgs {
    /// Agent ID.
    pub id: String,
}

#[derive(Args)]
pub struct ApproveArgs {
    /// Agent ID.
    pub id: String,
    /// Request ID of the deferred action.
    pub request_id: String,
    /// Approval decision: approve or deny.
    pub decision: String,
}
