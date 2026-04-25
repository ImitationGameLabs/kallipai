mod args;
mod bridge;
mod routes;
mod sse;
mod state;

use anyhow::Result;
use clap::Parser;
use state::AppState;
use std::sync::Arc;
use tracing::info;

use args::Args;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Expose daemon URL so agent shells can discover it via $JUST_AGENT_DAEMON_URL.
    // Safe: called once at startup, single-threaded, before any concurrent operations.
    unsafe {
        std::env::set_var(
            "JUST_AGENT_DAEMON_URL",
            format!("http://{}", args.listen_addr),
        );
    }

    let state = Arc::new(AppState::new());

    let app = routes::router().with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;
    info!(addr = %args.listen_addr, "daemon listening");
    axum::serve(listener, app).await?;
    Ok(())
}
