//! just-agent: daemon client CLI.

mod args;

use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use just_agent_client::DaemonClient;

use args::{Cli, Commands};

fn build_client() -> DaemonClient {
    let url =
        std::env::var("JUST_AGENT_DAEMON_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let token = std::env::var("JUST_AGENT_AUTH_TOKEN")
        .expect("JUST_AGENT_AUTH_TOKEN must be set (export it from daemon startup output)");
    DaemonClient::new_with_token(&url, token)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Start(args) => {
            let client = build_client();
            let id = client
                .spawn(just_agent_core::types::CreateAgentRequest {
                    workspace_root: args.workspace_root,
                    skills: args.skills,
                    prompt: args.prompt,
                    created_by: std::env::var("JUST_AGENT_ID").ok(),
                })
                .await?;
            println!("{id}");
        }
        Commands::Send(args) => {
            let client = build_client();
            client.post_message(&args.id, &args.message).await?;
        }
        Commands::List => {
            let client = build_client();
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
            let client = build_client();
            if let Err(e) = client.kill_agent(&args.id).await {
                let msg = e.to_string();
                if msg.contains("409") || msg.contains("busy") || msg.contains("subagent") {
                    eprintln!(
                        "Cannot stop agent: {}. \
                         Try: just-agent interrupt {}",
                        msg.to_lowercase(),
                        args.id
                    );
                }
                return Err(e);
            }
            println!("Agent {} stopped.", args.id);
        }
        Commands::Events(args) => {
            let client = build_client();
            let mut stream = client.event_stream(&args.id).await?;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(event) => println!("{}", serde_json::to_string(&event)?),
                    Err(e) => eprintln!("SSE error: {e}"),
                }
            }
        }
        Commands::Status(args) => {
            let client = build_client();
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
            let client = build_client();
            client.interrupt_agent(&args.id).await?;
            println!("Agent {} interrupted.", args.id);
        }
        Commands::Approve(args) => {
            let client = build_client();
            client
                .respond_approval(&args.id, &args.request_id, &args.decision, None)
                .await?;
            println!("Approval sent.");
        }
    }
    Ok(())
}
