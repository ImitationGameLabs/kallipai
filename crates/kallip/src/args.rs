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
    /// Deliver messages to the user via the relay (the lesche data-plane).
    #[command(subcommand)]
    Lesche(LescheCommand),
    /// List, read, summarize, or clear this agent's message inbox.
    #[command(subcommand)]
    Inbox(InboxCommand),
    /// Read-only team directory: who exists and in what state. Discovery
    /// only — management actions (spawn/remove/metadata) live under `subagent`.
    #[command(name = "agent", subcommand)]
    Dir(AgentDirCommand),
    /// Manage named profile sets: list, rebind agents, transfer the
    /// default marker, or remove a set (bound agents interrupt with
    /// --force).
    #[command(subcommand)]
    ProfileSet(ProfileSetCommand),
    /// Transfer files through the files service (direct HTTP; no tagma
    /// daemon connection). Credentials come from the spawn env:
    /// KALLIP_FILES_URL + KALLIP_FILES_TOKEN.
    #[command(subcommand)]
    File(FileCommand),
    /// Read an image record into this agent's conversation. Self-scoped:
    /// the tagma enforces the bound set's modalities and records the turn.
    /// Connects to the tagma (KALLIP_TAGMA_URL + KALLIP_AUTH_TOKEN), not
    /// the file family's direct files-service connection.
    #[command(subcommand)]
    Image(ImageCommand),
    /// Task ledger on the tagma: queue, state machine, event trail, hard
    /// gates, and closed-task archives (served by the tagma task API).
    #[command(subcommand)]
    Task(TaskCommand),
    /// Declarative team management: converge the fleet to a declaration,
    /// inspect the three-way status, and maintain the lock archive.
    #[command(subcommand)]
    Team(TeamCommand),
}

/// The `kallip team` family — declarative team management. The
/// declaration (tagma.toml) is the desired team; the lock (tagma.lock,
/// a CLI-side archive next to it) records role→agent bindings; converge
/// makes reality match the declaration and rewrites the lock.
#[derive(Subcommand)]
pub enum TeamCommand {
    /// Show the three-way comparison for one declaration: declaration vs
    /// lock vs live registry, one row per role, with converge's verdict.
    Status(TeamStatusArgs),
    /// Converge the live fleet to the declaration: plan, preflight
    /// (whole-batch refusal on structural problems), then execute with
    /// fail-fast. Writes the lock on applied/aborted outcomes.
    Converge(TeamConvergeArgs),
    /// Rebuild the lock archive from reality: live members come from the
    /// registry, parked members from the inactive area (their agent dirs
    /// carry the role). Every recovered parked body is listed.
    LockRebuild(TeamLockRebuildArgs),
}

/// Path arguments shared by the team family: the declaration file and
/// the lock archive. The lock defaults to `tagma.lock` next to the
/// declaration (they are one team archive); the declaration defaults to
/// `./tagma.toml`.
#[derive(Args)]
pub struct TeamCommonArgs {
    /// Declaration file (tagma.toml).
    #[arg(long)]
    pub file: Option<String>,
    /// Lock archive (tagma.lock); defaults to the declaration's directory.
    #[arg(long)]
    pub lock: Option<PathBuf>,
    /// Machine face: JSON output, stable field names.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct TeamStatusArgs {
    #[command(flatten)]
    pub common: TeamCommonArgs,
}

#[derive(Args)]
pub struct TeamConvergeArgs {
    #[command(flatten)]
    pub common: TeamCommonArgs,
    /// Plan only: print what would happen, touch nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Wait for busy deactivation targets to go idle, then converge
    /// (poll loop; Ctrl-C aborts the wait, never the fleet).
    #[arg(long)]
    pub drain: bool,
    /// Interrupt busy deactivation targets instead of refusing (recorded
    /// as an auditable escape in the affected result rows).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct TeamLockRebuildArgs {
    /// Directory to write tagma.lock into (defaults to the declaration's
    /// directory, or the current directory with no --file).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Machine face: JSON output.
    #[arg(long)]
    pub json: bool,
}

