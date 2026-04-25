use clap::Parser;

/// CLI arguments for just-agent-daemon.
#[derive(Parser)]
#[command(name = "just-agent-daemon", about = "HTTP API server hosting multiple agent instances")]
pub struct Args {
    /// Address to listen on.
    #[arg(long, env = "JUST_AGENT_DAEMON_ADDR", default_value = "0.0.0.0:3000")]
    pub listen_addr: String,
}
