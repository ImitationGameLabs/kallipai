use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use just_llm_client::ToolDispatcher;
use just_llm_client::types::chat::{FunctionDefinition, ToolDefinition, ToolType};
use kallip_common::AgentId;
use kallip_common::policy::ExecPolicy;
use kallip_shell::{ShellBuilder, shell_tool_set};
use serde_json::json;
use tokio::sync::Mutex;

use crate::config::AgentConfig;
use crate::context::ContextStore;
use crate::dirlock::DirLockManager;
pub mod context;
pub mod skill;

pub use skill::{
    META_SKILL_NAME, load_skill, meta_skill_content, parse_frontmatter_meta, skill_dir,
    skill_metadata, validate_skill_name,
};

/// Inputs to [`build_tool_dispatch`], grouped to keep that function's argument
/// list readable. The daemon assembles these from its `SpawnArgs` per agent.
pub struct ToolDispatchInputs<'a> {
    /// Shared context store (same one the main loop uses).
    pub ctx: Arc<Mutex<ContextStore>>,
    /// Agent configuration (workspace, permission class, ...). Borrowed for the
    /// duration of the call.
    pub config: &'a AgentConfig,
    /// Extra environment handed to every spawned `bash` (agent id, auth token, ...).
    pub env: HashMap<String, String>,
    /// Sink for background-task terminal notices; the daemon wires it to the
    /// agent's prompt channel.
    pub notice_sink: Arc<dyn Fn(String) + Send + Sync>,
    /// bash_exec execution-policy overrides.
    pub exec_policy: Arc<RwLock<ExecPolicy>>,
    /// Inter-agent directory write-lock coordinator (drives the writable set and
    /// peers' readonly holes under landlock).
    pub lock_manager: Arc<DirLockManager>,
    /// The agent's identity (used to look up its locks in `lock_manager`).
    pub agent_id: AgentId,
    /// The agent's on-disk dir (`<data_root>/agents/<id>/`). Its `skills/`
    /// subdirectory is carved writable so the agent can author local skills
    /// (the data-dir integrity baseline).
    pub agent_dir: PathBuf,
}

/// Builds the tool registry exposed by `kallip`.
///
/// Spawns a fresh isolated `bash` per command via [`ShellBuilder`] (the one-shot
/// backend). The working directory is read fresh from `pwd` after each command and
/// reported in the tool result — it does not persist implicitly across calls. A background task
/// that finishes delivers a completion notice through `notice_sink` (the daemon wires it to the agent's
/// prompt channel, so the LLM learns without polling `bash_background_read`).
///
/// Context tools share the same `ContextStore` as the main loop.
pub async fn build_tool_dispatch(inputs: ToolDispatchInputs<'_>) -> Result<ToolDispatcher> {
    let ToolDispatchInputs {
        ctx,
        config,
        env,
        notice_sink,
        exec_policy,
        lock_manager,
        agent_id,
        agent_dir,
    } = inputs;
    let builder = ShellBuilder::new()
        .initial_cwd(config.workspace_root.clone())
        .envs(env)
        // The exit code is intentionally omitted from the notice — the agent reads it
        // (and the output) via `bash_background_read`. Keeping the notice minimal avoids
        // duplicating state the agent will fetch anyway.
        .on_terminal(move |id, state, _code| {
            notice_sink(format!("[Background task {id} {}]", state.as_str()));
        });
    // Landlock-enforce this agent's bash against its current access decision
    // (Linux + `landlock`). The closure composes the decision fresh per spawn.
    // Both classes read broadly; the class distinction is on WRITE and secret
    // visibility:
    // - **Normal**: writable = write-locks + skills carve; secrets readable
    //   (mitigated by proxy tools, design doc §4.5).
    // - **Guest**: readonly — writable is the skills carve only (no workspace
    //   write); secret dirs are hidden by mount-ns tmpfs overlays so a broad-read
    //   Guest can read source/caches (`~/.cargo`) without reaching keys/tokens.
    //
    // `readonly_holes` (peers' locked workspaces, mount-ns bind-ro) apply to both.
    //
    // The skills carve (`agent_dir/skills`) lets either class author local skills
    // while the rest of the data tree stays non-writable — the data-dir integrity
    // baseline. Only the agent's own skills dir (disjoint from peers' by `<id>`);
    // it does not grant write to `meta.json`/`context.json`/`exec_policy.toml`/etc.
    #[cfg(all(target_os = "linux", feature = "landlock"))]
    let builder = {
        let lm = lock_manager.clone();
        let aid = agent_id.clone();
        let skills_dir = agent_dir.join("skills");
        let is_guest = matches!(
            config.permissions_class,
            crate::config::PermissionClass::Guest
        );
        builder.access_source(move || {
            // Normal: write-locks + skills carve. Guest: readonly — skills carve
            // only (no write-locks), so even a lock the Guest happens to hold is
            // not writable under landlock.
            let writable = if is_guest {
                vec![skills_dir.clone()]
            } else {
                let mut w = lm.write_paths(&aid)?;
                w.push(skills_dir.clone());
                w
            };
            let readonly_holes = lm.readonly_paths(&aid)?;
            // Recomputed per spawn (not snapshotted at dispatch build): a secret
            // dir created mid-session must be masked by the next bash_exec. Same
            // per-spawn cadence as `readonly_holes` above.
            let hide_holes = if is_guest {
                guest_hide_holes()
            } else {
                Vec::new()
            };
            Ok(kallip_shell::landlock::AccessDecision {
                read: kallip_shell::landlock::ReadPolicy::Broad,
                writable,
                readonly_holes,
                hide_holes,
            })
        })
    };
    // Without the landlock feature the coordinator is advisory only; the params
    // are still threaded so the daemon API is uniform across builds.
    #[cfg(not(all(target_os = "linux", feature = "landlock")))]
    let _ = (&lock_manager, &agent_id, &agent_dir);
    let backend = builder.build().await?;
    let backend = Arc::new(Mutex::new(backend));

    let mut dispatch = ToolDispatcher::new();
    dispatch.add_tools(shell_tool_set(backend))?;
    dispatch.add_tools(context::context_tool_set(ctx, exec_policy))?;

    Ok(dispatch)
}

