use clap::Parser;

/// CLI arguments for just-agent-tui.
#[derive(Parser)]
#[command(name = "just-agent-tui", about = "Interactive TUI client for just-agent")]
pub struct Args {
    /// Daemon URL.
    #[arg(long, env = "JUST_AGENT_DAEMON_URL", default_value = "http://localhost:3000")]
    pub daemon_url: String,

    /// Activate a skill by name (repeatable).
    #[arg(long = "skill", env = "JUST_AGENT_SKILLS", value_delimiter = ',')]
    pub skills: Vec<String>,

    /// Use stdin/stdout instead of the TUI.
    #[arg(long)]
    pub stdio: bool,
}
