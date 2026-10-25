//! kallip: tagma client CLI.

mod args;
mod image;
mod reference;
mod skill;
mod task;
mod team;

use anyhow::Result;
use args::{
    Cli, Commands,
    agent::{AgentCommand, AgentDirCommand},
    approval::ApprovalCommand,
    budget::BudgetCommand,
    file::FileCommand,
    inbox::InboxCommand,
    lesche::LescheCommand,
    policy::PolicyCommand,
    profile::ProfileSetCommand,
    skill::SkillCommand,
    subagent::{SubagentCommand, WorkspaceCommand},
};
use clap::{CommandFactory, Parser};
use kallip::file::FilesClient;
use kallipai_client::DirectReadQuery;
use kallipai_client::MessageDelivery;
use kallipai_client::RoomReadQuery;
use kallipai_client::TagmaClient;
use kallipai_client::profile::{ProfileConfig, ProfileSet};
use kallipai_client::types::LescheSessionEntry;
use kallipai_common::agentid::AgentId;
use kallipai_common::policy::{ExecDecision, ExecOverride};
use kallipai_common::protocol::{
    BACKGROUND_TASK_LIST_MAX, BgTaskView, InterruptOutcome, ProfileSetUpdateRequest,
};
use kallipai_common::timefmt;
use kallipai_common::tokens::parse_token_amount;
use uuid::Uuid;

