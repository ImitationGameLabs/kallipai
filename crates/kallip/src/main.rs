//! kallip: daemon client CLI.

mod args;

use anyhow::Result;
use clap::Parser;
use kallip_client::{DaemonClient, PromoteDecision};
use kallip_common::agentid::AgentId;
use kallip_common::policy::{ExecDecision, ExecOverride};
use kallip_common::promote::{NO_REASON_PROVIDED, SkillPromoteStatus};
use kallip_common::tokens::parse_token_amount;

/// Returns the deny reason string, or the placeholder when absent.
/// Only meaningful when `status == Denied`.
fn deny_reason_display(reason: &Option<String>) -> &str {
    reason.as_deref().unwrap_or(NO_REASON_PROVIDED)
}

use args::{
    AgentCommand, ApprovalCommand, BudgetCommand, Cli, Commands, DirlockCommand, PolicyCommand,
    SkillCommand, SkillPromoteCommand, SubagentCommand,
};

/// Read agent ID from KALLIP_ID env var.
fn agent_id_from_env() -> anyhow::Result<AgentId> {
    std::env::var("KALLIP_ID")
        .map_err(|_| anyhow::anyhow!("KALLIP_ID env var not set"))
        .and_then(|s| s.parse::<AgentId>().map_err(Into::into))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = DaemonClient::from_env()?;

    match cli.command {
        Commands::Agent(cmd) => match cmd {
            AgentCommand::Message(args) => {
                client.post_message(&args.id, &args.message).await?;
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
            AgentCommand::Activity(args) => {
                // Activity is self-reported: the target is always the calling
                // agent (KALLIP_ID); the daemon only accepts this from the
                // agent itself or an operator.
                let id = agent_id_from_env()?;
                client
                    .update_activity(
                        &id,
                        kallip_common::protocol::UpdateActivityRequest {
                            activity: args.activity,
                        },
                    )
                    .await?;
            }
        },
        Commands::Subagent(cmd) => {
            let current = agent_id_from_env()?;
            match cmd {
                SubagentCommand::Spawn(args) => {
                    let id = client
                        .spawn(kallip_common::protocol::CreateAgentRequest {
                            workspace_root: args.workspace_root,
                            skills: args.skills,
                            prompt: args.prompt,
                            created_by: Some(current),
                            role: args.role.unwrap_or_default(),
                            description: args.description.unwrap_or_default(),
                            max_tool_rounds: None,
                            permission_class: args.permission_class,
                        })
                        .await?;
                    println!("{id}");
                }
                SubagentCommand::List => {
                    let agents = client.list_agents(Some(&current)).await?;
                    print_agent_list(&agents, "No direct subagents.");
                }
                SubagentCommand::Remove(args) => {
                    annotate_remove_error(client.remove_agent(&args.id).await, &args.id)?;
                    println!("Agent {} archived.", args.id);
                }
                SubagentCommand::Interrupt(args) => {
                    client.interrupt_agent(&args.id).await?;
                    println!("Agent {} interrupted.", args.id);
                }
                SubagentCommand::Metadata(args) => {
                    let updated = client
                        .update_agent_metadata(
                            &args.id,
                            kallip_common::protocol::UpdateAgentMetadataRequest {
                                role: args.role,
                                description: args.description,
                            },
                        )
                        .await?;
                    print_agent_summary(&updated);
                }
            }
        }
        Commands::Dirlock(cmd) => match cmd {
            DirlockCommand::Acquire(args) => {
                let id = agent_id_from_env()?;
                let resp = client
                    .dirlock_acquire(&id, &args.path, args.timeout_secs)
                    .await?;
                if resp.already_held {
                    println!("Already held.");
                } else {
                    println!("Acquired.");
                }
            }
            DirlockCommand::Release(args) => {
                let id = agent_id_from_env()?;
                client.dirlock_release(&id, &args.path).await?;
                println!("Released.");
            }
            DirlockCommand::Status => {
                let id = agent_id_from_env()?;
                let paths = client.dirlock_status(&id).await?;
                if paths.is_empty() {
                    println!("(no locks held)");
                } else {
                    for p in paths {
                        println!("{p}");
                    }
                }
            }
            DirlockCommand::Who(args) => match client.dirlock_who(&args.dir).await? {
                Some(holder) => println!("held by {holder}"),
                None => println!("unlocked"),
            },
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
                    .list_approvals(&kallip_client::ListApprovalsParams {
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
            PolicyCommand::Show(args) => {
                let perms = client.agent_permissions(&args.id).await?;
                println!("max_depth: {}", perms.max_depth);
                println!("workspace_root: {}", perms.workspace_root);
                if let Some(sup) = &perms.created_by {
                    println!("created_by: {sup}");
                }
                println!("permission_class: {}", perms.permission_class);
                println!("preset: {}", perms.preset);
            }
            PolicyCommand::ExecGet(args) => {
                let policy = client.get_exec_policy(&args.id).await?;
                if policy.overrides.is_empty() {
                    println!("(no per-command overrides; static catalog applies)");
                } else {
                    for (command, entry) in &policy.overrides {
                        match &entry.reason {
                            Some(reason) => {
                                println!("{command}: {} ({reason})", entry.decision);
                            }
                            None => println!("{command}: {}", entry.decision),
                        }
                    }
                }
            }
            PolicyCommand::ExecSet(args) => {
                let decision: ExecDecision = args
                    .decision
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid decision: {e}"))?;
                let entry = match args.reason {
                    Some(reason) => ExecOverride::new(decision).with_reason(reason),
                    None => ExecOverride::new(decision),
                };
                let mut policy = client.get_exec_policy(&args.id).await?;
                policy
                    .overrides
                    .insert(args.command.to_ascii_lowercase(), entry);
                client.update_exec_policy(&args.id, &policy).await?;
                println!("Updated {} = {}.", args.command, decision);
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
            SkillCommand::Promote(cmd) => match cmd {
                SkillPromoteCommand::Submit(args) => {
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
                SkillPromoteCommand::List { status } => {
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
                SkillPromoteCommand::Show { id } => {
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
                SkillPromoteCommand::Approve { id } => {
                    client
                        .respond_promote_request(&id, PromoteDecision::Approve, None)
                        .await?;
                    println!("Approved.");
                }
                SkillPromoteCommand::Deny { id, reason } => {
                    client
                        .respond_promote_request(&id, PromoteDecision::Deny, reason.as_deref())
                        .await?;
                    println!("Denied.");
                }
            },
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

fn print_approval_entry(a: &kallip_common::protocol::ApprovalEntry) {
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

/// Display label for an agent: its role, falling back to the id when no role
/// is set so every row is identifiable.
fn agent_label(a: &kallip_common::protocol::AgentSummary) -> String {
    if a.role.is_empty() {
        a.id.to_string()
    } else {
        a.role.clone()
    }
}

/// Print a list of agents one row per line, or `empty_msg` when there are none.
fn print_agent_list(agents: &[kallip_common::protocol::AgentSummary], empty_msg: &str) {
    if agents.is_empty() {
        println!("{empty_msg}");
        return;
    }
    for a in agents {
        let mut line = format!("{}  {}  ws={}", agent_label(a), a.state, a.workspace_root);
        if !a.description.is_empty() {
            line.push_str("  ");
            line.push_str(&a.description);
        }
        if !a.activity.is_empty() {
            line.push_str("  [");
            line.push_str(&a.activity);
            line.push(']');
        }
        if let Some(reason) = &a.faulted_reason {
            // Surface why a faulted agent could not be brought up, so an
            // operator can decide between fixing the workspace and removing it.
            line.push_str("  faulted: ");
            line.push_str(reason);
        }
        println!("{line}");
    }
}

/// Print an agent's role/description summary (e.g. after a metadata update).
fn print_agent_summary(updated: &kallip_common::protocol::AgentSummary) {
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

/// Propagate a `remove_agent` result, printing a remediation hint to stderr
/// first if the failure looks like the agent is busy or has subagents.
fn annotate_remove_error(result: anyhow::Result<()>, id: &AgentId) -> anyhow::Result<()> {
    if let Err(err) = &result {
        let msg = err.to_string();
        if msg.contains("409") || msg.contains("busy") || msg.contains("subagent") {
            eprintln!(
                "Cannot remove agent: {}. Try: kallip subagent interrupt {}",
                msg.to_lowercase(),
                id
            );
        }
    }
    result
}
