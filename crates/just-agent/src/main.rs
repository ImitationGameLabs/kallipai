//! just-agent: daemon client CLI.

mod args;

use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use just_agent_client::{DaemonClient, PromoteDecision};
use just_agent_common::agentid::AgentId;
use just_agent_common::policy::PolicyDecision;
use just_agent_common::promote::{NO_REASON_PROVIDED, SkillPromoteStatus};
use just_agent_common::tokens::parse_token_amount;

/// Returns the deny reason string, or the placeholder when absent.
/// Only meaningful when `status == Denied`.
fn deny_reason_display(reason: &Option<String>) -> &str {
    reason.as_deref().unwrap_or(NO_REASON_PROVIDED)
}

use args::{
    AgentCommand, ApprovalCommand, BudgetCommand, Cli, Commands, PolicyCommand,
    PromoteRequestCommand, SkillCommand,
};

/// Read agent ID from JUST_AGENT_ID env var.
fn agent_id_from_env() -> anyhow::Result<AgentId> {
    std::env::var("JUST_AGENT_ID")
        .map_err(|_| anyhow::anyhow!("JUST_AGENT_ID env var not set"))
        .and_then(|s| s.parse::<AgentId>().map_err(Into::into))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = DaemonClient::from_env()?;

    match cli.command {
        Commands::Agent(cmd) => match cmd {
            AgentCommand::Spawn(args) => {
                let id = client
                    .spawn(just_agent_common::protocol::CreateAgentRequest {
                        workspace_root: args.workspace_root,
                        skills: args.skills,
                        prompt: args.prompt,
                        created_by: std::env::var("JUST_AGENT_ID").ok().map(AgentId::from),
                        role: args.role.unwrap_or_default(),
                        description: args.description.unwrap_or_default(),
                        max_tool_rounds: None,
                    })
                    .await?;
                println!("{id}");
            }
            AgentCommand::Send(args) => {
                client.post_message(&args.id, &args.message).await?;
            }
            AgentCommand::List(args) => {
                let agents = client.list_agents(args.created_by.as_ref()).await?;
                if agents.is_empty() {
                    println!("No agents running.");
                } else {
                    for a in &agents {
                        // Fall back to the id when no role is set so every row is identifiable.
                        let label = if a.role.is_empty() {
                            a.id.to_string()
                        } else {
                            a.role.clone()
                        };
                        let mut line = format!("{}  {}  ws={}", label, a.state, a.workspace_root);
                        if !a.description.is_empty() {
                            line.push_str("  ");
                            line.push_str(&a.description);
                        }
                        if !a.activity.is_empty() {
                            line.push_str("  [");
                            line.push_str(&a.activity);
                            line.push(']');
                        }
                        println!("{line}");
                    }
                }
            }
            AgentCommand::Remove(args) => {
                if let Err(e) = client.remove_agent(&args.id).await {
                    let msg = e.to_string();
                    if msg.contains("409") || msg.contains("busy") || msg.contains("subagent") {
                        eprintln!(
                            "Cannot remove agent: {}. \
                             Try: just-agent interrupt {}",
                            msg.to_lowercase(),
                            args.id
                        );
                    }
                    return Err(e);
                }
                println!("Agent {} archived.", args.id);
            }
            AgentCommand::Events(args) => {
                let mut stream = client.event_stream(&args.id).await?;
                while let Some(result) = stream.next().await {
                    match result {
                        Ok(event) => println!("{}", serde_json::to_string(&event)?),
                        Err(e) => eprintln!("SSE error: {e}"),
                    }
                }
            }
            AgentCommand::Status(args) => {
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
            AgentCommand::Permissions(args) => {
                let perms = client.agent_permissions(&args.id).await?;
                println!("max_depth: {}", perms.max_depth);
                println!("workspace_root: {}", perms.workspace_root);
                if let Some(sup) = &perms.created_by {
                    println!("created_by: {sup}");
                }
                println!();
                println!("default: {}", perms.tool_policy.default);
                println!();
                println!("tool policy:");
                for (tool, decision) in &perms.tool_policy.tools {
                    println!("  {tool}: {decision}");
                }
            }
            AgentCommand::Interrupt(args) => {
                client.interrupt_agent(&args.id).await?;
                println!("Agent {} interrupted.", args.id);
            }
            AgentCommand::Metadata(args) => {
                let updated = client
                    .update_agent_metadata(
                        &args.id,
                        just_agent_common::protocol::UpdateAgentMetadataRequest {
                            role: args.role,
                            description: args.description,
                        },
                    )
                    .await?;
                println!(
                    "{}  role={}  description={}",
                    updated.id,
                    if updated.role.is_empty() {
                        "(unset)"
                    } else {
                        &updated.role
                    },
                    if updated.description.is_empty() {
                        "(unset)"
                    } else {
                        &updated.description
                    },
                );
            }
            AgentCommand::Activity(args) => {
                // Activity is self-reported: the target is always the calling
                // agent (JUST_AGENT_ID), like `spawn` reads `created_by` from env.
                let id = agent_id_from_env()?;
                client
                    .update_activity(
                        &id,
                        just_agent_common::protocol::UpdateActivityRequest {
                            activity: args.activity,
                        },
                    )
                    .await?;
            }
        },
        Commands::Approval(cmd) => match cmd {
            ApprovalCommand::List(args) => {
                let status = if args.all {
                    None
                } else {
                    args.status.clone().or(Some("committed".into()))
                };
                let order = if args.reverse { "asc" } else { "desc" };
                let resp = client
                    .list_approvals(&just_agent_client::ListApprovalsParams {
                        offset: args.offset,
                        limit: args.limit,
                        requested_by: args.requested_by.clone().map(AgentId::from),
                        status,
                        order: Some(order.to_owned()),
                    })
                    .await?;
                if resp.items.is_empty() {
                    println!("No pending approvals.");
                } else {
                    for a in &resp.items {
                        print_approval_entry(a);
                        println!("---");
                    }
                    println!("(total: {})", resp.total);
                }
            }
            ApprovalCommand::Get(args) => {
                let a = client.get_approval(&args.id).await?;
                print_approval_entry(&a);
            }
            ApprovalCommand::Approve(args) => {
                client.respond_approval(&args.id, "approve", None).await?;
                println!("Approved.");
            }
            ApprovalCommand::Deny(args) => {
                client
                    .respond_approval(&args.id, "deny", Some(&args.reason))
                    .await?;
                println!("Denied.");
            }
        },
        Commands::Policy(cmd) => match cmd {
            PolicyCommand::Get(args) => {
                let policy = client.get_policy(&args.id).await?;
                println!("default: {}", policy.default);
                println!();
                for (tool, decision) in &policy.tools {
                    println!("{tool}: {decision}");
                }
            }
            PolicyCommand::Set(args) => {
                let decision: PolicyDecision = args
                    .decision
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid decision: {e}"))?;
                let mut policy = client.get_policy(&args.id).await?;
                policy.tools.insert(args.tool.clone(), decision);
                client.update_policy(&args.id, &policy).await?;
                println!("Updated {} = {}.", args.tool, decision);
            }
        },
        Commands::Skill(cmd) => match cmd {
            SkillCommand::Paths(_) => {
                let id = agent_id_from_env()?;
                let paths = client.skill_paths(&id).await?;
                println!("shared: {}", paths.shared);
                if let Some(local) = &paths.local {
                    println!("local:  {local}");
                }
            }
            SkillCommand::Meta(args) => {
                let id = agent_id_from_env()?;
                let meta = client.skill_meta(&id, &args.name).await?;
                println!("name: {}", meta.name);
                if let Some(desc) = &meta.description {
                    println!("description: {desc}");
                }
            }
        },
        Commands::PromoteRequest(cmd) => match cmd {
            PromoteRequestCommand::Submit(args) => {
                let id = agent_id_from_env()?;
                let resp = client.submit_promote_request(&id, &args.name).await?;
                println!("Skill: {}", resp.skill_name);
                println!("Request ID: {}", resp.request_id);
                println!("Status: {}", resp.status);
                if resp.has_existing {
                    println!("Existing: shared skill will be overwritten on approval");
                } else {
                    println!("Existing: (new skill)");
                }
            }
            PromoteRequestCommand::List { status } => {
                let resp = client.list_promote_requests(status.as_deref()).await?;
                if resp.items.is_empty() {
                    println!("No promote requests.");
                } else {
                    for r in &resp.items {
                        println!(
                            "{}  {}  {}  by:{}  {}",
                            r.id, r.skill_name, r.status, r.requested_by, r.created_at
                        );
                        if let Some(desc) = &r.description {
                            println!("  description: {desc}");
                        }
                        if r.has_existing {
                            println!("  has_existing: true");
                        }
                        if r.status == SkillPromoteStatus::Denied {
                            println!("  deny_reason: {}", deny_reason_display(&r.deny_reason));
                        }
                    }
                    println!("(total: {})", resp.total);
                }
            }
            PromoteRequestCommand::Show { id } => {
                let resp = client.show_promote_request(&id).await?;
                println!("id: {}", resp.id);
                println!("skill: {}", resp.skill_name);
                println!("status: {}", resp.status);
                println!("requested_by: {}", resp.requested_by);
                println!("has_existing: {}", resp.has_existing);
                if let Some(desc) = &resp.description {
                    println!("description: {desc}");
                }
                if resp.status == SkillPromoteStatus::Denied {
                    println!("deny_reason: {}", deny_reason_display(&resp.deny_reason));
                }
                if let Some(old) = &resp.old_content {
                    println!("\n--- old content ---");
                    println!("{old}");
                } else {
                    println!("\n--- old content: (none, new skill) ---");
                }
                println!("\n--- new content ---");
                println!("{}", resp.new_content);
            }
            PromoteRequestCommand::Approve { id } => {
                client
                    .respond_promote_request(&id, PromoteDecision::Approve, None)
                    .await?;
                println!("Approved.");
            }
            PromoteRequestCommand::Deny { id, reason } => {
                client
                    .respond_promote_request(&id, PromoteDecision::Deny, reason.as_deref())
                    .await?;
                println!("Denied.");
            }
        },
        Commands::Budget(cmd) => match cmd {
            BudgetCommand::Get => {
                let resp = client.get_token_budget().await?;
                println!("{}", resp.format_display());
            }
            BudgetCommand::Increase(args) => {
                let amount = parse_token_amount(&args.amount).map_err(|e| anyhow::anyhow!(e))?;
                let delta = i64::try_from(amount)
                    .map_err(|_| anyhow::anyhow!("token amount {amount} exceeds maximum delta"))?;
                let resp = client.adjust_token_budget(delta).await?;
                println!("Budget increased. {}", resp.format_display());
            }
            BudgetCommand::Decrease(args) => {
                let amount = parse_token_amount(&args.amount).map_err(|e| anyhow::anyhow!(e))?;
                let delta = i64::try_from(amount)
                    .map_err(|_| anyhow::anyhow!("token amount {amount} exceeds maximum delta"))?;
                let resp = client.adjust_token_budget(-delta).await?;
                println!("Budget decreased. {}", resp.format_display());
            }
            BudgetCommand::Set(args) => {
                let value = parse_token_amount(&args.amount).map_err(|e| anyhow::anyhow!(e))?;
                let resp = client.set_token_budget(value).await?;
                println!("Budget set. {}", resp.format_display());
            }
        },
    }
    Ok(())
}

fn print_approval_entry(a: &just_agent_common::protocol::ApprovalEntry) {
    println!("id: {}", a.id);
    println!("status: {}", a.status);
    println!("requested_by: {}", a.requested_by);
    println!("tool: {}", a.content.tool_name);
    println!("arguments: {}", a.content.arguments);
    if let Some(r) = &a.commit_reason {
        println!("commit_reason: {r}");
    }
    if let Some(r) = &a.deny_reason {
        println!("deny_reason: {r}");
    }
    println!("created_at: {}", a.created_at);
}
