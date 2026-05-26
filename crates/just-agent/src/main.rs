//! just-agent: daemon client CLI.

mod args;

use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use just_agent_client::DaemonClient;

use args::{Cli, Commands};

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Start(args) => {
            let client = DaemonClient::new(&args.daemon.daemon_url);
            let id = client
                .spawn(args.workspace_root, args.skills, args.prompt)
                .await?;
            println!("{id}");
        }
        Commands::Send(args) => {
            init_tracing();
            let client = DaemonClient::new(&args.daemon.daemon_url);
            let content = client
                .send_prompt(&args.id, &args.prompt, args.timeout)
                .await?;
            println!("{content}");
        }
        Commands::List(args) => {
            let client = DaemonClient::new(&args.daemon_url);
            let agents = client.list_agents().await?;
            if agents.is_empty() {
                println!("No agents running.");
            } else {
                for a in &agents {
                    println!("{}  {}  ws={}", a.id, a.state, a.workspace_root);
                }
            }
        }
        Commands::Stop(args) => {
            let client = DaemonClient::new(&args.daemon.daemon_url);
            client.kill_agent(&args.id).await?;
            println!("Agent {} stopped.", args.id);
        }
        Commands::Events(args) => {
            let client = DaemonClient::new(&args.daemon.daemon_url);
            let mut stream = client.event_stream(&args.id).await?;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(event) => println!("{}", serde_json::to_string(&event)?),
                    Err(e) => eprintln!("SSE error: {e}"),
                }
            }
        }
        Commands::Status(args) => {
            let client = DaemonClient::new(&args.daemon.daemon_url);
            let status = client.agent_status(&args.id).await?;
            println!("state: {}", status.state);
            println!("{}", status.context.format_summary());
            if !status.recent_retries.is_empty() {
                println!(
                    "retries: {} (last: {})",
                    status.recent_retries.len(),
                    status
                        .recent_retries
                        .first()
                        .map(|r| r.error.as_str())
                        .unwrap_or("n/a")
                );
                for r in &status.recent_retries {
                    println!(
                        "  [{}/{}] {} — waited {:.1}s  (round {})",
                        r.attempt, r.max_attempts, r.error, r.delay_secs, r.round,
                    );
                }
            }
        }
        Commands::Interrupt(args) => {
            let client = DaemonClient::new(&args.daemon.daemon_url);
            client.interrupt_agent(&args.id).await?;
            println!("Agent {} interrupted.", args.id);
        }
        Commands::Approve(args) => {
            init_tracing();
            let client = DaemonClient::new(&args.daemon.daemon_url);
            client
                .respond_approval(&args.id, &args.request_id, &args.decision, None)
                .await?;
            println!("Approval sent.");
        }
    }
    Ok(())
}
