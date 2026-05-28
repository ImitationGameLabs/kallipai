use clap::Parser;

/// CLI arguments for just-agent-tui.
#[derive(Parser)]
#[command(
    name = "just-agent-tui",
    about = "Interactive client for just-agent (TUI or stdio).\n\
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

    /// Use stdin/stdout instead of the TUI. Still interactive, not for scripting.
    /// Intended as a fallback when the TUI is broken or unusable.
    #[arg(long)]
    pub stdio: bool,
}
