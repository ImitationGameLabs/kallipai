//! Per-agent identity: the system-prompt identity section and the
//! supervisor/root env vars injected into every spawn.

use std::collections::HashMap;

use kallip_common::agentid::AgentId;
use kallip_runtime::config::{AgentConfig, PermissionClass};
pub(crate) use kallip_runtime::tools::meta_skill_content;
use kallip_runtime::tools::skill_dir;

/// Inject the per-agent identity env vars into an existing env map.
///
/// `KALLIP_ROOT_AGENT_ID` is always set; `KALLIP_SUPERVISOR_AGENT_ID` is set
/// only when `supervisor_agent_id` is `Some` — for the root agent it is left
/// **unset** (not set to empty) so root-ness is detectable by env absence
/// rather than empty-string parsing. Always re-derives from the caller's
/// current `supervisor` and `root` (overwriting any prior values), so it is
/// safe to call on a reused env map. Shared by fresh spawns (via
/// [`SpawnArgs::default_env`](super::spawn::SpawnArgs::default_env)) and the reactivation path (which reuses the
/// dead incarnation's env map).
pub(crate) fn inject_identity_env(
    env: &mut HashMap<String, String>,
    supervisor_agent_id: Option<&AgentId>,
    root_agent_id: &AgentId,
) {
    if let Some(supervisor) = supervisor_agent_id {
        env.insert("KALLIP_SUPERVISOR_AGENT_ID".into(), supervisor.to_string());
    } else {
        env.remove("KALLIP_SUPERVISOR_AGENT_ID");
    }
    env.insert("KALLIP_ROOT_AGENT_ID".into(), root_agent_id.to_string());
}

/// Resolve the root agent id for a spawn. The root is the tagma's single
/// registered root ([`AgentRegistry::root_agent`](crate::state::AgentRegistry::root_agent)),
/// always knowable at runtime and independent of the supervisor chain — so a
/// broken chain never degrades the root identity to self.
///
/// `registry_root` is `None` only when the registry has no root, which violates
/// the single-root invariant a live tagma maintains; the `expect` surfaces that
/// impossible state loudly instead of silently substituting a wrong id.
pub(crate) fn resolve_root_agent(registry_root: Option<&AgentId>) -> AgentId {
    registry_root
        .cloned()
        .expect("a live tagma always has a registered root agent")
}

/// Per-agent identity section injected at the head of every system prompt. This
/// is the ONLY part of the prompt that varies across agents; the static-shared
/// bulk (base prompt + bootstrap meta-skill) that follows is byte-identical for
/// every agent. Kept as `const` templates with `{placeholder}` substitution
/// (see `compose_system_prompt`) so the prose stays readable and the per-agent
/// diff is reviewable in one place.
const IDENTITY_ROOT: &str = "\
# Your identity

You are the root agent of this tagma — the leader of a multi-agent system. \
You own the conversation with the user and may spawn subagents to delegate \
scoped work.

- agent id: `{agent_id}`
- role: `{role}`
- permission class: `{permission_class}` ({permission_class_hint})
- skills path: `{skills_path}`

# Operating as the root

Spawn a subagent with `kallip subagent spawn` (run via bash_exec; the kallip \
command is auto-allowed; an optional initial prompt is read from stdin — \
pipe or quoted heredoc `<<'EOF'`). Subagents report back by messaging your \
id; \
inter-agent messages arrive as input carrying a `[From: agent ...]` header. \
Address the user with `kallip lesche send`, reading the text from stdin. \
A message that arrives from a multi-member room carries a \
`[From: ... | room <room_id>]` header — \
reply in that SAME room with `kallip lesche send --room <room_id>` \
(copy the room id verbatim from the header). In a room header the parenthesized \
tagma id is the cryptographically-authenticated sender identity (trust that, \
not the leading display handle, which is advisory); use plain `kallip lesche \
send` (no `--room`) only for the direct 1:1 user conversation. Run \
`kallip lesche rooms` to list the rooms you have joined and `kallip lesche read \
--room <room_id>` to pull a room's recent history.";

