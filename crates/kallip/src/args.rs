use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use kallip_common::agentid::AgentId;

#[derive(Parser)]
#[command(
    name = "kallip",
    version,
    about = "Headless CLI for agents to coordinate with and manage other agents"
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
    /// Manage agent permissions and bash_exec exec-policy overrides
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// Skill discovery
    #[command(subcommand)]
    Skill(SkillCommand),
    /// Manage agent token budget
    #[command(subcommand)]
    Budget(BudgetCommand),
    /// Manage this agent's direct subagents
    #[command(subcommand)]
    Subagent(SubagentCommand),
    /// Manage directory write-locks (mutual exclusion across agents)
    #[command(subcommand)]
    Dirlock(DirlockCommand),
    /// Deliver messages to the user
    #[command(subcommand)]
    Lesche(LescheCommand),
}

/// Ungrouped per-agent ops, flattened into the top-level command list — they
/// never appear as an "agent" group in `--help`.
#[derive(Subcommand)]
pub enum AgentCommand {
    /// Send a message to an agent
    Message(MessageArgs),
    /// Show agent context usage
    Status(IdArgs),
    /// Report this agent's current activity (self-only)
    Activity(ActivityArgs),
}

/// Deliver messages to the user via the tagma's relay (the lesche data-plane).
/// Targets the calling agent (resolved from `KALLIP_ID`), like `activity`.
///
/// The primitive is "send a message", not "reply": a message may be a response,
/// a proactive heads-up, or (future) a file. Today only text is supported.
#[derive(Subcommand)]
pub enum LescheCommand {
    /// Send a text message to the user (via the tagma's relay, when attached).
    Send(SendArgs),
}

/// Text payload for `kallip lesche send`. The text is a positional argument;
/// when omitted, the entire stdin is read (multiline — pipe, heredoc, or
/// `< file` all work).
#[derive(Args)]
pub struct SendArgs {
    /// The message text. If omitted, reads the full text from stdin (multiline).
    #[arg(allow_hyphen_values = true)]
    pub text: Option<String>,
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
    /// Short display label (e.g. "researcher"). Required by the tagma when
    /// spawning a subordinate (the only spawn path: `subagent spawn`).
    #[arg(long)]
    pub role: Option<String>,
    /// Longer prose: what this agent is for.
    #[arg(long)]
    pub description: Option<String>,
    /// Explicitly downgrade the subagent's FS-access permission class
    /// (`normal` = home+workspace read-write, `guest` = read-only). Omit to
    /// grant the tier's default ceiling. Honored only for subagent spawns; the
    /// tagma rejects a value above the tier ceiling or the supervisor's class.
    #[arg(long, value_name = "CLASS", value_parser = ["normal", "guest"])]
    pub permission_class: Option<String>,
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
pub struct MessageArgs {
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
    /// Show full agent permissions and the active classify preset
    Show(IdArgs),
    /// Show agent bash_exec command-policy overrides
    ExecGet(IdArgs),
    /// Set a per-command bash_exec override (superior-only)
    ExecSet(ExecSetArgs),
}

#[derive(Args)]
pub struct ExecSetArgs {
    /// Agent ID.
    pub id: AgentId,
    /// Command name (e.g. cargo, sudo).
    pub command: String,
    /// Decision: allow, ask, deny.
    pub decision: String,
    /// Optional reason surfaced to the agent when the decision narrows (ask/deny).
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Subcommand)]
pub enum SkillCommand {
    /// Generate the skill index for a directory from each file's frontmatter
    ///
    /// Reads the directory at `path` directly and prints a markdown index of
    /// its entries: each `.md` skill (from its frontmatter) and each
    /// subdirectory (from its `README.md` frontmatter). The agent passes the
    /// `skills path` from its identity facts, then pins this output.
    Index(SkillIndexArgs),
    /// Show metadata for a specific skill
    Meta(SkillMetaArgs),
}

#[derive(Args)]
pub struct SkillIndexArgs {
    /// Absolute path of the skill directory to index.
    pub path: PathBuf,
}

#[derive(Args)]
pub struct SkillMetaArgs {
    /// Path to the skill — the stem (`<skills>/agent/kallip`) or the full
    /// `<skills>/agent/kallip.md`. Read directly from the filesystem.
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// Budget commands
// ---------------------------------------------------------------------------

/// Manage tagma-wide token budget.
#[derive(Subcommand)]
pub enum BudgetCommand {
    /// Show tagma-wide token budget status
    Get,
    /// Increase tagma-wide token budget
    Increase(BudgetAmountArgs),
    /// Decrease tagma-wide token budget
    Decrease(BudgetAmountArgs),
    /// Set remaining tagma-wide token budget (=0 pauses all agents)
    Set(BudgetAmountArgs),
}

#[derive(Args)]
pub struct BudgetAmountArgs {
    /// Token amount (supports K, M, G suffixes, e.g. 100M, 500K, 1G).
    pub amount: String,
}

// ---------------------------------------------------------------------------
// Subagent commands — manage the current agent's (KALLIP_ID) direct subagents
// ---------------------------------------------------------------------------

/// Manage the current agent's direct subagents. The acting superior is taken
/// from the `KALLIP_ID` env var, so these commands only make sense inside
/// an agent context.
#[derive(Subcommand)]
pub enum SubagentCommand {
    /// Spawn a direct subagent of the current agent
    Spawn(SpawnArgs),
    /// List the current agent's direct subagents
    List,
    /// Remove a direct subagent
    Remove(IdArgs),
    /// Interrupt a direct subagent's current operation
    Interrupt(IdArgs),
    /// Update a direct subagent's role and/or description
    Metadata(MetadataArgs),
}

// ---------------------------------------------------------------------------
// Dirlock commands — directory write-locks (self-scoped via KALLIP_ID)
// ---------------------------------------------------------------------------

/// Manage this agent's directory write-locks. The acting agent is taken from
/// the `KALLIP_ID` env var (self-only acquire/release/status); `who` is a
/// global lookup. Agents drive these through `bash_exec`.
#[derive(Subcommand)]
pub enum DirlockCommand {
    /// Acquire the write-lock on a directory (self). On conflict the tagma
    /// returns the holder so you can peer-message it to coordinate.
    Acquire(DirlockPathArgs),
    /// Release the write-lock on a directory (self). Idempotent.
    Release(DirlockPathArgs),
    /// List the directories this agent currently holds write-locks on.
    Status,
    /// Show which agent holds the write-lock on a directory (or "unlocked").
    Who(DirlockDirArgs),
}

#[derive(Args)]
pub struct DirlockPathArgs {
    /// Directory to lock/unlock (absolute or relative to cwd).
    pub path: String,
    /// How long (seconds) to retry on conflict before returning the holder.
    #[arg(long)]
    pub timeout_secs: Option<u64>,
}

#[derive(Args)]
pub struct DirlockDirArgs {
    /// Directory to query.
    pub dir: String,
}