/// The `kallip profile-set` family — runtime profile-set management.
#[derive(Subcommand)]
pub enum ProfileSetCommand {
    /// List the configured profile sets, their profile counts, and the
    /// default marker.
    List,
    /// Rebind an agent to a named set. A live agent swaps its failover
    /// chain on its next wake-up; a parked one resolves it at restore.
    Bind {
        /// Agent id or role (resolved like every agent-addressing command).
        id: String,
        /// Exact set name; unknown names list the available sets.
        set: String,
    },
    /// Transfer the default-set marker to an existing set.
    Default {
        /// Exact set name.
        set: String,
    },
    /// Remove a set. The default set and the root's set are refused;
    /// other referenced sets list their bound agents and need --force
    /// (they are interrupted, then keep a dangling record until rebound).
    Remove {
        /// Exact set name.
        set: String,
        /// Interrupt the bound agents, then remove.
        #[arg(long)]
        force: bool,
    },
}
/// The `kallip agent` command family: a read-only fleet directory.
#[derive(Subcommand)]
pub enum AgentDirCommand {
    /// List every agent on this tagma: role, state, since, id, workspace.
    List,
}
/// Ungrouped per-agent ops, flattened into the top-level command list — they
/// never appear as an "agent" group in `--help`.
#[derive(Subcommand)]
pub enum AgentCommand {
    /// Send a peer message to an agent (fire-and-forget; processed
    /// asynchronously). The message text is read from the full stdin
    /// (multiline); prefer a quoted heredoc `<<'EOF'` so shell expansion
    /// cannot corrupt it. Prints a one-line JSON echo on success.
    Message(MessageArgs),
    /// Show an agent's context token usage and recent retry history.
    Status(StatusArgs),
    /// Report this agent's current activity (self-only)
    Activity(ActivityArgs),
}

/// Deliver messages via the tagma's relay (the lesche data-plane).
/// Targets the calling agent (resolved from `KALLIP_ID`), like `activity`.
///
/// The primitive is "send a message", not "reply": a message may be a
/// response, a proactive heads-up, or (future) a file. Three addressing
/// forms: bare (the bilateral 1:1 with the user), `--room`, `--tagma`.
#[derive(Subcommand)]
pub enum LescheCommand {
    /// Send a text message: to the user (default), a room (`--room`), or a
    /// peer tagma's direct session (`--tagma`).
    Send(SendArgs),
    /// List the rooms this tagma has joined (so you can address them with
    /// `send --room <room>`). See also `sessions` for every addressable
    /// surface in one list.
    Rooms,
    /// Read a conversation's history (`--room` or `--tagma` required).
    Read(ReadArgs),
    /// List every addressable surface in one list: the user's bilateral 1:1,
    /// the joined rooms, and this tagma's direct sessions (each with kind,
    /// id, and peer metadata).
    Sessions,
}

/// Args for `kallip lesche send`. The text is read from the full stdin
/// (multiline — pipe, heredoc, or `< file`); prefer a quoted heredoc
/// `<<'EOF'` so shell expansion cannot corrupt it.
#[derive(Args)]
pub struct SendArgs {
    /// The room id to send into. Omit for the bilateral 1:1
    /// conversation; pass the room id (copied verbatim from the inbound
    /// `[From: ... | room <id>]` header) to reply in a multi-member room.
    #[arg(long, allow_hyphen_values = true, conflicts_with = "tagma")]
    pub room: Option<String>,
    /// The peer tagma id to send to (copied from the inbound
    /// `[From: ... (<tagma-id>) | direct <session>]` header or from
    /// `sessions`). Sends into that tagma's direct session; the first send
    /// creates it (idempotent to re-send). Note: attachments must live in a
    /// workspace the peer can read — a private-area file record fails the
    /// peer's fetch with 403.
    #[arg(long, allow_hyphen_values = true)]
    pub tagma: Option<String>,
}

