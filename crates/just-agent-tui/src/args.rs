use clap::Parser;

/// CLI arguments for just-agent-tui.
#[derive(Parser)]
#[command(
    name = "just-agent-tui",
    about = "Interactive client for just-agent (TUI).\n\
    Designed for human use. For scripting, use the `just-agent` CLI instead."
)]
pub struct Args {
    /// Daemon URL.
    #[arg(
        long,
        env = "JUST_AGENT_DAEMON_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    pub daemon_url: String,
}
