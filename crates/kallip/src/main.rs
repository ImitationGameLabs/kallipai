//! kallip: tagma client CLI.

mod args;

use anyhow::Result;
use clap::Parser;
use kallip_client::TagmaClient;
use kallip_common::agentid::AgentId;
use kallip_common::policy::{ExecDecision, ExecOverride};
use kallip_common::tokens::parse_token_amount;

use args::{
    AgentCommand, ApprovalCommand, BudgetCommand, Cli, Commands, DirlockCommand, LescheCommand,
    PolicyCommand, SkillCommand, SubagentCommand,
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
    let client = TagmaClient::from_env()?;

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
        Commands::Lesche(cmd) => match cmd {
            LescheCommand::Send(args) => {
                // Self-only: send as the calling agent (KALLIP_ID). The text is
                // the positional arg, or stdin (multiline) when omitted. Deliver
                // via the tagma's relay first, then print the stable marker only
                // on success — so a failed POST (relay down, burst cap, etc.)
                // does not let local clients render a message that was never
                // delivered.
                let id = agent_id_from_env()?;
                let text = match args.text {
                    Some(t) => t,
                    None => {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                        buf
                    }
                };
                client.post_message_delivery(&id, &text).await?;
                println!("{}", kallip_common::message::marker_line(&text));
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
            SkillCommand::Index(args) => {
                println!("{}", render_skill_index(&args.path)?);
            }
            SkillCommand::Meta(args) => {
                let meta = read_skill_meta(&args.path)?;
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

/// A row in the generated skill index.
struct IndexRow {
    name: String,
    description: Option<String>,
    is_category: bool,
}

/// Read one skill's metadata directly from its file. Accepts either the stem
/// (`<skills>/agent/kallip`) or the full `<skills>/agent/kallip.md`. Falls back
/// to the file stem for `name` when the file has no frontmatter, mirroring the
/// old server-side `skill_metadata` default.
fn read_skill_meta(path: &std::path::Path) -> Result<kallip_common::protocol::SkillMeta> {
    use anyhow::Context;
    use kallip_common::protocol::parse_frontmatter;

    let file = match path.extension().and_then(|e| e.to_str()) {
        None => path.with_extension("md"),
        Some(ext) if ext.eq_ignore_ascii_case("md") => path.to_path_buf(),
        Some(other) => {
            anyhow::bail!(
                "skill path must be a stem or `.md`, got `.{other}`: {}",
                path.display()
            );
        }
    };
    let content = std::fs::read_to_string(&file)
        .with_context(|| format!("failed to read skill {}", file.display()))?;
    Ok(parse_frontmatter(&content).unwrap_or_else(|| {
        let name = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_owned();
        kallip_common::protocol::SkillMeta {
            name,
            description: None,
        }
    }))
}
/// Generate the skill index for `dir` as a linear section list (one `##`
/// heading per entry), read directly from the filesystem — optimized for LLM
/// consumption and pinning (the agent runs this once and pins the output).
///
/// Each `.md` file (except `README.md`/`index.md`) becomes a skill section from
/// its frontmatter; each subdirectory becomes a category section whose name is
/// the directory name (the navigable identifier) and whose description comes
/// from its `README.md` frontmatter. Categories sort first (top-down
/// navigation), then skills, each alphabetical. Symlinks are followed
/// (`fs::metadata`), so a symlink to a `.md` counts as a skill and a symlink to
/// a dir as a category.
///
/// Unreadable entries are skipped uniformly (dangling symlink, unreadable
/// README, unreadable skill `.md`) so one bad file never bricks the whole
/// listing. `index.md` is explicitly skipped so stale copies in already-deployed
/// data dirs (the seed never clobbers a non-empty target) stay invisible — they
/// never pollute the generated index as bogus skills.
fn render_skill_index(dir: &std::path::Path) -> Result<String> {
    use kallip_common::protocol::{parse_frontmatter, parse_frontmatter_description};
    use std::fs;

    let mut rows: Vec<IndexRow> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // `fs::metadata` follows symlinks; `DirEntry::file_type` would not.
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue, // dangling symlink / unreadable — skip
        };
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        // Skip dotfiles/dot-dirs (`.git/`, `.draft.md`, editor droppings) —
        // they are not skills or categories.
        if file_name.starts_with('.') {
            continue;
        }

        if meta.is_dir() {
            // Category: name = directory (navigable), description from README.md.
            let description = fs::read_to_string(path.join("README.md"))
                .ok()
                .and_then(|c| parse_frontmatter_description(&c));
            rows.push(IndexRow {
                name: file_name.into_owned(),
                description,
                is_category: true,
            });
        } else if meta.is_file() && has_md_extension(&file_name) {
            // Skip the dir's own README.md and any stale index.md.
            if file_name == "README.md" || file_name == "index.md" {
                continue;
            }
            // Unreadable skill file — skip (uniform with README + symlink paths).
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // The skill's navigable identifier is its file stem (the path
            // component, used for `kallip skill meta` and file reads); the
            // frontmatter `name` is only a display label, not shown here.
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("skill")
                .to_owned();
            let description = parse_frontmatter(&content).and_then(|m| m.description);
            rows.push(IndexRow {
                name,
                description,
                is_category: false,
            });
        }
    }

    // Categories first, then skills; each alphabetical (case-insensitive).
    rows.sort_by(|a, b| {
        b.is_category
            .cmp(&a.is_category)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    // Linear layout: a self-describing H1 (the blob is pinned into context),
    // then one `##` section per entry. The trailing `/` distinguishes a
    // category from a skill; a missing description leaves the heading alone so
    // the agent still sees the entry exists.
    let mut out = format!("# Skill index for `{}`\n\n", dir.display());
    for row in rows {
        let display = if row.is_category {
            format!("{}/", row.name)
        } else {
            row.name.clone()
        };
        out.push_str(&format!("## `{display}`\n"));
        if let Some(desc) = row.description {
            out.push_str(desc.trim_end());
            out.push_str("\n\n");
        } else {
            out.push('\n');
        }
    }
    Ok(out)
}

/// Case-insensitive `.md` extension check, so a skill named `kallip.MD` is
/// treated the same as `kallip.md` by both `skill index` and `skill meta`.
fn has_md_extension(name: &str) -> bool {
    name.len() >= 3 && name[name.len() - 3..].eq_ignore_ascii_case(".md")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a temp skill fixture: two skills, a category subdir with a
    /// README, a stale index.md, and a symlink to a skill.
    fn build_fixture() -> tempfile::TempDir {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("alpha.md"),
            // frontmatter `name` deliberately differs from the file stem — the
            // index must show the stem (navigable), not the display label.
            "---\nname: Alpha Display\ndescription: first skill\n---\nbody\n",
        )
        .unwrap();
        fs::write(
            root.join("beta.md"),
            "---\nname: beta\ndescription: second skill\n---\nbody\n",
        )
        .unwrap();
        // A skill with no frontmatter — name falls back to the file stem.
        fs::write(root.join("plain.md"), "no frontmatter\n").unwrap();

        let sub = root.join("agent");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("README.md"),
            "---\ndescription: Agent self-management\n---\n# Agent Skills\n",
        )
        .unwrap();

        // Stale deployed index.md — must NOT appear as a skill.
        fs::write(root.join("index.md"), "---\nname: Skill Index\n---\nold\n").unwrap();

        // Symlink to a skill — followed, lists as a skill.
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("alpha.md"), root.join("linked.md")).unwrap();

        dir
    }

    #[test]
    fn render_skill_index_lists_skills_and_categories() {
        let fixture = build_fixture();
        let out = render_skill_index(fixture.path()).unwrap();

        // Self-describing H1 names the directory (the blob is pinned as-is).
        assert!(
            out.starts_with(&format!(
                "# Skill index for `{}`\n\n",
                fixture.path().display()
            )),
            "linear index must start with a self-describing H1: {out}"
        );

        // Category first (top-down): `##` heading with trailing /, then its
        // README description as the body paragraph.
        let agent_pos = out
            .find("## `agent/`\nAgent self-management\n")
            .expect("agent category section");
        // Skills appear under their FILE STEM (navigable), not the frontmatter
        // display name, with the description from frontmatter as the body.
        let alpha_pos = out
            .find("## `alpha`\nfirst skill\n")
            .expect("alpha skill section with description");
        assert!(
            agent_pos < alpha_pos,
            "category must sort before skills: {out}"
        );
        assert!(
            !out.contains("Alpha Display"),
            "frontmatter display name must not appear: {out}"
        );
        assert!(out.contains("## `beta`\nsecond skill\n"));
        // No-frontmatter skill: heading only, no description body.
        assert!(
            out.contains("## `plain`\n\n"),
            "no-description skill must be heading-only: {out}"
        );

        // Stale index.md and the dir's own README.md are NOT listed as skills.
        assert!(!out.contains("Skill Index"));
        assert!(!out.contains("README"));
    }

    #[test]
    fn read_skill_meta_accepts_stem_and_md_path() {
        let fixture = build_fixture();
        let alpha = fixture.path().join("alpha.md");
        // Stem form (no .md) and full .md form resolve to the same file.
        let by_stem = read_skill_meta(&alpha.with_extension("")).unwrap();
        let by_file = read_skill_meta(&alpha).unwrap();
        assert_eq!(by_stem.name, "Alpha Display");
        assert_eq!(by_stem.description.as_deref(), Some("first skill"));
        assert_eq!(by_stem.name, by_file.name);

        // A file with no frontmatter falls back to the file stem as `name`.
        let plain = read_skill_meta(&fixture.path().join("plain")).unwrap();
        assert_eq!(plain.name, "plain");
        assert!(plain.description.is_none());

        // A missing file errors.
        assert!(read_skill_meta(&fixture.path().join("nope")).is_err());
        // A non-`.md` extension is rejected with a clear error (not used as-is).
        assert!(read_skill_meta(&fixture.path().join("alpha.txt")).is_err());
    }

    #[test]
    fn has_md_extension_is_case_insensitive() {
        assert!(has_md_extension("kallip.md"));
        assert!(has_md_extension("kallip.MD"));
        assert!(has_md_extension("kallip.Md"));
        assert!(!has_md_extension("kallip.txt"));
        assert!(!has_md_extension("kallipmarkdown"));
        // An empty string is not a `.md` file.
        assert!(!has_md_extension(""));
    }

    /// The shipped category READMEs are load-bearing seed content for the
    /// generated index — a frontmatter typo would otherwise ship an empty
    /// category row silently. Pin that both parse to a non-empty description.
    #[test]
    fn shipped_category_readmes_parse_to_description() {
        use kallip_common::protocol::parse_frontmatter_description;
        use std::fs;

        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for category in ["agent", "code"] {
            let readme = repo_root.join("skills").join(category).join("README.md");
            let content = fs::read_to_string(&readme)
                .unwrap_or_else(|e| panic!("shipped {category}/README.md must exist: {e}"));
            assert!(
                parse_frontmatter_description(&content).is_some_and(|d| !d.trim().is_empty()),
                "shipped skills/{category}/README.md must carry a non-empty frontmatter description"
            );
        }
    }
}
