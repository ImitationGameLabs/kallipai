use clap::{Args, Parser, Subcommand};
use just_agent_common::agentid::AgentId;

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
    /// Manage agent tool policy
    #[command(subcommand)]
    Policy(PolicyCommand),
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
    /// Show agent permissions and tool policy
    Permissions(IdArgs),
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
    /// Approve a committed action
    Approve(ApprovalIdArgs),
    /// Deny a committed action
    Deny(ApprovalDenyArgs),
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
pub struct ApprovalIdArgs {
    /// Approval ID.
    pub id: String,
}

#[derive(Args)]
pub struct ApprovalDenyArgs {
    /// Approval ID.
    pub id: String,
    /// Reason for denial.
    pub reason: String,
}

#[derive(Subcommand)]
pub enum PolicyCommand {
    /// Show agent tool policy
    Get(IdArgs),
    /// Modify a single tool policy rule
    Set(PolicySetArgs),
}

#[derive(Args)]
pub struct PolicySetArgs {
    /// Agent ID.
    pub id: AgentId,
    /// Tool name.
    pub tool: String,
    /// Decision: allow, ask, deny, classify.
    pub decision: String,
}