/// Secret directories a Guest agent's `bash` must not see — overlaid by empty
/// read-only tmpfs (hide-holes) so a broad-read Guest can read source/caches
/// without reaching keys/tokens. Normal agents get no hide-holes (their secret
/// use is via proxy tools, design doc §4.5). The returned paths populate
/// [`kallip_shell::landlock::AccessDecision::hide_holes`], which
/// `kallip-shell::landlock::apply` realizes via libsandbox's
/// `prepare_tmpfs`/`install_tmpfs` (one tmpfs overlay per path).
///
/// **Contract:** every returned path must be an existing **directory** —
/// `prepare_tmpfs` needs a mountpoint, and `mount(2)` is defined for directory
/// subtrees. The `is_dir()` filter below enforces this; file-based secrets
/// (e.g. `~/.netrc`) are **not** covered by directory hide-holes and need
/// operator action (relocation or `KALLIP_SECRET_HIDE_PATHS` of a parent
/// dir). The returned paths must also stay prefix-disjoint from
/// `DirLockManager::readonly_paths` (peer workspaces) — see the invariant on
/// `AccessDecision::hide_holes`.
///
/// The default set cannot be exhaustive (`~/.kube`, `~/.docker/config.json`,
/// `~/.netrc`, …) — operators extend it via `KALLIP_SECRET_HIDE_PATHS`, a
/// `:`-separated list appended to the defaults.
#[cfg(all(target_os = "linux", feature = "landlock"))]
fn guest_hide_holes() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let push_if_dir = |p: PathBuf, out: &mut Vec<PathBuf>| {
        if p.is_dir() {
            out.push(p);
        }
    };
    if let Some(home) = dirs::home_dir() {
        for sub in [".ssh", ".gnupg", ".aws"] {
            push_if_dir(home.join(sub), &mut out);
        }
    }
    // The daemon profiles dir (holds API keys). Reuse profile::config's resolution
    // so a custom KALLIP_PROFILES_FILE location is covered, not just the default.
    if let Some(dir) = crate::profile::config::profiles_config_dir() {
        push_if_dir(dir, &mut out);
    }
    if let Some(extras) = std::env::var_os("KALLIP_SECRET_HIDE_PATHS") {
        for part in extras.to_string_lossy().split(':') {
            if !part.is_empty() {
                push_if_dir(PathBuf::from(part), &mut out);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Approval meta-tool definitions (handled by executor, not dispatcher)
// ---------------------------------------------------------------------------

pub fn approval_list_definition() -> ToolDefinition {
    ToolDefinition {
        kind: ToolType::Function,
        function: FunctionDefinition {
            name: "approval_list".into(),
            description: Some(
                "List approval requests awaiting or having received a decision. \
                 Filter by status: pending, committed, approved, denied, redeemed, cancelled. \
                 Returns approval details including id needed for commit/redeem/cancel."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["pending", "committed", "approved", "denied", "redeemed", "cancelled", "all"],
                        "description": "Filter by status. Omit to list all."
                    }
                }
            })),
            strict: None,
        },
    }
}

