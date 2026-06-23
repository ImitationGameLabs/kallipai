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
    /// Skill discovery
    #[command(subcommand)]
    Skill(SkillCommand),
    /// Manage skill promote requests (review-based promote flow)
    #[command(subcommand)]
    PromoteRequest(PromoteRequestCommand),
    /// Manage agent token budget
    #[command(subcommand)]
    Budget(BudgetCommand),
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Spawn a new agent via daemon
    Spawn(SpawnArgs),
    /// Send message to agent
    Send(SendArgs),
    /// List agents (optionally only a superior's direct subagents)
    List(ListArgs),
    /// Remove an agent
    Remove(IdArgs),
    /// Stream agent events
    Events(IdArgs),
    /// Show agent context usage
    Status(IdArgs),
    /// Show agent permissions and tool policy
    Permissions(IdArgs),
    /// Interrupt current agent operation
    Interrupt(IdArgs),
    /// Update an agent's role and/or description (direct supervisor only)
    Metadata(MetadataArgs),
    /// Report this agent's current activity (self-only; reads JUST_AGENT_ID)
    Activity(ActivityArgs),
}

#[derive(Args)]
pub struct SpawnArgs {
    /// Working directory for the agent.
    #[arg(long)]
    pub workspace_root: Option<String>,
    /// Activate a skill by name (repeatable).
    #[arg(long = "skill", value_delimiter = ',')]
    pub skills: Vec<String>,
    /// Optional initial prompt for the agent.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Short display label ("researcher"). Required for subagent spawns.
    #[arg(long)]
    pub role: Option<String>,
    /// Longer prose: what this agent is for.
    #[arg(long)]
    pub description: Option<String>,
}

#[derive(Args, Default)]
pub struct ListArgs {
    /// Only list the direct subagents of this superior agent.
    #[arg(long)]
    pub created_by: Option<AgentId>,
}

#[derive(Args)]
pub struct MetadataArgs {
    /// Agent ID.
    pub id: AgentId,
    /// New role. Must be non-empty if provided.
    #[arg(long)]
    pub role: Option<String>,
    /// New description. Use the empty string to clear.
    #[arg(long)]
    pub description: Option<String>,
}

#[derive(Args)]
pub struct ActivityArgs {
    /// Current activity, in a short phrase (e.g. "reading docs/x.md"). Pass an
    /// empty string to clear. Field name matches `UpdateActivityRequest::activity`.
    pub activity: String,
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
    /// Show agent bash_exec command-policy overrides
    ExecGet(IdArgs),
    /// Set a per-command bash_exec override (superior-only)
    ExecSet(ExecSetArgs),
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

#[derive(Args)]
pub struct ExecSetArgs {
    /// Agent ID.
    pub id: AgentId,
    /// Command name (e.g. cargo, sudo).
    pub command: String,
    /// Decision: allow, ask, deny.
    pub decision: String,
}

#[derive(Subcommand)]
pub enum SkillCommand {
    /// Show skill directory paths
    Paths(SkillPathsArgs),
    /// Show metadata for a specific skill
    Meta(SkillMetaArgs),
}

#[derive(Args)]
pub struct SkillPathsArgs;

#[derive(Args)]
pub struct SkillMetaArgs {
    /// Skill name (supports nested paths like code/refactoring).
    pub name: String,
}

// ---------------------------------------------------------------------------
// Promote request commands (review-based promote flow)
// ---------------------------------------------------------------------------

/// Top-level promote-request commands used by agents via shell.
#[derive(Subcommand)]
pub enum PromoteRequestCommand {
    /// Submit a promote request for the current agent's local skill.
    Submit(PromoteRequestSubmitArgs),
    /// List promote requests (open to all agents for visibility).
    List {
        /// Filter by status: pending, approved, denied.
        #[arg(long)]
        status: Option<String>,
    },
    /// Show old/new content of a promote request for diff review.
    Show {
        /// Request ID.
        id: String,
    },
    /// Approve a pending promote request.
    Approve {
        /// Request ID.
        id: String,
    },
    /// Deny a pending promote request.
    Deny {
        /// Request ID.
        id: String,
        /// Reason for denial.
        reason: Option<String>,
    },
}

#[derive(Args)]
pub struct PromoteRequestSubmitArgs {
    /// Skill name to promote (supports nested paths like code/refactoring).
    pub name: String,
}

// ---------------------------------------------------------------------------
// Budget commands
// ---------------------------------------------------------------------------

/// Manage daemon-wide token budget.
#[derive(Subcommand)]
pub enum BudgetCommand {
    /// Show daemon-wide token budget status
    Get,
    /// Increase daemon-wide token budget
    Increase(BudgetAmountArgs),
    /// Decrease daemon-wide token budget
    Decrease(BudgetAmountArgs),
    /// Set remaining daemon-wide token budget (=0 pauses all agents)
    Set(BudgetAmountArgs),
}

#[derive(Args)]
pub struct BudgetAmountArgs {
    /// Token amount (supports K, M, G suffixes, e.g. 100M, 500K, 1G).
    pub amount: String,
}
