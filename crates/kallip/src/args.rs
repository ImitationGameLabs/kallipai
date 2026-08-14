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
    /// Print the full auto-generated command reference and exit (no tagma
    /// connection needed). Pin its output (label `kallip:reference`) for
    /// one-stop command syntax.
    #[arg(long)]
    pub reference: bool,
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(flatten)]
    Agent(AgentCommand),
    /// Approvals gate tool actions that need supervisor sign-off (pending ->
    /// committed -> approved/denied -> redeemed/cancelled).
    #[command(subcommand)]
    Approval(ApprovalCommand),
    /// Manage agent permissions and bash_exec exec-policy overrides
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// Discover and inspect skills via the generated index.
    #[command(subcommand)]
    Skill(SkillCommand),
    /// Manage the tagma-wide token budget (shared by all agents; set 0 to
    /// pause everyone).
    #[command(subcommand)]
    Budget(BudgetCommand),
    /// Manage this agent's direct subagents
    #[command(subcommand)]
    Subagent(SubagentCommand),
    /// Manage directory write-locks (mutual exclusion across agents)
    #[command(subcommand)]
    Dirlock(DirlockCommand),
    /// Deliver messages to the user via the relay (the lesche data-plane).
    #[command(subcommand)]
    Lesche(LescheCommand),
    /// List, read, summarize, or clear this agent's message inbox.
    #[command(subcommand)]
    Inbox(InboxCommand),
}

/// Ungrouped per-agent ops, flattened into the top-level command list — they
/// never appear as an "agent" group in `--help`.
#[derive(Subcommand)]
pub enum AgentCommand {
    /// Send a peer message to an agent (fire-and-forget; processed
    /// asynchronously).
    Message(MessageArgs),
    /// Show an agent's context token usage and recent retry history.
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
    /// List the rooms this tagma has joined (so you can address them with
    /// `send --room <room>`).
    Rooms,
    /// Read a room's history (`--room` required).
    Read(ReadRoomArgs),
}

/// Text payload for `kallip lesche send`. The text is a positional argument;
/// when omitted, the entire stdin is read (multiline — pipe, heredoc, or
/// `< file` all work).
#[derive(Args)]
pub struct SendArgs {
    /// The message text. If omitted, reads the full text from stdin (multiline).
    #[arg(allow_hyphen_values = true)]
    pub text: Option<String>,
    /// The room id to send into. Omit for the bilateral 1:1
    /// conversation; pass the room id (copied verbatim from the inbound
    /// `[From: ... | room <id>]` header) to reply in a multi-member room.
    #[arg(long, allow_hyphen_values = true)]
    pub room: Option<String>,
}

/// Args for `kallip lesche read` (pull a room's history).
#[derive(Args)]
pub struct ReadRoomArgs {
    /// The room id to read from (one of the ids listed by `kallip lesche rooms`).
    #[arg(long, allow_hyphen_values = true)]
    pub room: String,
    /// Return only messages with `seq > after_seq` (exclusive). Default: from
    /// the start.
    #[arg(long)]
    pub after_seq: Option<i64>,
    /// Max messages to return (server-clamped).
    #[arg(long)]
    pub limit: Option<u64>,
}

// ---------------------------------------------------------------------------
// Inbox commands — self-scoped via KALLIP_ID
// ---------------------------------------------------------------------------

/// Manage this agent's message inbox. The acting agent is taken from
/// `KALLIP_ID` (self-only).
#[derive(Subcommand)]
pub enum InboxCommand {
    /// List messages in the inbox (newest first).
    List(InboxListArgs),
    /// Read a single message by id (marks it as read).
    Read(InboxReadArgs),
    /// Show inbox summary counts (total, unread).
    Summary(InboxSummaryArgs),
    /// Mark a message as done.
    Done(InboxReadArgs),
    /// Clear messages: done-only by default, all with --all.
    Clear(InboxClearArgs),
}

#[derive(Args)]
pub struct InboxListArgs {
    /// Agent ID (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
    /// Filter by status: unread, read, done.
    #[arg(long)]
    pub status: Option<String>,
    /// Max messages to return (default 50, max 200).
    #[arg(long)]
    pub limit: Option<u32>,
}

#[derive(Args)]
pub struct InboxReadArgs {
    /// Agent ID (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
    /// Message ID (positional).
    pub msg_id: i64,
}

#[derive(Args)]
pub struct InboxSummaryArgs {
    /// Agent ID (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
}

#[derive(Args)]
pub struct InboxClearArgs {
    /// Agent ID (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
    /// Clear all messages, not just done ones.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args)]
pub struct SpawnArgs {
    /// Working directory for the agent (required).
    #[arg(long)]
    pub workspace_root: String,
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
    /// Transfer the supervisor's entire workspace to this subagent for its
    /// lifetime (the supervisor cannot write its workspace until the child is
    /// removed). Exclusive: the supervisor may have no other subagent while a
    /// full-handoff child exists.
    #[arg(long)]
    pub full_handoff: bool,
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
    /// List approvals; default shows committed ones awaiting a decision.
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
    /// Reads the directory at `path` directly and prints a markdown bullet
    /// index of its entries: each `.md` skill (from its frontmatter) and each
    /// subdirectory (from its `README.md` frontmatter), with each category's
    /// children inlined one level deep. The agent passes the `skills path`
    /// from its identity facts, then pins this output.
    Index(SkillIndexArgs),
    /// Show metadata for a specific skill
    Meta(SkillMetaArgs),
}

#[derive(Args)]
pub struct SkillIndexArgs {
    /// Absolute path of the skill directory to index.
    pub path: PathBuf,
    /// Number of levels to render (default 2). `1` gives a flat one-level
    /// view; raise it for a small subtree to fetch more in one batch. Clamped
    /// to `[1, MAX_INDEX_DEPTH]` by the renderer.
    #[arg(long, default_value_t = 2)]
    pub depth: u32,
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
    /// Increase the tagma-wide token budget by an amount.
    Increase(BudgetAmountArgs),
    /// Decrease the tagma-wide token budget by an amount.
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