/// Args for `kallip lesche read` (pull a conversation's history; exactly
/// one of `--room` / `--tagma`).
#[derive(Args)]
pub struct ReadArgs {
    /// The room id to read from (one of the ids listed by `kallip lesche
    /// rooms`).
    #[arg(
        long,
        allow_hyphen_values = true,
        required_unless_present = "tagma",
        conflicts_with = "tagma"
    )]
    pub room: Option<String>,
    /// The peer tagma id to read (the session id is derived from the pair,
    /// so you never type a session id).
    #[arg(long, allow_hyphen_values = true)]
    pub tagma: Option<String>,
    /// Return only messages with `seq > after_seq` (exclusive). Default: from
    /// the start.
    #[arg(long)]
    pub after_seq: Option<i64>,
    /// Max messages to return (server-clamped).
    #[arg(long)]
    pub limit: Option<u64>,
}

// ---------------------------------------------------------------------------
// File commands — files-service client, credentials from the spawn env
// ---------------------------------------------------------------------------

/// The `kallip file` family: content transfer against the files service.
/// The acting principal is the tagma named by the bearer token (the
/// spawn env); `--space self` is its own region, `shared` is the space's
/// shared region.
#[derive(Subcommand)]
pub enum FileCommand {
    /// Upload a local file to a space path.
    Put(FilePutArgs),
    /// Download a record by id (to --out, or stdout when omitted).
    Get(FileGetArgs),
    /// Deliver a record into another principal's inbox.
    Send(FileSendArgs),
    /// List records in a slice of the caller's space.
    Ls(FileLsArgs),
}

/// Args for `kallip file put`.
#[derive(Args)]
pub struct FilePutArgs {
    /// Space path to store under (e.g. /users/alice/shared/report.pdf).
    pub path: String,
    /// Local file to upload (streamed, never buffered whole).
    #[arg(long = "file", value_name = "FILE")]
    pub file: PathBuf,
    /// Print the response as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Args for `kallip file get`.
#[derive(Args)]
pub struct FileGetArgs {
    /// Record id to download.
    pub id: String,
    /// Write the content here instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

/// Args for `kallip file send`. Exactly one target.
#[derive(Args)]
pub struct FileSendArgs {
    /// Record id to deliver.
    pub id: String,
    /// Deliver into this tagma's inbox (same space required).
    #[arg(
        long,
        value_name = "TAGMA",
        required_unless_present = "to_user",
        group = "file-send-target"
    )]
    pub to_tagma: Option<String>,
    /// Deliver into this user's inbox.
    #[arg(
        long,
        value_name = "USER",
        required_unless_present = "to_tagma",
        group = "file-send-target"
    )]
    pub to_user: Option<String>,
    /// Print the response as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Args for `kallip file ls`.
#[derive(Args)]
pub struct FileLsArgs {
    /// Which slice: `self` (the caller's own region, inbox included) or
    /// `shared` (the space's shared region).
    #[arg(long, value_parser = ["self", "shared"])]
    pub space: String,
    /// Narrow to paths under this relative prefix (e.g. inbox/).
    #[arg(long)]
    pub prefix: Option<String>,
    /// Max entries (the server clamps to its own cap).
    #[arg(long)]
    pub limit: Option<u64>,
    /// Print the listing as JSON.
    #[arg(long)]
    pub json: bool,
}

/// The `kallip image` family: read image records into the conversation.
#[derive(Subcommand)]
pub enum ImageCommand {
    /// Ingest an image record into this agent's live context.
    Read(ImageReadArgs),
}

/// Args for `kallip image read`.
#[derive(Args)]
pub struct ImageReadArgs {
    /// Files-service record id to ingest.
    pub id: String,
    /// Media type of the record (default `image/png`).
    #[arg(long, value_name = "TYPE")]
    pub media_type: Option<String>,
    /// Caption carried alongside the reference.
    #[arg(long, value_name = "TEXT")]
    pub caption: Option<String>,
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
    /// Agent ID or role (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
    /// Filter by status: unread, read, done.
    #[arg(long)]
    pub status: Option<String>,
    /// Max messages to return (default 50, max 200).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Render timestamps as relative distances (8m ago) instead of absolute UTC.
    #[arg(long)]
    pub relative_time: bool,
}

#[derive(Args)]
pub struct InboxReadArgs {
    /// Agent ID or role (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
    /// Message ID (positional).
    pub msg_id: i64,
}

