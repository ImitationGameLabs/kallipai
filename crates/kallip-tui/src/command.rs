//! TUI-local slash command parsing, completion, and help.

use kallip_common::command::{BudgetOp, SlashCommand};
use kallip_common::tokens::parse_token_amount;

/// Static descriptor for a known command.
pub struct CommandInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub has_arg: bool,
}

const COMMANDS: &[CommandInfo] = &[
    CommandInfo {
        name: "/help",
        description: "Show available commands",
        has_arg: false,
    },
    CommandInfo {
        name: "/quit",
        description: "Exit the TUI",
        has_arg: false,
    },
    CommandInfo {
        name: "/clear",
        description: "Clear chat output",
        has_arg: false,
    },
    CommandInfo {
        name: "/status",
        description: "Show context token usage",
        has_arg: false,
    },
    CommandInfo {
        name: "/approvals",
        description: "View and manage approval requests",
        has_arg: false,
    },
    CommandInfo {
        name: "/budget",
        description: "Show or adjust token budget (+N/-N/=N, =N sets remaining)",
        has_arg: true,
    },
];

/// Returns the full command registry.
pub fn commands() -> &'static [CommandInfo] {
    COMMANDS
}

/// Try to parse user input as a slash command.
///
/// Returns:
/// - `None` — input is a normal prompt (doesn't start with `/` or contains newlines)
/// - `Some(Ok(cmd))` — successfully parsed command
/// - `Some(Err(msg))` — starts with `/` but invalid (unknown, missing arg)
pub fn parse(input: &str) -> Option<Result<SlashCommand, String>> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') || trimmed.contains('\n') {
        return None;
    }

    // Split into command word and the rest
    let (cmd, rest) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
    let cmd = cmd.to_ascii_lowercase();

    let result = match cmd.as_str() {
        "/help" => SlashCommand::Help,
        "/quit" | "/q" | "/exit" => SlashCommand::Quit,
        "/clear" => SlashCommand::Clear,
        "/status" => SlashCommand::Status,
        "/approvals" => SlashCommand::Approvals,
        "/budget" => return Some(parse_budget(rest)),
        _ => return Some(Err(format!("unknown command: {cmd}"))),
    };

    Some(Ok(result))
}

/// Parse the argument portion of `/budget [args]`.
fn parse_budget(arg: &str) -> Result<SlashCommand, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(SlashCommand::Budget { op: None });
    }

    if let Some(num_str) = arg.strip_prefix('+') {
        let amount = parse_token_amount(num_str)?;
        let delta = i64::try_from(amount)
            .map_err(|_| format!("token amount {amount} exceeds maximum delta"))?;
        Ok(SlashCommand::Budget {
            op: Some(BudgetOp::Adjust(delta)),
        })
    } else if let Some(num_str) = arg.strip_prefix('-') {
        let amount = parse_token_amount(num_str)?;
        let delta = i64::try_from(amount)
            .map_err(|_| format!("token amount {amount} exceeds maximum delta"))?;
        Ok(SlashCommand::Budget {
            op: Some(BudgetOp::Adjust(-delta)),
        })
    } else if let Some(num_str) = arg.strip_prefix('=') {
        let value = parse_token_amount(num_str)?;
        Ok(SlashCommand::Budget {
            op: Some(BudgetOp::Set(value)),
        })
    } else {
        Err("budget requires +, -, or = prefix (e.g. +100M, -50M, =500M)".into())
    }
}

/// Return command descriptors whose names start with `prefix`.
pub fn matching(prefix: &str) -> Vec<&'static CommandInfo> {
    let lower = prefix.to_ascii_lowercase();
    commands()
        .iter()
        .filter(|c| c.name.starts_with(lower.as_str()))
        .collect()
}

/// Build formatted help text listing all commands.
pub fn help_text() -> String {
    let mut out = String::from("Available commands:\n");
    for cmd in commands() {
        let arg_hint = if cmd.has_arg { " <arg>" } else { "" };
        out.push_str(&format!(
            "  {:<12} {}{}\n",
            cmd.name, cmd.description, arg_hint
        ));
    }
    out
}