pub fn approval_commit_definition() -> ToolDefinition {
    ToolDefinition {
        kind: ToolType::Function,
        function: FunctionDefinition {
            name: "approval_commit".into(),
            description: Some(
                "Submit an approval request with your justification for \
                 why this tool call is necessary. After committing, the request becomes \
                 visible to an approver. Only works on approvals with 'pending' status."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The id of the approval to commit."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Your justification for why this tool call is necessary."
                    }
                },
                "required": ["id", "reason"]
            })),
            strict: None,
        },
    }
}

pub fn approval_redeem_definition() -> ToolDefinition {
    ToolDefinition {
        kind: ToolType::Function,
        function: FunctionDefinition {
            name: "approval_redeem".into(),
            description: Some(
                "Execute a previously approved tool action. \
                 The stored tool call runs and returns its result. \
                 Only works on approvals with 'approved' status."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The id of the approval to redeem."
                    }
                },
                "required": ["id"]
            })),
            strict: None,
        },
    }
}

pub fn approval_cancel_definition() -> ToolDefinition {
    ToolDefinition {
        kind: ToolType::Function,
        function: FunctionDefinition {
            name: "approval_cancel".into(),
            description: Some(
                "Cancel an approval that is no longer needed. \
                 Works on pending, committed, approved, and denied approvals."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The id of the approval to cancel."
                    }
                },
                "required": ["id"]
            })),
            strict: None,
        },
    }
}

#[cfg(all(test, target_os = "linux", feature = "landlock"))]
mod tests {
    use super::guest_hide_holes;

    /// `guest_hide_holes` is recomputed per spawn (see `build_tool_dispatch`), so
    /// its contract — `is_dir()` filter, `KALLIP_SECRET_HIDE_PATHS` `:`-split,
    /// empty/non-existent/file entries dropped — is the invariant that mid-session
    /// secret masking rests on. Pinned in isolation: HOME/XDG are neutralized so
    /// only the env override contributes.
    #[test]
    #[serial_test::serial]
    fn guest_hide_holes_respects_is_dir_and_env_list() {
        // Neutralize the default contributors: an empty HOME (no .ssh/.gnupg/.aws)
        // and an XDG_CONFIG_HOME with no `kallip` subdir (profiles dir absent).
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        // A real secret dir, a missing path, and a regular file — only the dir
        // qualifies (mount(2) needs a directory mountpoint).
        let secret_dir = tempfile::tempdir_in(home.path()).unwrap();
        let secret_file = home.path().join("notadir");
        std::fs::write(&secret_file, b"x").unwrap();
        let missing = home.path().join("nope");

        let mut list = std::ffi::OsString::new();
        list.push(secret_dir.path());
        list.push(":");
        list.push(&missing);
        list.push(":");
        list.push(&secret_file);
        list.push(":");
        temp_env::with_vars(
            [
                ("HOME", Some(home.path().as_os_str())),
                ("XDG_CONFIG_HOME", Some(xdg.path().as_os_str())),
                ("KALLIP_SECRET_HIDE_PATHS", Some(list.as_os_str())),
            ],
            || {
                let holes = guest_hide_holes();
                assert_eq!(
                    holes,
                    vec![secret_dir.path().to_path_buf()],
                    "only the existing directory must be a hide-hole; got {holes:?}"
                );
            },
        );
    }
}
