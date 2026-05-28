use clap::Parser;

/// CLI arguments for just-agent-daemon.
#[derive(Parser)]
#[command(
    name = "just-agent-daemon",
    about = "HTTP API server hosting multiple agent instances"
)]
pub struct Args {
    /// Address to listen on.
    #[arg(long, env = "JUST_AGENT_DAEMON_ADDR", default_value = "127.0.0.1:3000")]
    pub listen_addr: String,
    /// URL that agents use to reach this daemon (injected into PTY env).
    #[arg(
        long,
        env = "JUST_AGENT_ADVERTISE_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    pub advertise_url: String,
}
