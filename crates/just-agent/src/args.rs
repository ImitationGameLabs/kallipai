use clap::{Args, Parser, Subcommand};
use just_agent_common::types::AgentId;

#[derive(Parser)]
#[command(
    name = "just-agent",
    about = "Headless CLI to spawn, monitor, and orchestrate agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(flatten)]
    Agent(AgentCommand),
    /// Manage approvals
    #[command(subcommand)]
    Approval(ApprovalCommand),
}

#[derive(Subcommand)]
pub enum AgentCommand {
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
    pub id: AgentId,
    /// Message to send.
    pub message: String,
}

#[derive(Args)]
pub struct IdArgs {
    /// Agent ID.
    pub id: AgentId,
}

#[derive(Subcommand)]
pub enum ApprovalCommand {
    /// List approvals
    List(ApprovalListArgs),
    /// Show details of an approval
    Get(ApprovalGetArgs),
    /// Approve or deny
    Respond(ApprovalRespondArgs),
}

#[derive(Args)]
pub struct ApprovalListArgs {
    /// Page offset (0-based).
    #[arg(long)]
    pub offset: Option<u64>,
    /// Page size. Clamped to [1, 20]; defaults to 5.
    #[arg(long)]
    pub limit: Option<u64>,
    /// Filter by owning agent ID.
    #[arg(long)]
    pub requested_by: Option<String>,
    /// Show all statuses (default: committed only).
    #[arg(long, conflicts_with = "status")]
    pub all: bool,
    /// Filter by status: pending, committed, approved, denied, redeemed, cancelled.
    #[arg(long, conflicts_with = "all")]
    pub status: Option<String>,
    /// Reverse sort order (oldest first; default is newest first).
    #[arg(long)]
    pub reverse: bool,
}

#[derive(Args)]
pub struct ApprovalGetArgs {
    /// Approval ID.
    pub id: String,
}

#[derive(Args)]
pub struct ApprovalRespondArgs {
    /// Approval ID.
    pub id: String,
    /// Decision: approve or deny.
    pub decision: String,
}
