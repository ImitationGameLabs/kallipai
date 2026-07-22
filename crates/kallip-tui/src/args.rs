use clap::Parser;

/// CLI arguments for kallip-tui.
#[derive(Parser)]
#[command(
    name = "kallip-tui",
    about = "Interactive client for kallip (TUI).\n\
    Designed for human use. For scripting, use the `kallip` CLI instead."
)]
pub struct Args {
    /// Tagma URL.
    #[arg(
        long,
        env = "KALLIP_TAGMA_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    pub tagma_url: String,
}
