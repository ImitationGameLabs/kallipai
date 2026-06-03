//! Slash command definitions and user input types.
//!
//! Shared between the daemon (produces `UserInput`), runtime (consumes it),
//! and the TUI (parses and dispatches commands).

/// Input from the TUI, sent through the prompt channel.
pub enum UserInput {
    /// A normal chat message to send to the LLM.
    Prompt(String),
    /// A slash command to execute.
    Command(SlashCommand),
}

/// A parsed slash command.
#[derive(Debug)]
pub enum SlashCommand {
    Help,
    Quit,
    Clear,
    Status,
    Approvals,
}
