use clap::{Parser, Subcommand};

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
    /// Manage named profile sets: list them or rebind agents to a set.
    #[command(subcommand)]
    ProfileSet(ProfileSetCommand),
    /// Transfer files through the files service (direct HTTP; no tagma
    /// daemon connection). Credentials come from the spawn env:
    /// KALLIP_POLIS_URL (edge origin; /v1/files is derived) + KALLIP_FILES_TOKEN.
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

pub(crate) mod agent;
pub(crate) mod approval;
pub(crate) mod budget;
pub(crate) mod file;
pub(crate) mod image;
pub(crate) mod inbox;
pub(crate) mod lesche;
pub(crate) mod policy;
pub(crate) mod profile;
pub(crate) mod skill;
pub(crate) mod subagent;
pub(crate) mod task;
pub(crate) mod team;
use self::{
    agent::{AgentCommand, AgentDirCommand},
    approval::ApprovalCommand,
    budget::BudgetCommand,
    file::FileCommand,
    image::ImageCommand,
    inbox::InboxCommand,
    lesche::LescheCommand,
    policy::PolicyCommand,
    profile::ProfileSetCommand,
    skill::SkillCommand,
    subagent::SubagentCommand,
    task::TaskCommand,
    team::TeamCommand,
};