#[derive(Args)]
pub struct InboxSummaryArgs {
    /// Agent ID or role (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
}

#[derive(Args)]
pub struct InboxClearArgs {
    /// Agent ID or role (defaults to KALLIP_ID).
    #[arg(long)]
    pub id: Option<AgentId>,
    /// Clear all messages, not just done ones.
    #[arg(long)]
    pub all: bool,
}

/// Args for `kallip subagent spawn`. The optional initial prompt is read from
/// the full stdin (multiline — pipe, heredoc, or `< file`); empty stdin (or
/// whitespace-only) means no initial prompt. Prefer a quoted heredoc
/// `<<'EOF'` so shell expansion cannot corrupt it.
#[derive(Args)]
pub struct SpawnArgs {
    /// Working directory for the agent (required).
    #[arg(long)]
    pub workspace_root: String,
    /// Activate a skill by name (repeatable).
    #[arg(long = "skill", value_delimiter = ',')]
    pub skills: Vec<String>,
    /// Short display label (e.g. "researcher"). Required by the tagma when
    /// spawning a subordinate (the only spawn path: `subagent spawn`).
    #[arg(long)]
    pub role: Option<String>,
    /// Longer prose: what this agent is for.
    #[arg(long)]
    pub description: Option<String>,
    /// Profile set the subagent resolves against, by exact name (see the
    /// tagma's profile config for the configured sets). Required; the
    /// spawn rejects unknown names.
    #[arg(long, value_name = "SET")]
    pub profile_set: String,
    /// FS-access permission class (`normal` = home+workspace read-write,
    /// `guest` = read-only). Required, explicit, and downgrade-only — the
    /// tagma rejects a value above the supervisor's class.
    #[arg(long, value_name = "CLASS", value_parser = ["normal", "guest"])]
    pub permission_class: String,
    /// Transfer the supervisor's entire workspace to this subagent for its
    /// lifetime (the supervisor cannot write its workspace until the child is
    /// removed). Exclusive: the supervisor may have no other subagent while a
    /// full-handoff child exists.
    #[arg(long)]
    pub full_handoff: bool,
}