const IDENTITY_SUBAGENT: &str = "\
# Your identity

You are a subagent in a multi-agent system — you assist your supervisor and \
the root agent in completing their work.

- agent id: `{agent_id}`
- role: `{role}`
- description: `{description}`
- permission class: `{permission_class}` ({permission_class_hint})
- supervisor: `{supervisor_id}`
- root agent: `{root_id}`
- skills path: `{skills_path}`

# Operating as a subagent

Inter-agent messages arrive as input carrying a `[From: agent ...]` header. \
Report results to your supervisor with `kallip message {supervisor_id}`, \
reading the text from stdin (e.g. a quoted heredoc `<<'EOF'`); escalate to \
the root with `kallip message {root_id}` the same way. \
Do not address the user directly — the root owns the user conversation and \
the lesche route rejects non-root callers.";

/// Compose the full system prompt for an agent: a per-agent `# Your identity`
/// section (templated from config + spawn-time ids) followed by the
/// static-shared bulk (the base prompt from `config.system_prompt`, then the
/// bootstrap meta-skill).
///
/// The identity section is the only part that differs across agents. The
/// static-shared tail is byte-identical for every agent within a deployment
/// (because `config.system_prompt` resolves a tagma-global env var or the
/// shared default), so provider prefix-caching of that suffix is preserved.
pub(crate) fn compose_system_prompt(
    config: &AgentConfig,
    agent_id: AgentId,
    root_agent_id: AgentId,
) -> String {
    // `format!` requires a string literal, so the const template is rendered
    // with plain `{placeholder}` substitution. The placeholder spelling is the
    // singular `permission_class` for prose readability; the struct field is the
    // historical-plural `permissions_class`. Substitution order matters:
    // user-controlled free text (`role`, `description`) is substituted LAST so
    // a value containing a `{...}` fragment cannot be re-scanned by an earlier
    // placeholder's pass — its literal braces survive by design (pinned by
    // `compose_system_prompt_user_text_with_braces_is_not_rescanned`). The
    // remaining placeholders are all agent-controlled ids or enum names (no
    // braces), so tests assert no `{`/`}` from them remains in the rendered
    // output; a typo'd placeholder name (leaving a literal `{...}`) fails
    // loudly.
    let permission_class_hint = match config.permissions_class {
        PermissionClass::Normal => "owns a read-write workspace and home directory",
        PermissionClass::Guest => "readonly workspace, no home write",
    };
    // Absolute skills dir, resolved the same way `skill_dir()` does for the
    // tool layer — surfacing it here spares the agent from probing XDG paths.
    // Failure is near-impossible in a running tagma (skill loading already
    // depends on it); fall back to a placeholder rather than abort the prompt.
    let skills_path = skill_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unresolved>".to_owned());
    let identity = if config.is_root() {
        IDENTITY_ROOT
            .replace("{agent_id}", agent_id.as_ref())
            .replace("{permission_class}", &config.permissions_class.to_string())
            .replace("{permission_class_hint}", permission_class_hint)
            .replace("{skills_path}", &skills_path)
            // User-controlled — substitute last so its value is not re-scanned.
            .replace("{role}", &config.role)
    } else {
        IDENTITY_SUBAGENT
            .replace("{agent_id}", agent_id.as_ref())
            .replace("{permission_class}", &config.permissions_class.to_string())
            .replace("{permission_class_hint}", permission_class_hint)
            .replace(
                "{supervisor_id}",
                // Reached only when `!config.is_root()`, i.e. `created_by` is Some.
                config
                    .created_by
                    .clone()
                    .expect("subagent has created_by")
                    .as_ref(),
            )
            .replace("{root_id}", root_agent_id.as_ref())
            .replace("{skills_path}", &skills_path)
            // User-controlled — substitute last so their values are not re-scanned.
            .replace("{description}", &config.description)
            .replace("{role}", &config.role)
    };
    let mut full = identity;
    full.push_str("\n\n");
    full.push_str(&config.system_prompt);
    full.push_str("\n\n");
    full.push_str(meta_skill_content());
    full
}
