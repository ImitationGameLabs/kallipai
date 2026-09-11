//! kallip: tagma client CLI.

mod args;
mod reference;
mod skill;
mod task;
mod team;

use anyhow::Result;
use args::{
    AgentCommand, AgentDirCommand, ApprovalCommand, BudgetCommand, Cli, Commands, FileCommand,
    InboxCommand, LescheCommand, PolicyCommand, ProfileSetCommand, SkillCommand, SubagentCommand,
};
use clap::{CommandFactory, Parser};
use kallip::file::FilesClient;
use kallip_client::TagmaClient;
use kallip_client::types::LescheSessionEntry;
use kallip_common::agentid::AgentId;
use kallip_common::policy::{ExecDecision, ExecOverride};
use kallip_common::protocol::{ProfileSetUpdateRequest, SetDefaultRequest};
use kallip_common::timefmt;
use kallip_common::tokens::parse_token_amount;
use kallip_runtime::profile::{ProfileConfig, ProfileSet};
use uuid::Uuid;

/// Read agent ID from KALLIP_ID env var.
fn agent_id_from_env() -> anyhow::Result<AgentId> {
    std::env::var("KALLIP_ID")
        .map_err(|_| anyhow::anyhow!("KALLIP_ID env var not set"))
        .and_then(|s| s.parse::<AgentId>().map_err(Into::into))
}
/// Read the full stdin as a text payload (multiline — pipe, heredoc, or
/// `< file`). Stdin is the only text entry point for `message`,
/// `lesche send`, and `subagent spawn`'s initial prompt: shell argument
/// forms are removed so shell expansion (backticks/`$` inside double
/// quotes) can never corrupt a message; prefer a quoted heredoc
/// `<<'EOF'`.
fn read_text_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
    Ok(buf)
}

