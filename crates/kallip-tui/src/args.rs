use clap::Parser;

/// CLI arguments for kallip-tui.
#[derive(Parser)]
#[command(
    name = "kallip-tui",
    about = "Interactive client for kallip (TUI).\n\
    Designed for human use. For scripting, use the `kallip` CLI instead."
)]
pub struct Args {
    /// Daemon URL.
    #[arg(
        long,
        env = "KALLIP_DAEMON_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    pub daemon_url: String,
}
