//! `subagent` command subtree of the `kallip` CLI (clap derive).

use super::agent::IdArgs;
use clap::{Args, Subcommand};
use kallip_common::agentid::AgentId;

// ---------------------------------------------------------------------------
// Subagent commands — manage the current agent's (KALLIP_ID) direct subagents
// ---------------------------------------------------------------------------

/// Args for `kallip subagent spawn`. The optional initial prompt is read from
/// the full stdin (multiline — pipe, heredoc, or `< file`); empty stdin (or
/// whitespace-only) means no initial prompt. Prefer a quoted heredoc
/// `<<'EOF'` so shell expansion cannot corrupt it.
#[derive(Args)]
pub(crate) struct SpawnArgs {
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
pub(crate) struct MetadataArgs {
    /// Agent ID or role.
    pub id: AgentId,
    /// New role. Must be non-empty if provided.
    #[arg(long)]
    pub role: Option<String>,
    /// New description. Use the empty string to clear.
    #[arg(long)]
    pub description: Option<String>,
}

/// Manage the current agent's direct subagents. The acting superior is taken
/// from the `KALLIP_ID` env var, so these commands only make sense inside
/// an agent context.
#[derive(Subcommand)]
pub(crate) enum SubagentCommand {
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