/// Render a set's effective modalities in canonical order ("text, image").
/// Shared by `profile-set list` and the per-agent `status` deep view so both
/// faces read the same fact.
fn modality_list(
    effective: std::collections::BTreeSet<kallip_common::protocol::Modality>,
) -> String {
    kallip_common::protocol::Modality::ALL
        .iter()
        .filter(|m| effective.contains(m))
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Display suffix for a set whose members declare beyond the effective
/// intersection (shared by `profile-set list` and the `status` deep view).
fn modality_shadow_suffix(set: &ProfileSet) -> &str {
    if set.has_shadowed_members() {
        " (intersection; some members declare more)"
    } else {
        ""
    }
}

/// Map the stdin text to the optional initial spawn prompt: empty or
/// whitespace-only stdin means no prompt, matching the optional prompt
/// semantics of the wire request. Non-blank text is passed through
/// verbatim (not trimmed).
fn prompt_from_stdin_text(text: String) -> Option<String> {
    (!text.trim().is_empty()).then_some(text)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.reference {
        println!("{}", reference::render());
        return Ok(());
    }
    let Some(command) = cli.command else {
        // Bare `kallip` with no subcommand: print help and exit 0.
        Cli::command().print_help()?;
        return Ok(());
    };
    // The file family talks to the files service directly (spawn-env
    // credentials); it needs no tagma daemon connection, so dispatch it
    // before the tagma client is built.
    if let Commands::File(cmd) = &command {
        run_file(cmd).await?;
        return Ok(());
    }
    let client = TagmaClient::from_env()?;

    match command {
        Commands::Agent(cmd) => match cmd {
            AgentCommand::Message(args) => {
                let id = client.resolve_agent_ref(args.id.as_ref()).await?;
                // Echo prints only on success: a failed send must not look
                // delivered.
                let text = read_text_stdin()?;
                let resp = client.post_message(&id, &text).await?;
                println!(
                    "{}",
                    kallip_common::message::message_sent_line(
                        id.as_ref(),
                        &text,
                        resp.queue_depth,
                        resp.warning.as_deref()
                    )
                );
            }
            AgentCommand::Status(args) => {
                // Two scales: no argument renders the fleet overview;
                // an argument keeps the per-agent deep view unchanged.
                let Some(ref_arg) = args.id else {
                    print_status_overview(&client).await?;
                    return Ok(());
                };
                let id = client.resolve_agent_ref(ref_arg.as_ref()).await?;
                let status = client.agent_status(&id).await?;
                let now = now_epoch();
                // Four blank-line groups: a timezone-anchored clock first (readers
                // calibrate against it instead of doing date arithmetic), then
                // state, context, retries.
                println!("current datetime: {}", timefmt::format_utc(now));
                println!();
                println!("state: {}", status.state);
                println!();
                println!("{}", status.context.format_summary());
                // Tagma-wide shared budget rides in the context group; it is a
                // token magnitude, so it never participates in --relative-time.
                if status.token_budget_unlimited {
                    println!(
                        "budget: unlimited / {} consumed",
                        timefmt::humanize_count(status.token_consumed)
                    );
                } else {
                    println!(
                        "budget: {} / {} remaining",
                        timefmt::humanize_count(
                            status.token_budget.saturating_sub(status.token_consumed)
                        ),
                        timefmt::humanize_count(status.token_budget)
                    );
                }
                if !status.recent_retries.is_empty() {
                    println!();
                    // The summary carries our classification, not the vendor's
                    // wording; the archived error body stays out of the line view.
                    println!(
                        "retries: {} (last: {})",
                        status.recent_retries.len(),
                        status.recent_retries[0].kind.as_str()
                    );
                    for r in &status.recent_retries {
                        let stamp = if args.relative_time {
                            timefmt::format_relative(now, r.timestamp)
                        } else {
                            timefmt::format_utc(r.timestamp)
                        };
                        let quota = r
                            .quota_reset
                            .map(|q| format!("  quota resets {}", timefmt::format_relative(now, q)))
                            .unwrap_or_default();
                        println!(
                            "  {stamp}  {:<11} attempt {}/{}  round {}  backoff {:.0}s{quota}",
                            r.kind.as_str(),
                            r.attempt,
                            r.max_attempts,
                            r.round,
                            r.delay_secs,
                        );
                    }
                }
                // The bound set's effective modalities — the same fact the
                // management UI and `profile-set list` show (the member
                // intersection governs; shadowed members are flagged). A
                // profiles-fetch or parse failure degrades the read view to
                // the bare set name rather than failing the whole status.
                if let Some(active) = &status.profile {
                    let cfg = client
                        .get_profiles()
                        .await
                        .ok()
                        .and_then(|v| serde_json::from_value::<ProfileConfig>(v).ok());
                    if let Some(set) = cfg.as_ref().and_then(|c| c.sets.get(&active.profile_set)) {
                        let suffix = modality_shadow_suffix(set);
                        println!();
                        println!(
                            "profile set: {} (modalities: {}){suffix}",
                            active.profile_set,
                            modality_list(set.effective_modalities())
                        );
                    } else {
                        println!();
                        println!("profile set: {}", active.profile_set);
                    }
                }
            }
            AgentCommand::Activity(args) => {
                // Activity is self-reported: the target is always the calling
                // agent (KALLIP_ID); the tagma only accepts this from the
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
        Commands::Dir(cmd) => match cmd {
            AgentDirCommand::List => print_agent_directory(&client).await?,
        },
        Commands::Lesche(cmd) => match cmd {
            LescheCommand::Send(args) => {
                // Self-only: send as the calling agent (KALLIP_ID). The text
                // is the full stdin (multiline). Deliver via the tagma's relay
                // first, then print the stable marker only on success — so a
                // failed POST (relay down, burst cap, etc.) does not let local
                // clients render a message that was never delivered.
                let id = agent_id_from_env()?;
                let text = read_text_stdin()?;
                client
                    .post_message_delivery(&id, &text, args.room.as_deref(), args.tagma.as_deref())
                    .await?;
                println!("{}", kallip_common::message::marker_line(&text));
            }
            LescheCommand::Rooms => {
                // Self-only: list the calling tagma's joined rooms.
                let id = agent_id_from_env()?;
                let rooms = client.list_joined_rooms(&id).await?;
                if rooms.is_empty() {
                    println!("(no rooms joined)");
                } else {
                    for room in rooms {
                        println!("{room}");
                    }
                }
            }
            LescheCommand::Read(args) => {
                // Self-only: read one conversation's history (room or direct
                // session). The tagma route renders a text block (one bracketed
                // block per message), so print it verbatim (no trailing newline
                // added).
                let id = agent_id_from_env()?;
                let text = match (&args.room, &args.tagma) {
                    (Some(room), _) => {
                        client
                            .read_room_messages(&id, room, args.after_seq, args.limit)
                            .await?
                    }
                    (_, Some(peer)) => {
                        client
                            .read_direct_session_messages(&id, peer, args.after_seq, args.limit)
                            .await?
                    }
                    // clap enforces exactly-one (`required_unless_present` +
                    // `conflicts_with`), so this arm is unreachable via the CLI.
                    (None, None) => anyhow::bail!("--room or --tagma is required"),
                };
                print!("{text}");
            }
            LescheCommand::Sessions => {
                // Self-only: every addressable surface in one list. The tagma
                // aggregates (bilateral + rooms + direct sessions); print one
                // line per surface so the ids are copy-pastable into
                // `send --room` / `send --tagma` / `read --tagma`.
                let id = agent_id_from_env()?;
                let entries = client.list_lesche_sessions(&id).await?;
                if entries.is_empty() {
                    println!("(no sessions)");
                }
                for entry in entries {
                    println!("{}", render_session_line(&entry));
                }
            }
        },
        Commands::Subagent(cmd) => {
            let current = agent_id_from_env()?;
            match cmd {
                SubagentCommand::Spawn(args) => {
                    // Initial prompt (optional) comes from stdin: empty or whitespace-only
                    // stdin means none (`prompt_from_stdin_text`); the text is passed
                    // through verbatim.
                    let prompt = prompt_from_stdin_text(read_text_stdin()?);
                    let id = client
                        .spawn(kallip_common::protocol::CreateAgentRequest {
                            workspace_root: args.workspace_root,
                            skills: args.skills,
                            prompt,
                            created_by: Some(current),
                            role: args.role.unwrap_or_default(),
                            description: args.description.unwrap_or_default(),
                            max_tool_rounds: None,
                            profile_set: args.profile_set,
                            permission_class: args.permission_class,
                            delegation_mode: args.full_handoff.then(|| {
                                kallip_common::protocol::DELEGATION_FULL_HANDOFF.to_owned()
                            }),
                        })
                        .await?;
                    println!("{id}");
                }
                SubagentCommand::List => {
                    let agents = client.list_agents(Some(&current)).await?;
                    print_agent_list(&agents, "No direct subagents.");
                }
                SubagentCommand::Remove(args) => {
                    let id = client.resolve_agent_ref(args.id.as_ref()).await?;
                    annotate_remove_error(client.remove_agent(&id).await, &id)?;
                    println!("Agent {id} archived.");
                }
                SubagentCommand::Interrupt(args) => {
                    let id = client.resolve_agent_ref(args.id.as_ref()).await?;
                    client.interrupt_agent(&id).await?;
                    println!("Agent {id} interrupted.");
                }
                SubagentCommand::Metadata(args) => {
                    let id = client.resolve_agent_ref(args.id.as_ref()).await?;
                    // A rename re-points the addressing alias; capturing the
                    // old role first lets the notice below name it.
                    let old_role = client
                        .list_agents(None)
                        .await?
                        .into_iter()
                        .find(|a| a.id == id)
                        .map(|a| a.role);
                    let updated = client
                        .update_agent_metadata(
                            &id,
                            kallip_common::protocol::UpdateAgentMetadataRequest {
                                role: args.role.clone(),
                                description: args.description,
                            },
                        )
                        .await?;
                    print_agent_summary(&updated);
                    if let (Some(old), Some(new)) = (old_role.as_deref(), args.role.as_deref())
                        && old != new
                    {
                        eprintln!("warning: uuid unchanged; '{new}' is now the addressing alias");
                        if !old.is_empty() {
                            eprintln!(
                                "tips: notify agents or scripts that address '{old}' — they will now get 'role not found'"
                            );
                        }
                    }
                }
            }
        }
        Commands::Approval(cmd) => match cmd {
            ApprovalCommand::List(args) => {
                let status = if args.all {
                    None
                } else {
                    args.status.clone().or(Some("committed".into()))
                };
                let order = if args.reverse { "asc" } else { "desc" };
                let requested_by = match &args.requested_by {
                    Some(r) => Some(client.resolve_agent_ref(r).await?),
                    None => None,
                };
                let resp = client
                    .list_approvals(&kallip_client::ListApprovalsParams {
                        offset: args.offset,
                        limit: args.limit,
                        requested_by,
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
                let id = client.resolve_agent_ref(args.id.as_ref()).await?;
                let perms = client.agent_permissions(&id).await?;
                println!("max_depth: {}", perms.max_depth);
                println!("workspace_root: {}", perms.workspace_root);
                if let Some(sup) = &perms.created_by {
                    println!("created_by: {sup}");
                }
                println!("permission_class: {}", perms.permission_class);
                println!("preset: {}", perms.preset);
            }
            PolicyCommand::ExecGet(args) => {
                let id = client.resolve_agent_ref(args.id.as_ref()).await?;
                let policy = client.get_exec_policy(&id).await?;
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
                let id = client.resolve_agent_ref(args.id.as_ref()).await?;
                let mut policy = client.get_exec_policy(&id).await?;
                policy
                    .overrides
                    .insert(args.command.to_ascii_lowercase(), entry);
                client.update_exec_policy(&id, &policy).await?;
                println!("Updated {} = {}.", args.command, decision);
            }
        },
        Commands::Skill(cmd) => match cmd {
            SkillCommand::Index(args) => {
                println!("{}", skill::render_skill_index(&args.path, args.depth)?);
            }
            SkillCommand::Meta(args) => {
                let meta = skill::read_skill_meta(&args.path)?;
                println!("name: {}", meta.name);
                if let Some(desc) = &meta.description {
                    println!("description: {desc}");
                }
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
            BudgetCommand::Unlimited => {
                if let Err(e) = client.set_token_budget_unlimited().await {
                    match unlimited_reject_hint(&e) {
                        Some(hint) => anyhow::bail!("{hint}"),
                        None => return Err(e),
                    }
                }
                let resp = client.get_token_budget().await?;
                println!("Budget is now unlimited. {}", resp.format_display());
            }
        },
        Commands::ProfileSet(cmd) => match cmd {
            ProfileSetCommand::List => {
                // Typed parse of the masked wire config: the modality
                // presentation comes from the same effective_modalities()
                // source the runtime and the management UI use.
                let cfg: ProfileConfig = serde_json::from_value(client.get_profiles().await?)?;
                let agents = client.list_agents(None).await?;
                if !cfg.default.is_empty() {
                    println!("default set: {}", cfg.default);
                }
                for (name, set) in &cfg.sets {
                    let users = agents
                        .iter()
                        .filter(|a| a.profile_set.as_deref() == Some(name.as_str()))
                        .count();
                    let suffix = modality_shadow_suffix(set);
                    println!(
                        "{name}: {} profile(s), {users} agent(s), modalities: {}{suffix}",
                        set.profiles.len(),
                        modality_list(set.effective_modalities())
                    );
                }
            }
            ProfileSetCommand::Bind { id, set } => {
                let id = client.resolve_agent_ref(&id).await?;
                let summary = client
                    .bind_profile_set(
                        &id,
                        ProfileSetUpdateRequest {
                            profile_set: set.clone(),
                        },
                    )
                    .await?;
                println!("Bound {} to profile set {}.", agent_label(&summary), set);
            }
            ProfileSetCommand::Default { set } => {
                let cfg = client
                    .set_default_profile_set(SetDefaultRequest {
                        default: set.clone(),
                    })
                    .await?;
                let new_default = cfg.get("default").and_then(|v| v.as_str()).unwrap_or("");
                println!("Default profile set: {new_default}");
            }
            ProfileSetCommand::Remove { set, force } => {
                let resp = client.delete_profile_set(&set, force).await?;
                if resp.interrupted.is_empty() {
                    println!("Removed profile set {}.", resp.removed);
                } else {
                    let interrupted: Vec<String> = resp
                        .interrupted
                        .iter()
                        .map(|r| {
                            if r.role.is_empty() {
                                r.id.to_string()
                            } else {
                                r.role.clone()
                            }
                        })
                        .collect();
                    println!(
                        "Removed profile set {} (interrupted: {}).",
                        resp.removed,
                        interrupted.join(", ")
                    );
                }
            }
        },
        Commands::Inbox(cmd) => match cmd {
            InboxCommand::List(args) => {
                let id = resolve_id_ref(&client, args.id).await?;
                let resp = client
                    .inbox_list(&id, args.status.as_deref(), args.limit)
                    .await?;
                if resp.is_empty() {
                    println!("Inbox is empty.");
                } else {
                    // A clock anchor at the top: inbox times are core content,
                    // and the header calibrates the per-entry stamps below it.
                    println!("current datetime: {}", timefmt::format_utc(now_epoch()));
                    for e in &resp {
                        print_inbox_entry(e, now_epoch(), args.relative_time);
                        println!("---");
                    }
                    println!("(showing {})", resp.len());
                }
            }
            InboxCommand::Read(args) => {
                let id = resolve_id_ref(&client, args.id).await?;
                let e = client.inbox_read(&id, args.msg_id).await?;
                print_inbox_entry(&e, now_epoch(), false);
            }
            InboxCommand::Summary(args) => {
                let id = resolve_id_ref(&client, args.id).await?;
                let s = client.inbox_summary(&id).await?;
                println!("total: {}", s.total);
                println!("unread: {}", s.unread);
            }
            InboxCommand::Done(args) => {
                let id = resolve_id_ref(&client, args.id).await?;
                client.inbox_mark_done(&id, args.msg_id).await?;
                println!("Marked done.");
            }
            InboxCommand::Clear(args) => {
                let id = resolve_id_ref(&client, args.id).await?;
                let cleared = client.inbox_clear(&id, args.all).await?;
                println!("Cleared {cleared} message(s).");
            }
        },
        // Dispatched before the tagma client is built; the compiler
        // still wants the arm here.
        Commands::File(_) => unreachable!("file family dispatched above"),
        Commands::Task(cmd) => task::run_task(&client, &cmd).await?,
        Commands::Team(cmd) => team::run_team(&client, &cmd).await?,
    }
    Ok(())
}

/// The `kallip file` family: thin rendering over the lib's files client.
/// Every command reads its credentials from the spawn env, so a missing
/// variable errors before any request is made.
async fn run_file(cmd: &FileCommand) -> Result<()> {
    match cmd {
        FileCommand::Put(args) => {
            let client = FilesClient::from_env()?;
            let response = client.put_file(&args.path, &args.file).await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                println!("record: {}  blob: {}", response.record_id, response.blob_id);
            }
        }
        FileCommand::Get(args) => {
            let id = parse_record_id(&args.id)?;
            let client = FilesClient::from_env()?;
            let content = client.get_file(id).await?;
            match &args.out {
                Some(out) => std::fs::write(out, &content)
                    .map_err(|e| anyhow::anyhow!("cannot write {out:?}: {e}"))?,
                // Binary-safe: the raw bytes go to the locked stdout.
                None => {
                    use std::io::Write as _;
                    std::io::stdout().lock().write_all(&content)?;
                }
            }
        }
        FileCommand::Send(args) => {
            let id = parse_record_id(&args.id)?;
            let client = FilesClient::from_env()?;
            let response = client
                .send_file(id, args.to_user.as_deref(), args.to_tagma.as_deref())
                .await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                println!("delivered: {}  to: {}", response.record_id, response.path);
            }
        }
        FileCommand::Ls(args) => {
            let client = FilesClient::from_env()?;
            let entries = client
                .list_files(&args.space, args.prefix.as_deref(), args.limit)
                .await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("(no files)");
            } else {
                for entry in &entries {
                    println!(
                        "{}  {}B  {}  {}",
                        entry.id, entry.size, entry.created_at, entry.path
                    );
                }
                println!("(showing {})", entries.len());
            }
        }
    }
    Ok(())
}

/// Validate a record id up front: a malformed uuid is a caller error,
/// not a round trip the service must 404.
fn parse_record_id(raw: &str) -> Result<Uuid> {
    Uuid::parse_str(raw).map_err(|e| anyhow::anyhow!("invalid record id '{raw}': {e}"))
}

fn resolve_id(id: Option<AgentId>) -> Result<AgentId, anyhow::Error> {
    id.map(Ok).unwrap_or_else(|| {
        std::env::var("KALLIP_ID")
            .map(|s| s.parse::<AgentId>())
            .map_err(|_| anyhow::anyhow!("KALLIP_ID not set and --id not given"))?
            .map_err(|e| anyhow::anyhow!("invalid KALLIP_ID: {e}"))
    })
}

/// `--id`-style target: the env default (a uuid) passes through; an explicit
/// value may be a role and goes through ref resolution like any `<ID>`.
async fn resolve_id_ref(client: &TagmaClient, id: Option<AgentId>) -> Result<AgentId> {
    let id = resolve_id(id)?;
    client.resolve_agent_ref(id.as_ref()).await
}
fn now_epoch() -> u64 {
    timefmt::now_epoch()
}

fn print_inbox_entry(e: &kallip_client::InboxEntry, now: u64, relative: bool) {
    println!("id: {}", e.id);
    println!("source: {}", e.source);
    println!("status: {}", e.status);
    let epoch = e.timestamp.unix_timestamp().max(0) as u64;
    let stamp = if relative {
        timefmt::format_relative(now, epoch)
    } else {
        timefmt::format_utc(epoch)
    };
    println!("time: {stamp}");
    println!("body: {}", e.body);
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

/// Triage rank for the fleet overview: anomalies first (faulted, parked,
/// waiting), healthy work later. Lower sorts earlier.
fn state_rank(s: kallip_common::protocol::AgentState) -> u8 {
    use kallip_common::protocol::AgentState;
    match s {
        AgentState::Faulted => 0,
        AgentState::Parked => 1,
        AgentState::Waiting => 2,
        AgentState::Retrying => 3,
        AgentState::Busy => 4,
        AgentState::Idle => 5,
    }
}

/// One agent block, shared by the fleet overview and the directory: labelled
/// fields on their own lines. The description is untruncated and the
/// workspace is the full absolute path — these views are read end-to-end,
/// not scanned, so density would only hurt.
fn render_agent_block(a: &kallip_common::protocol::AgentSummary, now: u64) -> String {
    let since = a
        .state_since
        .map(|t| timefmt::format_relative(now, t))
        .unwrap_or_else(|| "?".to_string());
    let state = format!("{} (since {since})", a.state);
    let mut block = format!("role: {}\nid: {}", agent_label(a), a.id);
    if !a.description.is_empty() {
        block.push_str(&format!("\ndesc: {}", a.description));
    }
    block.push_str(&format!("\nstate: {state}\nws: {}", a.workspace_root));
    block
}

/// Print agent blocks with one blank line between them; both the overview
/// and the directory render through this single path.
fn print_agent_blocks(agents: &[kallip_common::protocol::AgentSummary], now: u64) {
    let blocks: Vec<String> = agents.iter().map(|a| render_agent_block(a, now)).collect();
    println!("{}", blocks.join("\n\n"));
}

/// `kallip status` with no argument: the whole fleet at a glance — clock,
/// budget, anomaly-sorted agent blocks, and the drill-down tip.
async fn print_status_overview(client: &TagmaClient) -> Result<()> {
    let now = timefmt::now_epoch();
    let mut agents = client.list_agents(None).await?;
    let budget = client.get_token_budget().await?;
    println!("current datetime: {}", timefmt::format_utc(now));
    println!();
    if budget.unlimited {
        println!(
            "budget: unlimited / {} consumed",
            timefmt::humanize_count(budget.consumed)
        );
    } else {
        println!(
            "budget: {} / {} remaining",
            timefmt::humanize_count(budget.remaining),
            timefmt::humanize_count(budget.budget)
        );
    }
    println!();
    println!();
    if agents.is_empty() {
        println!("(no agents)");
    }
    agents.sort_by(|a, b| {
        state_rank(a.state)
            .cmp(&state_rank(b.state))
            .then(a.state_since.cmp(&b.state_since))
    });
    print_agent_blocks(&agents, now);
    println!();
    println!("tips: kallip status <agent-id> for details");
    Ok(())
}

/// `kallip agent list`: the same blocks in plain role order — a directory,
/// a triage board.
async fn print_agent_directory(client: &TagmaClient) -> Result<()> {
    let now = timefmt::now_epoch();
    let mut agents = client.list_agents(None).await?;
    if agents.is_empty() {
        println!("(no agents)");
        return Ok(());
    }
    agents.sort_by_key(agent_label);
    print_agent_blocks(&agents, now);
    Ok(())
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
        match a.lock {
            Some(kallip_common::protocol::LockState::Missing) => {
                // A live Normal-class agent without its workspace lock is the
                // lock-evaporation signature — make it unmissable.
                line.push_str("  !! lock=missing");
            }
            Some(kallip_common::protocol::LockState::Held) => line.push_str("  lock=held"),
            None => {}
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

/// One presentation line per addressable surface, shaped so the ids can be
/// copy-pasted straight into `send --room` / `send --tagma` / `read --tagma`.
fn render_session_line(entry: &LescheSessionEntry) -> String {
    match entry.kind.as_str() {
        "room" => match &entry.name {
            Some(name) => format!("room {} ({name})", entry.id),
            None => format!("room {}", entry.id),
        },
        "direct" => format!(
            "direct {} peer={} \"{}\"",
            entry.id,
            entry.peer_tagma.as_deref().unwrap_or(""),
            entry.peer_handle.as_deref().unwrap_or(""),
        ),
        kind => format!("{kind} {}", entry.id),
    }
}

/// Maps a failed `budget unlimited` request to the user-facing hint.
/// Servers predating the unlimited wire field reject the request with a
/// 400 whose text says the request "must specify" a budget — surface
/// an upgrade hint instead of the raw protocol error. `None` for any
/// other error, which is propagated unchanged.
fn unlimited_reject_hint(err: &anyhow::Error) -> Option<&'static str> {
    let msg = format!("{err:#}");
    if msg.contains("must specify") {
        Some("this server does not support unlimited budgets; upgrade the tagma")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_whitespace_stdin_means_no_prompt() {
        assert_eq!(prompt_from_stdin_text(String::new()), None);
        assert_eq!(prompt_from_stdin_text(" \n\t".into()), None);
    }

    #[test]
    fn nonblank_stdin_is_the_prompt_verbatim() {
        assert_eq!(
            prompt_from_stdin_text(" explore \n".into()).as_deref(),
            Some(" explore \n")
        );
    }

    #[test]
    fn file_ls_parses_space_and_flags() {
        let cli = Cli::try_parse_from([
            "kallip", "file", "ls", "--space", "shared", "--prefix", "inbox/", "--json",
        ])
        .expect("parses");
        match cli.command.expect("command") {
            Commands::File(FileCommand::Ls(args)) => {
                assert_eq!(args.space, "shared");
                assert_eq!(args.prefix.as_deref(), Some("inbox/"));
                assert!(args.json);
                assert!(args.limit.is_none());
            }
            _ => panic!("expected file ls"),
        }
    }

    #[test]
    fn file_ls_rejects_an_unknown_space() {
        let Err(err) = Cli::try_parse_from(["kallip", "file", "ls", "--space", "other"]) else {
            panic!("invalid space accepted")
        };
        assert!(err.to_string().contains("self"), "{err}");
    }

    #[test]
    fn file_send_rejects_two_targets_at_once() {
        let Err(err) = Cli::try_parse_from([
            "kallip",
            "file",
            "send",
            &uuid::Uuid::new_v4().to_string(),
            "--to-user",
            "someone",
            "--to-tagma",
            "sometagma",
        ]) else {
            panic!("conflicting targets accepted")
        };
        assert!(err.to_string().contains("cannot be used with"), "{err}");
    }
    #[test]
    fn file_send_requires_a_target() {
        let Err(err) =
            Cli::try_parse_from(["kallip", "file", "send", &uuid::Uuid::new_v4().to_string()])
        else {
            panic!("a send without a target accepted")
        };
        assert!(err.to_string().contains("required arguments"), "{err}");
    }

    #[test]
    fn lesche_send_rejects_room_and_tagma_together() {
        let Err(err) = Cli::try_parse_from([
            "kallip",
            "lesche",
            "send",
            "--room",
            "room-1",
            "--tagma",
            "tagma-peer",
        ]) else {
            panic!("conflicting targets accepted")
        };
        assert!(err.to_string().contains("cannot be used with"), "{err}");
    }

    #[test]
    fn lesche_read_requires_a_target() {
        let Err(err) = Cli::try_parse_from(["kallip", "lesche", "read"]) else {
            panic!("a read without a target accepted")
        };
        assert!(err.to_string().contains("required arguments"), "{err}");
    }

    #[test]
    fn lesche_read_rejects_room_and_tagma_together() {
        let Err(err) = Cli::try_parse_from([
            "kallip",
            "lesche",
            "read",
            "--room",
            "room-1",
            "--tagma",
            "tagma-peer",
        ]) else {
            panic!("conflicting targets accepted")
        };
        assert!(err.to_string().contains("cannot be used with"), "{err}");
    }

    #[test]
    fn render_session_line_covers_bilateral_room_and_direct_shapes() {
        let bilateral = LescheSessionEntry {
            kind: "bilateral".into(),
            id: "conv-1".into(),
            name: None,
            peer_tagma: None,
            peer_handle: None,
        };
        assert_eq!(render_session_line(&bilateral), "bilateral conv-1");

        let room = LescheSessionEntry {
            kind: "room".into(),
            id: "room-1".into(),
            name: Some("ops".into()),
            peer_tagma: None,
            peer_handle: None,
        };
        assert_eq!(render_session_line(&room), "room room-1 (ops)");

        let direct = LescheSessionEntry {
            kind: "direct".into(),
            id: "sess-1".into(),
            name: None,
            peer_tagma: Some("tagma-peer".into()),
            peer_handle: Some("Peer".into()),
        };
        assert_eq!(
            render_session_line(&direct),
            "direct sess-1 peer=tagma-peer \"Peer\""
        );
    }

    /// The unlimited subcommand parses bare (no amount), wiring the
    /// set-unlimited request path.
    #[test]
    fn budget_unlimited_parses_without_args() {
        let cli = Cli::try_parse_from(["kallip", "budget", "unlimited"]).expect("parses");
        match cli.command.expect("command") {
            Commands::Budget(BudgetCommand::Unlimited) => {}
            _ => panic!("expected budget unlimited"),
        }
    }

    /// A pre-unlimited server rejection maps to the upgrade hint.
    #[test]
    fn unlimited_reject_hint_maps_must_specify_errors() {
        let err = anyhow::anyhow!(
            "failed to set token budget unlimited: HTTP 400: request must specify one of 'set_remaining', 'delta', or 'set_unlimited'"
        );
        assert_eq!(
            unlimited_reject_hint(&err),
            Some("this server does not support unlimited budgets; upgrade the tagma")
        );
    }

    /// Any other failure propagates unchanged (no hint).
    #[test]
    fn unlimited_reject_hint_ignores_unrelated_errors() {
        let err = anyhow::anyhow!("connection refused");
        assert_eq!(unlimited_reject_hint(&err), None);
    }
}