/// Read agent ID from KALLIPAI_ID env var.
pub(crate) fn agent_id_from_env() -> anyhow::Result<AgentId> {
    std::env::var("KALLIPAI_ID")
        .map_err(|_| anyhow::anyhow!("KALLIPAI_ID env var not set"))
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
    effective: std::collections::BTreeSet<kallipai_common::protocol::Modality>,
) -> String {
    kallipai_common::protocol::Modality::ALL
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

/// Build the spawn request from parsed `subagent spawn` arguments, so each
/// flag maps to one wire field in a single place the tests can reach.
///
/// The keep share is checked here, before serde sees it: the wire form is JSON,
/// which has no non-finite number, so `NaN` or an infinity would be written as
/// `null`, read back as "unset", and leave the flag silently ignored. The env
/// knob refuses the same values, and both ask one predicate for the bound.
fn spawn_request(
    args: args::subagent::SpawnArgs,
    prompt: Option<String>,
    created_by: AgentId,
) -> Result<kallipai_common::protocol::CreateAgentRequest> {
    if let Some(share) = args.context_keep_tokens_ratio
        && !kallipai_common::protocol::context_keep_tokens_ratio_in_range(share)
    {
        anyhow::bail!(
            "--context-keep-tokens-ratio must be greater than 0 and at most 1 (got {share})"
        );
    }
    Ok(kallipai_common::protocol::CreateAgentRequest {
        workspace_root: args.workspace_root,
        skills: args.skills,
        prompt,
        created_by: Some(created_by),
        role: args.role.unwrap_or_default(),
        description: args.description.unwrap_or_default(),
        max_tool_rounds: None,
        context_keep_turns: args.context_keep_turns,
        context_keep_tokens_ratio: args.context_keep_tokens_ratio,
        profile_set: args.profile_set,
        permission_class: args.permission_class,
        delegation_mode: args
            .full_handoff
            .then(|| kallipai_common::protocol::DELEGATION_FULL_HANDOFF.to_owned()),
    })
}

/// Refuse a backward cursor at a target that has no such rows. `--before`
/// names a position in a room's log and asks for the page ending below it;
/// a direct-session read takes a forward cursor only (`after_seq` and
/// `limit`), so the route would never see the flag and the request would
/// quietly replay that session from its start instead of ending a page.
///
/// A `--quote` rides either target: it names one row, not a page edge, and
/// a direct read reaches that row from below (`direct_read_query`).
///
/// Flag-vs-flag conflicts live in clap; a flag against the target lives
/// here, because clap cannot say what a seq names.
fn reject_backward_cursor_at_non_room(args: &args::lesche::ReadArgs) -> Result<()> {
    if args.room.is_some() || args.before_seq.is_none() {
        return Ok(());
    }
    anyhow::bail!("--before rides a room read: a seq names a row in that room's log");
}

/// The page a room read asks for. `quote` is the one-row lookup behind a
/// `| quote=<seq>` segment, so it fixes the page to a single row and carries no
/// page size of its own (clap refuses `--limit` beside it).
fn room_read_query(args: &args::lesche::ReadArgs) -> Result<RoomReadQuery> {
    if let Some(seq) = args.quote {
        let Some(before_seq) = seq.checked_add(1) else {
            anyhow::bail!("--quote {seq} is out of range");
        };
        return Ok(RoomReadQuery {
            after_seq: None,
            before_seq: Some(before_seq),
            limit: Some(1),
        });
    }
    Ok(RoomReadQuery {
        after_seq: args.after_seq,
        before_seq: args.before_seq,
        limit: args.limit,
    })
}

/// The page a direct-session read asks for. `quote` is the one-row lookup
/// behind a `| quote=<seq>` segment: the direct log is read forward only, so
/// the row at `seq` is reached from below, with a page of one. Without a
/// quote the caller's own cursor and page size pass through.
fn direct_read_query(args: &args::lesche::ReadArgs) -> DirectReadQuery {
    match args.quote {
        Some(seq) => DirectReadQuery {
            // `--quote` is a flag clap keeps at one or above (`.range(1..)`),
            // so this subtraction has no floor to hit.
            after_seq: Some(seq - 1),
            limit: Some(1),
        },
        None => DirectReadQuery {
            after_seq: args.after_seq,
            limit: args.limit,
        },
    }
}

/// The block a `--quote` read may print: the conversation's own render of the
/// row at `seq`. A first block naming another position, or no block at all,
/// means this read holds nothing to show at `seq` -- a verdict to report,
/// never to render as silence.
///
/// Only the first block is this layer's business. The page size belongs to the
/// data plane: the read asks for one row and that side clamps the page to one,
/// so whether an answer holds a second block is not something plain text can
/// decide -- a row's body may carry block headers of its own. What is checked
/// here is whether the first block is the row that was asked for.
///
/// "Nothing to show" is not the same as "no such row": the renderer skips rows
/// whose payload no longer parses, so the verdict stays a statement about this
/// read.
///
/// The prefix carries the seq this read asked for, not a shape test on a
/// block header: when the named row does not exist, the page lands on the
/// row beside it, which names its own seq, and that mismatch is the verdict.
///
/// The prefix itself comes from `kallipai_common::readback`, the same source
/// the tagma's row render writes with, so the check cannot drift from the
/// shape it checks for.
fn expect_named_block(text: String, seq: i64) -> Result<String> {
    if !kallipai_common::readback::names_position(&text, seq) {
        anyhow::bail!("this read holds no readable row at seq {seq}");
    }
    Ok(text)
}

/// The quote position a `lesche send` may carry: a seq names a row in a
/// conversation's own log, so the flag rides a room send or a direct-session
/// send and points at nothing from the bilateral 1:1. A flag against the
/// target lives here rather than in clap (see
/// `reject_backward_cursor_at_non_room`); the tagma refuses the same send
/// with a 400, one hop later.
fn send_quote(args: &args::lesche::SendArgs) -> Result<Option<i64>> {
    match args.quote {
        Some(_) if args.room.is_none() && args.tagma.is_none() => {
            anyhow::bail!("--quote rides a room or direct send: a seq names a row in that log")
        }
        quote => Ok(quote),
    }
}

/// The members a `lesche send` names: a participant id names a member of a
/// room, so the flags are refused at either non-room target -- the same
/// refusal the tagma answers with 400, one hop earlier. Nothing is judged
/// about the ids here: their shape, the cap and the roster all live where
/// the room does, so a bad id costs nothing but the hop.
fn send_mentions(args: &args::lesche::SendArgs) -> Result<Vec<String>> {
    if args.room.is_none() && !args.mention.is_empty() {
        anyhow::bail!("--mention rides a room send: a participant id names a member of that room")
    }
    Ok(args.mention.clone())
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
                let resp = client.post_message(&id, &text, args.defer).await?;
                println!(
                    "{}",
                    kallipai_common::message::message_sent_line(
                        id.as_ref(),
                        &text,
                        resp.queue_depth,
                        resp.warning.as_deref(),
                        resp.delivery_mode
                    )
                );
            }
            AgentCommand::Status(args) => {
                // Two scales: no argument renders the fleet overview;
                // an argument renders the per-agent deep view.
                let Some(ref_arg) = args.id else {
                    print_status_overview(&client, args.utc).await?;
                    return Ok(());
                };
                let id = client.resolve_agent_ref(ref_arg.as_ref()).await?;
                let status = client.agent_status(&id).await?;
                let now = now_epoch();
                let zone = crate::task::display_zone(args.utc, &client).await;
                // Blank-line groups: a timezone-anchored clock first (readers
                // calibrate against it instead of doing date arithmetic), then
                // state, context and budget, retries, upstream facts, the
                // profile set, and the host-side background tasks.
                println!("current datetime: {}", timefmt::format_display(now, &zone));
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
                        "retry records: {} (last: {})",
                        status.recent_retries.len(),
                        status.recent_retries[0].kind.as_str()
                    );
                    for r in &status.recent_retries {
                        let stamp = if args.relative_time {
                            timefmt::format_relative(now, r.timestamp)
                        } else {
                            timefmt::format_display(r.timestamp, &zone)
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
                if !status.upstream_facts.is_empty() {
                    println!();
                    // The pacing table as this agent reads it: which profile the
                    // upstream is refusing, since when, and until when. One record
                    // per profile, keyed by that profile's id.
                    println!("upstream facts: {}", status.upstream_facts.len());
                    for fact in &status.upstream_facts {
                        let since = timefmt::format_relative(now, fact.recorded_at);
                        let until = fact
                            .until
                            .map(|until| {
                                format!("  reopens {}", timefmt::format_relative(now, until))
                            })
                            .unwrap_or_default();
                        println!(
                            "  {:<14} {:<9} recorded {since}{until}  {}",
                            fact.kind.as_str(),
                            fact.profile,
                            fact_excerpt(&fact.detail)
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
                // Host-side background tasks: the server sends the field only
                // to the operator and the root agent, so an absent field prints
                // nothing at all.
                if let Some(block) = background_tasks_block(
                    status.background_tasks.as_deref(),
                    now,
                    std::env::var("HOME").ok().as_deref(),
                ) {
                    println!();
                    println!("{block}");
                }
            }
            AgentCommand::Activity(args) => {
                // Activity is self-reported: the target is always the calling
                // agent (KALLIPAI_ID); the tagma only accepts this from the
                // agent itself or an operator.
                let id = agent_id_from_env()?;
                client
                    .update_activity(
                        &id,
                        kallipai_common::protocol::UpdateActivityRequest {
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
                // Self-only: send as the calling agent (KALLIPAI_ID). The text
                // is the full stdin (multiline). Deliver via the tagma's relay
                // first, then print the stable marker only on success — so a
                // failed POST (relay down, burst cap, etc.) does not let local
                // clients render a message that was never delivered.
                let id = agent_id_from_env()?;
                // Validate the flags before reading stdin: a wrong target, or a
                // quote, or a mention without a room, is answerable without the message body.
                let quote = send_quote(&args)?;
                let mentions = send_mentions(&args)?;
                let text = read_text_stdin()?;
                client
                    .post_message_delivery(
                        &id,
                        &text,
                        MessageDelivery {
                            room: args.room.as_deref(),
                            tagma: args.tagma.as_deref(),
                            quote,
                            mentions: &mentions,
                        },
                    )
                    .await?;
                println!("{}", kallipai_common::message::marker_line(&text));
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
                // The target check runs for every read; each target assembles
                // its own page.
                reject_backward_cursor_at_non_room(&args)?;
                let text = match (&args.room, &args.tagma, &args.session) {
                    (Some(room), _, _) => {
                        client
                            .read_room_messages(&id, room, room_read_query(&args)?)
                            .await?
                    }
                    (_, Some(peer), _) => {
                        client
                            .read_direct_session_messages(&id, peer, direct_read_query(&args))
                            .await?
                    }
                    (_, _, Some(session)) => {
                        client
                            .read_direct_session_by_id_messages(
                                &id,
                                session,
                                direct_read_query(&args),
                            )
                            .await?
                    }
                    // No target: a bare `--quote` names one row of the bilateral
                    // 1:1 -- the form a 1:1 message's read-back command spells.
                    // clap ties every other combination to a target, so the arm
                    // below is the 1:1 read.
                    (None, None, None) => match args.quote {
                        Some(quote) => client.read_quote(&id, quote).await?,
                        None => anyhow::bail!("--room, --tagma, --session or --quote is required"),
                    },
                };
                // A `--quote` read shows exactly the row it names: the page
                // fields above are how the row is fetched; this is the verdict
                // on what came back.
                let text = match args.quote {
                    Some(seq) => expect_named_block(text, seq)?,
                    None => text,
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
                    let request = spawn_request(args, prompt, current)?;
                    let id = client.spawn(request).await?;
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
                    match client.interrupt_agent(&id).await? {
                        InterruptOutcome::RoundInterrupted => {
                            println!("Agent {id}: the running round was aborted.");
                        }
                        InterruptOutcome::WaitCancelled => {
                            println!(
                                "Agent {id}: the interrupt was handed to the armed wait; if it was still current, the agent settles idle unless work is queued."
                            );
                        }
                        InterruptOutcome::Nothing => {
                            println!(
                                "Agent {id}: nothing to interrupt; the tagma saw no running round and no armed wait."
                            );
                        }
                    }
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
                            kallipai_common::protocol::UpdateAgentMetadataRequest {
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
                SubagentCommand::Workspace { command } => match command {
                    WorkspaceCommand::Relocate(args) => {
                        relocate_workspace(&client, args, false).await?;
                    }
                    WorkspaceCommand::Mv(args) => {
                        relocate_workspace(&client, args, true).await?;
                    }
                },
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
                    .list_approvals(&kallipai_client::ListApprovalsParams {
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
        },
        Commands::Inbox(cmd) => match cmd {
            InboxCommand::List(args) => {
                let id = resolve_id_ref(&client, args.id).await?;
                let zone = crate::task::display_zone(args.utc, &client).await;
                let resp = client
                    .inbox_list(&id, args.status.as_deref(), args.limit)
                    .await?;
                if resp.is_empty() {
                    println!("Inbox is empty.");
                } else {
                    // A clock anchor at the top: inbox times are core content,
                    // and the header calibrates the per-entry stamps below it.
                    println!(
                        "current datetime: {}",
                        timefmt::format_display(now_epoch(), &zone)
                    );
                    for e in &resp {
                        print_inbox_entry(e, now_epoch(), args.relative_time, &zone);
                        println!("---");
                    }
                    println!("(showing {})", resp.len());
                }
            }
            InboxCommand::Read(args) => {
                let id = resolve_id_ref(&client, args.id).await?;
                let e = client.inbox_read(&id, args.msg_id).await?;
                let zone = crate::task::display_zone(args.utc, &client).await;
                print_inbox_entry(&e, now_epoch(), false, &zone);
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
        Commands::Image(cmd) => image::run_image(&client, &cmd).await?,
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
        std::env::var("KALLIPAI_ID")
            .map(|s| s.parse::<AgentId>())
            .map_err(|_| anyhow::anyhow!("KALLIPAI_ID not set and --id not given"))?
            .map_err(|e| anyhow::anyhow!("invalid KALLIPAI_ID: {e}"))
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

fn print_inbox_entry(
    e: &kallipai_client::InboxEntry,
    now: u64,
    relative: bool,
    zone: &timefmt::DisplayZone,
) {
    println!("id: {}", e.id);
    println!("source: {}", e.source);
    println!("status: {}", e.status);
    let epoch = e.timestamp.unix_timestamp().max(0) as u64;
    let stamp = if relative {
        timefmt::format_relative(now, epoch)
    } else {
        timefmt::format_display(epoch, zone)
    };
    println!("time: {stamp}");
    println!("body: {}", e.body);
}

fn print_approval_entry(a: &kallipai_common::protocol::ApprovalEntry) {
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
fn state_rank(s: kallipai_common::protocol::AgentState) -> u8 {
    use kallipai_common::protocol::AgentState;
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
fn render_agent_block(a: &kallipai_common::protocol::AgentSummary, now: u64) -> String {
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
fn print_agent_blocks(agents: &[kallipai_common::protocol::AgentSummary], now: u64) {
    let blocks: Vec<String> = agents.iter().map(|a| render_agent_block(a, now)).collect();
    println!("{}", blocks.join("\n\n"));
}

/// A one-line excerpt of a fact's evidence for the status list: long enough to
/// recognize the failure, short enough to keep the row readable.
fn fact_excerpt(detail: &str) -> String {
    const LIMIT: usize = 80;
    let mut excerpt: String = detail.chars().take(LIMIT).collect();
    if detail.chars().count() > LIMIT {
        excerpt.push('…');
    }
    excerpt.replace('\n', " ")
}

/// Render the working directory for the background-task list: `$HOME` becomes
/// `~`, and a path that does not fit on one line keeps its trailing
/// directory components — the part that identifies the workspace — behind an
/// elision marked at the head. Only a single component too long to fit on its
/// own is cut inside itself.
fn display_working_dir(cwd: &str, home: Option<&str>) -> String {
    const MAX_CHARS: usize = 44;
    const ELISION: &str = "…/";
    let shortened = match home.filter(|home| !home.is_empty()) {
        Some(home) if cwd == home => "~".to_string(),
        Some(home) => {
            let prefix = format!("{home}/");
            match cwd.strip_prefix(prefix.as_str()) {
                Some(rest) => format!("~/{rest}"),
                None => cwd.to_string(),
            }
        }
        None => cwd.to_string(),
    };
    if shortened.chars().count() <= MAX_CHARS {
        return shortened;
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut width = ELISION.chars().count();
    for component in shortened.split('/').filter(|part| !part.is_empty()).rev() {
        let cost = if kept.is_empty() {
            component.chars().count()
        } else {
            component.chars().count() + 1
        };
        if width + cost > MAX_CHARS {
            break;
        }
        width += cost;
        kept.push(component);
    }
    if kept.is_empty() {
        let last = shortened.rsplit('/').next().unwrap_or(shortened.as_str());
        let skip = last.chars().count().saturating_sub(MAX_CHARS - 1);
        let tail: String = last.chars().skip(skip).collect();
        return format!("…{tail}");
    }
    kept.reverse();
    format!("{ELISION}{}", kept.join("/"))
}

/// Wall-clock age of a running task, in the two coarsest units that stay
/// useful: `42s`, `12m 32s`, `1h 04m`, `2d 10h`.
fn humanize_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else if secs < 24 * 60 * 60 {
        format!("{}h {:02}m", secs / (60 * 60), (secs / 60) % 60)
    } else {
        format!("{}d {:02}h", secs / (24 * 60 * 60), (secs / (60 * 60)) % 24)
    }
}

/// Byte magnitude for the background-task list: whole bytes below a kibibyte,
/// in the largest unit that keeps the number short.
fn humanize_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    }
}

/// One background-task block: labelled fields on their own lines, because this
/// view is read top to bottom rather than scanned. The command is printed
/// exactly as the shell recorded it — already flattened and capped at source.
fn render_background_task_block(task: &BgTaskView, now: u64, home: Option<&str>) -> String {
    let state = if task.stalled { "stalled" } else { "running" };
    format!(
        "id={}\nstate={state}\ncmd={}\nworking_dir={}\nelapsed={}\noutput_size={}",
        task.id,
        task.command,
        display_working_dir(&task.cwd, home),
        humanize_elapsed(now.saturating_sub(task.started_at)),
        humanize_bytes(task.bytes),
    )
}

/// The whole host-side background-task group: the count line plus one block per
/// task, or `None` when the server left the field out (a caller that is neither
/// the operator nor the root agent, or a server that predates the field) — the
/// group is then absent rather than printed empty. A list at the wire cap is
/// reported as `20+`: the shell publishes at most the same number of tasks, so
/// a client cannot tell a complete list from a truncated one.
fn background_tasks_block(
    tasks: Option<&[BgTaskView]>,
    now: u64,
    home: Option<&str>,
) -> Option<String> {
    let tasks = tasks?;
    let count = if tasks.len() >= BACKGROUND_TASK_LIST_MAX {
        format!("{BACKGROUND_TASK_LIST_MAX}+")
    } else {
        tasks.len().to_string()
    };
    let blocks: Vec<String> = tasks
        .iter()
        .take(BACKGROUND_TASK_LIST_MAX)
        .map(|task| render_background_task_block(task, now, home))
        .collect();
    let mut group = format!("background tasks: {count}");
    if !blocks.is_empty() {
        group.push('\n');
        group.push('\n');
        group.push_str(&blocks.join("\n\n"));
    }
    Some(group)
}

/// `kallip status` with no argument: the whole fleet at a glance — clock,
/// budget, anomaly-sorted agent blocks, and the drill-down tip.
async fn print_status_overview(client: &TagmaClient, utc: bool) -> Result<()> {
    let now = timefmt::now_epoch();
    let zone = crate::task::display_zone(utc, client).await;
    let mut agents = client.list_agents(None).await?;
    let budget = client.get_token_budget().await?;
    println!("current datetime: {}", timefmt::format_display(now, &zone));
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
fn agent_label(a: &kallipai_common::protocol::AgentSummary) -> String {
    if a.role.is_empty() {
        a.id.to_string()
    } else {
        a.role.clone()
    }
}

/// Print a list of agents one row per line, or `empty_msg` when there are none.
fn print_agent_list(agents: &[kallipai_common::protocol::AgentSummary], empty_msg: &str) {
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
            Some(kallipai_common::protocol::LockState::Missing) => {
                // A live Normal-class agent without its workspace lock is the
                // lock-evaporation signature — make it unmissable.
                line.push_str("  !! lock=missing");
            }
            Some(kallipai_common::protocol::LockState::Held) => line.push_str("  lock=held"),
            None => {}
        }
        println!("{line}");
    }
}

/// Run one `subagent workspace` verb against the daemon and print the
/// per-step notes. `with_files` selects the mv verb (the move runs after
/// the park); the relocate verb expects the target prepared.
async fn relocate_workspace(
    client: &TagmaClient,
    args: args::subagent::RelocateArgs,
    with_files: bool,
) -> anyhow::Result<()> {
    let id = client.resolve_agent_ref(args.id.as_ref()).await?;
    let relocated = client
        .relocate_agent(
            &id,
            kallipai_common::protocol::RelocateRequest {
                workspace_root: args.workspace_root,
                with_files,
            },
        )
        .await?;
    println!(
        "relocated: {}  {} -> {}",
        relocated.agent_id, relocated.old_workspace_root, relocated.workspace_root
    );
    for step in &relocated.steps {
        println!("- {step}");
    }
    Ok(())
}

/// Print an agent's role/description summary (e.g. after a metadata update).
fn print_agent_summary(updated: &kallipai_common::protocol::AgentSummary) {
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
    /// The room and session the arg tests address. Both are canonical UUIDs,
    /// because a `read` target rides a request path and the CLI refuses any
    /// other spelling of one.
    const ROOM: &str = "11111111-1111-4111-8111-111111111111";
    const SESSION: &str = "22222222-2222-4222-8222-222222222222";

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
    /// The request the parsed flags must produce, field for field, so a flag
    /// landing in the wrong field cannot pass by pinning three of them.
    fn expected_request(
        created_by: AgentId,
        context_keep_turns: Option<usize>,
        context_keep_tokens_ratio: Option<f64>,
    ) -> kallipai_common::protocol::CreateAgentRequest {
        kallipai_common::protocol::CreateAgentRequest {
            workspace_root: "/tmp/ws".to_owned(),
            skills: Vec::new(),
            prompt: None,
            created_by: Some(created_by),
            role: String::new(),
            description: String::new(),
            max_tool_rounds: None,
            context_keep_turns,
            context_keep_tokens_ratio,
            profile_set: "primary".to_owned(),
            permission_class: "guest".to_owned(),
            delegation_mode: None,
        }
    }

    #[test]
    fn spawn_keep_flags_reach_the_create_request() {
        let cli = Cli::try_parse_from([
            "kallip",
            "subagent",
            "spawn",
            "--workspace-root",
            "/tmp/ws",
            "--profile-set",
            "primary",
            "--permission-class",
            "guest",
            "--context-keep-turns",
            "3",
            "--context-keep-tokens-ratio",
            "0.5",
        ])
        .expect("parses");
        let created_by = ROOM.parse::<AgentId>().expect("agent id");
        let request = match cli.command.expect("command") {
            Commands::Subagent(SubagentCommand::Spawn(args)) => {
                spawn_request(args, None, created_by.clone()).expect("the flags are valid")
            }
            _ => panic!("expected subagent spawn"),
        };
        assert_eq!(
            request,
            expected_request(created_by, Some(3), Some(0.5)),
            "each flag lands in its own field and nothing else moves"
        );
    }

    #[test]
    fn spawn_without_keep_flags_leaves_the_defaults() {
        let cli = Cli::try_parse_from([
            "kallip",
            "subagent",
            "spawn",
            "--workspace-root",
            "/tmp/ws",
            "--profile-set",
            "primary",
            "--permission-class",
            "guest",
        ])
        .expect("parses");
        let created_by = ROOM.parse::<AgentId>().expect("agent id");
        let request = match cli.command.expect("command") {
            Commands::Subagent(SubagentCommand::Spawn(args)) => {
                spawn_request(args, None, created_by.clone()).expect("the flags are valid")
            }
            _ => panic!("expected subagent spawn"),
        };
        assert_eq!(
            request,
            expected_request(created_by, None, None),
            "an absent flag leaves its field unset and moves nothing else"
        );
    }

    /// A share the wire cannot carry is refused here: JSON writes a non-finite
    /// `f64` as `null`, which the tagma would read back as "unset".
    #[test]
    fn an_unusable_keep_share_is_refused_before_the_wire() {
        let created_by = ROOM.parse::<AgentId>().expect("agent id");
        for share in ["0", "-0.5", "1.5", "NaN", "inf"] {
            let flag = format!("--context-keep-tokens-ratio={share}");
            let cli = Cli::try_parse_from([
                "kallip",
                "subagent",
                "spawn",
                "--workspace-root",
                "/tmp/ws",
                "--profile-set",
                "primary",
                "--permission-class",
                "guest",
                &flag,
            ])
            .expect("clap parses any f64 the flag spells");
            let err = match cli.command.expect("command") {
                Commands::Subagent(SubagentCommand::Spawn(args)) => {
                    spawn_request(args, None, created_by.clone())
                        .expect_err("the share is out of range")
                }
                _ => panic!("expected subagent spawn"),
            };
            assert!(
                err.to_string().contains("--context-keep-tokens-ratio"),
                "{share}: {err}"
            );
        }
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
            ROOM,
            "--tagma",
            "tagma-peer",
        ]) else {
            panic!("conflicting targets accepted")
        };
        assert!(err.to_string().contains("cannot be used with"), "{err}");
    }

    /// A read target has to be the id the lesche minted: the value rides a
    /// request path, so any other spelling is refused before a request exists.
    #[test]
    fn lesche_read_refuses_a_target_that_is_not_an_id() {
        for argv in [
            vec!["kallip", "lesche", "read", "--room", "../../sessions/abc"],
            vec!["kallip", "lesche", "read", "--session", "../rooms/abc"],
        ] {
            let err = match Cli::try_parse_from(&argv) {
                Ok(_) => panic!("a non-id target parsed: {argv:?}"),
                Err(err) => err,
            };
            assert!(err.to_string().contains("canonical UUID"), "{err}");
        }
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
            ROOM,
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

    fn bg_task(id: &str, stalled: bool, bytes: u64, command: &str, cwd: &str) -> BgTaskView {
        BgTaskView {
            id: id.to_string(),
            stalled,
            bytes,
            command: command.to_string(),
            cwd: cwd.to_string(),
            started_at: 1_000,
        }
    }

    /// The deep view's task group end to end: count line, one block per task,
    /// and the fields in the order the reader needs them.
    #[test]
    fn background_task_group_renders_one_block_per_task() {
        let tasks = vec![
            bg_task(
                "9f2c1a04",
                false,
                4198,
                "cargo test --workspace --all-features",
                "/home/simplex/Projects/kallipai-workspace/kallipai-dev",
            ),
            bg_task(
                "3d0f77aa",
                true,
                512,
                "curl -sS --max-time 2 'http://127.0.0.1:8080/v1/health?deep=true'",
                "/home/simplex/team/workspaces/deep/nested/agent-x",
            ),
        ];
        let group =
            background_tasks_block(Some(&tasks), 1_752, Some("/home/simplex")).expect("present");
        assert_eq!(
            group,
            "background tasks: 2\n\n\
             id=9f2c1a04\nstate=running\ncmd=cargo test --workspace --all-features\n\
             working_dir=~/Projects/kallipai-workspace/kallipai-dev\nelapsed=12m 32s\n\
             output_size=4.1 KiB\n\n\
             id=3d0f77aa\nstate=stalled\n\
             cmd=curl -sS --max-time 2 'http://127.0.0.1:8080/v1/health?deep=true'\n\
             working_dir=~/team/workspaces/deep/nested/agent-x\nelapsed=12m 32s\n\
             output_size=512 B"
        );
    }

    /// An absent field is not an empty group: a caller that may not see the
    /// host-side list prints nothing at all, while an entitled caller with no
    /// running tasks still gets the count line.
    #[test]
    fn absent_task_field_prints_nothing_and_an_empty_list_prints_the_count() {
        assert_eq!(
            background_tasks_block(None, 1_752, Some("/home/simplex")),
            None
        );
        assert_eq!(
            background_tasks_block(Some(&[]), 1_752, Some("/home/simplex")),
            Some("background tasks: 0".to_string())
        );
    }

    /// A full list reads as a lower bound: the shell's published list is capped
    /// at the same number, so the client cannot tell a complete list from a
    /// truncated one. A longer input cannot arrive, for the same reason.
    #[test]
    fn a_full_task_list_is_reported_as_a_lower_bound() {
        let tasks: Vec<BgTaskView> = (0..BACKGROUND_TASK_LIST_MAX)
            .map(|i| bg_task(&format!("task-{i}"), false, 0, "sleep 1", "/tmp/work"))
            .collect();
        let group = background_tasks_block(Some(&tasks), 1_752, None).expect("present");
        assert!(group.starts_with("background tasks: 20+\n\n"));
        assert_eq!(group.matches("\nid=").count(), BACKGROUND_TASK_LIST_MAX);
    }

    /// Working directories shorten for the eye only: `$HOME` becomes `~`, and a
    /// path that does not fit on one line keeps the trailing components that
    /// name the workspace, cut inside a component only when it cannot fit.
    #[test]
    fn working_directories_are_shortened_for_display() {
        assert_eq!(
            display_working_dir("/home/simplex/Projects/kallipai-dev", Some("/home/simplex")),
            "~/Projects/kallipai-dev"
        );
        assert_eq!(
            display_working_dir("/home/simplex", Some("/home/simplex")),
            "~"
        );
        assert_eq!(
            display_working_dir("/srv/work", Some("/home/simplex")),
            "/srv/work"
        );
        assert_eq!(
            display_working_dir("/home/simplex/Projects/x", None),
            "/home/simplex/Projects/x"
        );
        assert_eq!(
            display_working_dir(
                "/home/simplex/.local/share/kallipai/tagmata/team/workspaces/deep/nested/agent-x",
                Some("/home/simplex")
            ),
            "…/team/workspaces/deep/nested/agent-x"
        );
        let at_cap = format!("/x/{}", "e".repeat(42));
        assert_eq!(
            display_working_dir(&at_cap, None),
            format!("…/{}", "e".repeat(42))
        );
        let monster = format!("/tmp/{}", "d".repeat(50));
        let cut = display_working_dir(&monster, None);
        assert!(cut.starts_with('…'));
        assert!(!cut.contains('/'));
        assert_eq!(cut.chars().count(), 44);
        assert!(cut.ends_with(&"d".repeat(43)));
    }

    /// Both magnitude renderers switch unit where the number stops being
    /// readable, and pad minutes and hours to two digits.
    #[test]
    fn elapsed_and_output_size_pick_the_unit_that_stays_readable() {
        assert_eq!(humanize_elapsed(42), "42s");
        assert_eq!(humanize_elapsed(752), "12m 32s");
        assert_eq!(humanize_elapsed(3_840), "1h 04m");
        assert_eq!(humanize_elapsed(208_800), "2d 10h");
        assert_eq!(humanize_bytes(512), "512 B");
        assert_eq!(humanize_bytes(4_198), "4.1 KiB");
        assert_eq!(humanize_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    /// Parse a `lesche read` invocation, so a test exercises the flags and the
    /// dispatch input together.
    fn parsed_read(extra: &[&str]) -> args::lesche::ReadArgs {
        let mut argv = vec!["kallip", "lesche", "read"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("parses");
        match cli.command.expect("command") {
            Commands::Lesche(LescheCommand::Read(args)) => args,
            _ => panic!("expected lesche read"),
        }
    }

    /// Parse a rendered read-back command, the way an agent pastes it: the
    /// tokens after `kallip lesche read` are the flags, and the parser is the
    /// one the CLI runs. A reference whose command the parser rejects is a
    /// command nobody can run, which is what this catches.
    fn parse_read_command(command: &str) -> args::lesche::ReadArgs {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        assert_eq!(
            &tokens[..3],
            ["kallip", "lesche", "read"],
            "a read-back command opens with the subcommand it runs: {command}"
        );
        parsed_read(&tokens[3..])
    }

    /// The three faces' read-back commands are accepted by this CLI, and each
    /// one lands on the log it names.
    #[test]
    fn every_rendered_read_back_command_runs() {
        use kallipai_common::readback::QuoteLog;

        let room = parse_read_command(
            &QuoteLog::Room("9f3a1c2e-8b7d-4f6a-9c1e-2d3b4a5c6d7e").read_command(7),
        );
        assert_eq!(
            room.room.as_deref(),
            Some("9f3a1c2e-8b7d-4f6a-9c1e-2d3b4a5c6d7e")
        );
        assert_eq!(room.quote, Some(7));
        assert_eq!(room.session, None);

        let direct = parse_read_command(
            &QuoteLog::Direct("c4b2a8f0-1d3e-4a5b-8c9d-0e1f2a3b4c5d").read_command(7),
        );
        assert_eq!(
            direct.session.as_deref(),
            Some("c4b2a8f0-1d3e-4a5b-8c9d-0e1f2a3b4c5d")
        );
        assert_eq!(direct.quote, Some(7));
        assert_eq!(direct.room, None);
        assert_eq!(direct.tagma, None);

        let bilateral = parse_read_command(&QuoteLog::Bilateral.read_command(42));
        assert_eq!(bilateral.quote, Some(42));
        assert_eq!(bilateral.room, None);
        assert_eq!(bilateral.tagma, None);
        assert_eq!(bilateral.session, None);
    }

    /// A bare `--quote` is the bilateral 1:1 read: no target flag, one row.
    #[test]
    fn a_bare_quote_names_the_bilateral_row() {
        let args = parsed_read(&["--quote", "9"]);
        assert_eq!(args.quote, Some(9));
        assert_eq!(args.room, None);
        assert_eq!(args.tagma, None);
        assert_eq!(args.session, None);
    }

    /// A session-addressed read names that session's log, then asks it for a
    /// page or for one row -- the two shapes a room read has, and never beside
    /// another target.
    #[test]
    fn a_session_read_names_its_log_and_asks_for_a_page_or_a_row() {
        let row = parsed_read(&["--session", SESSION, "--quote", "7"]);
        assert_eq!(row.session.as_deref(), Some(SESSION));
        assert_eq!(row.quote, Some(7));
        let query = direct_read_query(&row);
        assert_eq!(
            query.after_seq,
            Some(6),
            "the quoted row, reached from below"
        );
        assert_eq!(query.limit, Some(1));

        let page = parsed_read(&["--session", SESSION, "--after", "3"]);
        assert_eq!(page.session.as_deref(), Some(SESSION));
        assert_eq!(page.quote, None);
        let query = direct_read_query(&page);
        assert_eq!(
            query.after_seq,
            Some(3),
            "a page read passes the caller's cursor through"
        );
        assert_eq!(query.limit, None);

        assert!(
            Cli::try_parse_from([
                "kallip",
                "lesche",
                "read",
                "--session",
                SESSION,
                "--quote",
                "7",
                "--room",
                ROOM
            ])
            .is_err(),
            "a read names one log"
        );
        assert!(
            Cli::try_parse_from([
                "kallip",
                "lesche",
                "read",
                "--session",
                SESSION,
                "--quote",
                "7",
                "--tagma",
                "peer"
            ])
            .is_err(),
            "a read names one log"
        );
    }

    /// Parse a `lesche send` invocation the same way.
    fn parsed_send(extra: &[&str]) -> args::lesche::SendArgs {
        let mut argv = vec!["kallip", "lesche", "send"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("parses");
        match cli.command.expect("command") {
            Commands::Lesche(LescheCommand::Send(args)) => args,
            _ => panic!("expected lesche send"),
        }
    }

    #[test]
    fn lesche_read_parses_both_new_positions() {
        let backward = parsed_read(&["--room", ROOM, "--before", "5"]);
        assert_eq!(backward.before_seq, Some(5));
        assert_eq!(backward.quote, None);
        let quoted = parsed_read(&["--room", ROOM, "--quote", "7"]);
        assert_eq!(quoted.quote, Some(7));
        assert_eq!(quoted.before_seq, None);
        let forward = parsed_read(&["--room", ROOM, "--after", "3"]);
        assert_eq!(forward.after_seq, Some(3));
        assert_eq!(forward.before_seq, None);
    }

    #[test]
    fn lesche_read_rejects_paging_both_ways_at_once() {
        for extra in [
            ["--room", ROOM, "--after", "1", "--before", "5"],
            ["--room", ROOM, "--after", "1", "--quote", "7"],
            ["--room", ROOM, "--before", "5", "--quote", "7"],
            ["--room", ROOM, "--quote", "7", "--limit", "3"],
        ] {
            let mut argv = vec!["kallip", "lesche", "read"];
            argv.extend_from_slice(&extra);
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "two cursors accepted: {extra:?}"
            );
        }
    }

    #[test]
    fn lesche_read_rejects_a_position_below_one() {
        for extra in [
            ["--room", ROOM, "--before", "0"],
            ["--room", ROOM, "--before", "-3"],
            ["--room", ROOM, "--quote", "0"],
        ] {
            let mut argv = vec!["kallip", "lesche", "read"];
            argv.extend_from_slice(&extra);
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "non-positive position accepted: {extra:?}"
            );
        }
    }

    /// A `--before` cursor needs a page edge to name, and only a room read has
    /// one: a direct session is read forward, a bare direct read carries no
    /// position at all, and a `--quote` names a row rather than an edge, so it
    /// rides either target.
    #[test]
    fn a_backward_cursor_is_refused_only_beside_a_direct_session() {
        let err =
            reject_backward_cursor_at_non_room(&parsed_read(&["--tagma", "peer", "--before", "5"]))
                .expect_err("non-room target");
        assert!(
            err.to_string()
                .contains("--before rides a room read: a seq names a row"),
            "{err}"
        );
        assert!(
            reject_backward_cursor_at_non_room(&parsed_read(&["--tagma", "peer"])).is_ok(),
            "a plain direct read carries no position"
        );
        assert!(
            reject_backward_cursor_at_non_room(&parsed_read(&["--room", ROOM, "--before", "5"]))
                .is_ok(),
            "a room keeps its backward cursor"
        );
        let quoted = parsed_read(&["--tagma", "peer", "--quote", "7"]);
        assert!(
            reject_backward_cursor_at_non_room(&quoted).is_ok(),
            "a quote rides a direct read: it names a row, not a page edge"
        );
    }

    #[test]
    fn a_quote_lookup_asks_for_the_row_above_the_position() {
        let query = room_read_query(&parsed_read(&["--room", ROOM, "--quote", "42"]))
            .expect("quote composes");
        assert_eq!(query.after_seq, None);
        assert_eq!(query.before_seq, Some(43));
        assert_eq!(query.limit, Some(1));
    }

    #[test]
    fn a_direct_quote_lookup_asks_for_the_row_below_the_position() {
        let quoted = parsed_read(&["--tagma", "peer", "--quote", "42"]);
        let query = direct_read_query(&quoted);
        assert_eq!(query.after_seq, Some(41));
        assert_eq!(query.limit, Some(1));
        // The first row of a session sits at the floor: `after_seq = 0` reads
        // from the start, while a room's backward cursor treats 0 as no row.
        let floor = parsed_read(&["--tagma", "peer", "--quote", "1"]);
        let first = direct_read_query(&floor);
        assert_eq!(first.after_seq, Some(0));
        assert_eq!(first.limit, Some(1));
        // Without a quote the caller's own cursor and page size pass through.
        let plain = direct_read_query(&parsed_read(&[
            "--tagma", "peer", "--after", "3", "--limit", "20",
        ]));
        assert_eq!(plain.after_seq, Some(3));
        assert_eq!(plain.limit, Some(20));
    }

    #[test]
    fn a_position_with_no_room_above_it_is_rejected() {
        let args = args::lesche::ReadArgs {
            room: Some("ops".to_owned()),
            tagma: None,
            session: None,
            after_seq: None,
            before_seq: None,
            quote: Some(i64::MAX),
            limit: None,
        };
        let err = room_read_query(&args).expect_err("out of range");
        assert!(err.to_string().contains("is out of range"), "{err}");
    }

    #[test]
    fn a_backward_page_passes_its_cursor_through() {
        let query = room_read_query(&parsed_read(&["--room", ROOM, "--before", "9"]))
            .expect("backward page");
        assert_eq!(query.before_seq, Some(9));
        assert_eq!(query.after_seq, None);
        assert_eq!(query.limit, None);
        // The shape a pager actually sends: the cursor plus a page size.
        let sized = room_read_query(&parsed_read(&[
            "--room", ROOM, "--before", "9", "--limit", "20",
        ]))
        .expect("sized backward page");
        assert_eq!(sized.before_seq, Some(9));
        assert_eq!(sized.limit, Some(20));
        let plain = room_read_query(&parsed_read(&["--room", ROOM])).expect("plain page");
        assert_eq!(plain.after_seq, None);
        assert_eq!(plain.before_seq, None);
        assert_eq!(plain.limit, None);
    }

    #[test]
    fn a_quoted_read_prints_the_named_row_and_reports_its_absence() {
        let block = "[seq=42 from=agent:00000000-0000-4000-8000-0000000000a1 \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n";
        assert_eq!(
            expect_named_block(block.to_owned(), 42).expect("named row"),
            block
        );
        // The leading block alone decides the row: a page whose first block
        // names a longer seq must fail, and so must one whose body merely
        // carries the named seq.
        for (text, seq) in [
            (String::new(), 42),
            (
                "[seq=41 from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n".to_owned(),
                42,
            ),
            ("not a block".to_owned(), 42),
            (
                "[seq=42 from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n".to_owned(),
                4,
            ),
            (
                "[seq=41 from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nlook:\n[seq=42 from=agent:00000000-0000-4000-8000-0000000000b2 at=2026-01-01 0:00:00.0 +00:00:00]\n\n".to_owned(),
                42,
            ),
        ] {
            let err = expect_named_block(text.clone(), seq).expect_err("unnamed row");
            assert!(
                err.to_string()
                    .contains(&format!("no readable row at seq {seq}")),
                "{err}"
            );
        }
    }

    /// The check a `--quote` read runs takes its opening from the renderer's
    /// own source: a header built with it names the row the read asked for, and
    /// the row beside it -- the page a missing position lands on -- does not.
    #[test]
    fn a_rendered_header_names_the_row_the_read_asked_for() {
        let named = format!(
            "{}from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n",
            kallipai_common::readback::block_prefix(42)
        );
        assert_eq!(
            expect_named_block(named.clone(), 42).expect("named row"),
            named
        );
        let beside = format!(
            "{}from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n",
            kallipai_common::readback::block_prefix(41)
        );
        assert!(
            expect_named_block(beside, 42).is_err(),
            "the row beside the position is another row"
        );
    }

    #[test]
    fn a_quoted_read_prints_a_row_whose_body_looks_like_a_block() {
        let pasted = "[seq=99 from=agent:00000000-0000-4000-8000-0000000000b2 \"Bob\" at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n";
        let block = format!(
            "[seq=42 from=agent:00000000-0000-4000-8000-0000000000a1 \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00]\nlook:\n{pasted}"
        );
        assert_eq!(
            expect_named_block(block.clone(), 42).expect("a body quoting a block"),
            block
        );
    }

    #[test]
    fn a_quote_rides_a_room_or_direct_send() {
        assert_eq!(
            send_quote(&parsed_send(&["--room", ROOM, "--quote", "7"])).expect("room send"),
            Some(7)
        );
        assert_eq!(
            send_quote(&parsed_send(&["--tagma", "peer", "--quote", "7"]))
                .expect("direct-session send"),
            Some(7)
        );
        assert_eq!(send_quote(&parsed_send(&[])).expect("bilateral send"), None);
        let err = send_quote(&parsed_send(&["--quote", "7"])).expect_err("bilateral send");
        assert!(
            err.to_string().contains("rides a room or direct send"),
            "{err}"
        );
    }

    #[test]
    fn mentions_ride_a_room_send_only() {
        let sent = send_mentions(&parsed_send(&[
            "--room",
            "ops",
            "--mention",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "--mention",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        ]))
        .expect("room send");
        assert_eq!(
            sent,
            vec![
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_string(),
            ],
            "the flags collect in the order they were given"
        );

        assert!(
            send_mentions(&parsed_send(&[]))
                .expect("plain send")
                .is_empty(),
            "a send that names nobody names nobody"
        );

        let err = send_mentions(&parsed_send(&["--tagma", "peer", "--mention", "not-an-id"]))
            .expect_err("a direct-session send names no room");
        assert!(err.to_string().contains("rides a room send"), "{err}");
    }

    #[test]
    fn a_mention_and_a_quote_travel_together() {
        let args = parsed_send(&[
            "--room",
            "ops",
            "--quote",
            "7",
            "--mention",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        ]);
        assert_eq!(send_quote(&args).expect("room send"), Some(7));
        assert_eq!(
            send_mentions(&args).expect("room send"),
            vec!["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string()]
        );
    }

    #[test]
    fn lesche_send_rejects_a_position_below_one() {
        assert!(
            Cli::try_parse_from(["kallip", "lesche", "send", "--room", ROOM, "--quote", "0"])
                .is_err()
        );
    }
}