#[derive(Args)]
pub struct MetadataArgs {
    /// Agent ID or role.
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

/// Args for `kallip message`. The message text is read from the full stdin
/// (multiline — pipe, heredoc, or `< file`); prefer a quoted heredoc
/// `<<'EOF'` so shell expansion cannot corrupt it.
#[derive(Args)]
pub struct MessageArgs {
    /// Agent ID or role.
    pub id: AgentId,
}

#[derive(Args)]
pub struct IdArgs {
    /// Agent ID or role.
    pub id: AgentId,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Agent ID or role (positional; omit for the fleet overview).
    pub id: Option<AgentId>,
    /// Render timestamps as relative distances (8m ago) instead of absolute UTC.
    #[arg(long)]
    pub relative_time: bool,
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
    /// Filter by owning agent ID or role.
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
    /// Agent ID or role.
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
    /// Switch to an unlimited budget (enforcement off, consumption still tracked)
    Unlimited,
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
// Task commands — the tagma's task ledger, over the task domain API
// ---------------------------------------------------------------------------

/// The tagma's task ledger: queue, coarse state machine, event trail, hard
/// gates, and closed-task archives. Verbs go through the tagma task API
/// (the CLI never touches tasks.sqlite); the acting agent is taken
/// from `KALLIP_ID` (or --actor). Write verbs are enforced server-side:
/// the CLI is an entry point, the tagma is the law.
#[derive(Subcommand)]
pub enum TaskCommand {
    /// Register a task in the queue (--title ...), or pick a queued task up
    /// by id (queued -> in_progress; the serial gate applies, --force to
    /// override with an auditable escape).
    Start(TaskStartArgs),
    /// Record a checkpoint: a work note, a review receipt (--receipt), a
    /// move to review (--review), and/or the waiting timing marker
    /// (--waiting / --no-waiting; a marker, never a state).
    Checkpoint(TaskCheckpointArgs),
    /// Close a task: every review seat registered at dispatch must have
    /// filed a receipt (checkpoint --receipt), or pass --force with an
    /// auditable escape. The dossier (if registered) is packed canonically
    /// and content-addressed on close.
    Close(TaskCloseArgs),
    /// Reopen a closed task (back to in_progress).
    Reopen(TaskReopenArgs),
    /// Append a note to the task's trail (never moves the machine).
    Annotate(TaskAnnotateArgs),
    /// Dispatch the review round: registers the seat roster for the
    /// current review cycle; the close gate counts receipts against it.
    Dispatch(TaskDispatchArgs),
    /// Record a gate report — the announcement that precedes every
    /// recorded chain operation.
    GateReport(TaskGateReportArgs),
    /// Record a chain operation (commit/amend/rebase/reset); requires a
    /// gate report newer than the last recorded chain op.
    ChainOp(TaskChainOpArgs),
    /// Archive a closed task: it leaves the default list view
    /// (`task list --archived` shows archived tasks).
    Archive(TaskArchiveArgs),
    /// List tasks (oldest first).
    List(TaskListArgs),
    /// Show one task: current state, association keys, event trail.
    Show(TaskShowArgs),
    /// Export a task (id, or all tasks with --all). Default renders the
    /// show view; --json emits the machine face (stable field names, ISO
    /// 8601 UTC times) including the full event trail.
    Export(TaskExportArgs),
    /// Extract a closed task's content-addressed archive (the dossier
    /// snapshot frozen at close) into a directory.
    Extract(TaskExtractArgs),
}

#[derive(Args)]
pub struct TaskStartArgs {
    /// Existing task id to pick up (queued -> in_progress). Omit to
    /// register a new task from --title.
    pub id: Option<i64>,
    /// Title for a new task (registers it in the queue).
    #[arg(long)]
    pub title: Option<String>,
    /// Creator for a new task (defaults to KALLIP_ID).
    #[arg(long)]
    pub creator: Option<String>,
    /// Assignee for a new task; defaults to whoever picks it up.
    #[arg(long)]
    pub assignee: Option<String>,
    /// Register a review seat (repeatable). Every seat must file a receipt
    /// before the task can close.
    #[arg(long = "seat")]
    pub seats: Vec<String>,
    /// Dossier directory packed into the closed archive on close.
    #[arg(long)]
    pub dossier: Option<PathBuf>,
    /// Association key: inbox id window start (the task's message trail).
    #[arg(long)]
    pub inbox_start: Option<i64>,
    /// Association key: inbox id window end.
    #[arg(long)]
    pub inbox_end: Option<i64>,
    /// Association key: lesche room id.
    #[arg(long)]
    pub room: Option<String>,
    /// Association key: room seq window start.
    #[arg(long)]
    pub room_seq_start: Option<i64>,
    /// Association key: room seq window end.
    #[arg(long)]
    pub room_seq_end: Option<i64>,
    /// Override the serial gate (the escape is recorded in the event trail).
    #[arg(long)]
    pub force: bool,
    /// Acting agent (defaults to KALLIP_ID).
    #[arg(long)]
    pub actor: Option<String>,
}

#[derive(Args)]
pub struct TaskCheckpointArgs {
    /// Task id.
    pub id: i64,
    /// Work note recorded with the checkpoint.
    #[arg(long)]
    pub note: Option<String>,
    /// File a review receipt as this actor (satisfies the close gate).
    #[arg(long)]
    pub receipt: bool,
    /// Move the task in_progress -> review.
    #[arg(long)]
    pub review: bool,
    /// Set the waiting timing marker.
    #[arg(long)]
    pub waiting: bool,
    /// Clear the waiting timing marker.
    #[arg(long = "no-waiting")]
    pub no_waiting: bool,
    /// Acting agent (defaults to KALLIP_ID).
    #[arg(long)]
    pub actor: Option<String>,
}

#[derive(Args)]
pub struct TaskCloseArgs {
    /// Task id.
    pub id: i64,
    /// Why the task closes (gh-style two-level terminal state).
    #[arg(long, value_enum, default_value_t = TaskCloseReason::Completed)]
    pub reason: TaskCloseReason,
    /// One-sentence result recorded with the close event.
    #[arg(long)]
    pub summary: Option<String>,
    /// Override the receipt gate (the escape is recorded in the event trail).
    #[arg(long)]
    pub force: bool,
    /// Acting agent (defaults to KALLIP_ID).
    #[arg(long)]
    pub actor: Option<String>,
}

/// Terminal-state reason, serialized snake_case on the store face.
#[derive(clap::ValueEnum, Clone, Copy)]
#[value(rename_all = "snake_case")]
pub enum TaskCloseReason {
    Completed,
    NotPlanned,
    Duplicate,
}

/// The recorded git chain operation, serialized snake_case on the store face.
#[derive(clap::ValueEnum, Clone, Copy)]
#[value(rename_all = "snake_case")]
pub enum TaskChainOpType {
    Commit,
    Amend,
    Rebase,
    Reset,
}

#[derive(Args)]
pub struct TaskReopenArgs {
    /// Task id.
    pub id: i64,
    /// Override the serial gate (the escape is recorded in the event trail).
    #[arg(long)]
    pub force: bool,
    /// Acting agent (defaults to KALLIP_ID).
    #[arg(long)]
    pub actor: Option<String>,
}

#[derive(Args)]
pub struct TaskAnnotateArgs {
    /// Task id (any state, closed included).
    pub id: i64,
    /// The note to append.
    #[arg(long)]
    pub note: String,
    /// Acting agent (defaults to KALLIP_ID).
    #[arg(long)]
    pub actor: Option<String>,
}

#[derive(Args)]
pub struct TaskDispatchArgs {
    /// Task id (must be in_progress or review).
    pub id: i64,
    /// Seat roster for this review cycle (comma-separated). Omit to
    /// re-affirm the roster registered at create; pass an empty value
    /// for an explicit zero-seat registration.
    #[arg(long, value_delimiter = ',')]
    pub seats: Option<Vec<String>>,
    /// Acting agent (defaults to KALLIP_ID).
    #[arg(long)]
    pub actor: Option<String>,
}

#[derive(Args)]
pub struct TaskGateReportArgs {
    /// Task id.
    pub id: i64,
    /// One-line report (what was announced, where).
    #[arg(long)]
    pub note: String,
    /// Acting agent (defaults to KALLIP_ID).
    #[arg(long)]
    pub actor: Option<String>,
}

#[derive(Args)]
pub struct TaskChainOpArgs {
    /// Task id.
    pub id: i64,
    /// The chain operation: commit, amend, rebase, or reset.
    #[arg(long)]
    pub op: TaskChainOpType,
    /// Reference or one-line detail (e.g. the resulting hash).
    #[arg(long)]
    pub detail: Option<String>,
    /// Override the gate-report gate (the escape is recorded).
    #[arg(long)]
    pub force: bool,
    /// Acting agent (defaults to KALLIP_ID).
    #[arg(long)]
    pub actor: Option<String>,
}

#[derive(Args)]
pub struct TaskArchiveArgs {
    /// Task id (must be closed).
    pub id: i64,
    /// Override the closed-only gate (the escape is recorded).
    #[arg(long)]
    pub force: bool,
    /// Acting agent (defaults to KALLIP_ID).
    #[arg(long)]
    pub actor: Option<String>,
}

#[derive(Args)]
pub struct TaskListArgs {
    /// Filter by status: queued, in_progress, review, closed.
    #[arg(long)]
    pub status: Option<String>,
    /// Filter by assignee.
    #[arg(long)]
    pub assignee: Option<String>,
    /// List archived tasks only (the default view is the active one).
    #[arg(long)]
    pub archived: bool,
}

#[derive(Args)]
pub struct TaskShowArgs {
    /// Task id.
    pub id: i64,
}

#[derive(Args)]
pub struct TaskExportArgs {
    /// Task id; omit with --all.
    pub id: Option<i64>,
    /// Export every task.
    #[arg(long)]
    pub all: bool,
    /// Emit the machine face (JSON) instead of the show view.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct TaskExtractArgs {
    /// Task id.
    pub id: i64,
    /// Destination directory (created if absent).
    #[arg(long)]
    pub to: PathBuf,
}
