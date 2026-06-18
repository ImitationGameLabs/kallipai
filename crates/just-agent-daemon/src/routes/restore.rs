//! Agent persistence restoration.
//!
//! Restores persisted agents top-down, level by level. Root agents
//! (no supervisor) are restored first, then their children, and so on.
//! Siblings within each level are restored concurrently. If an agent fails
//! to restore, its entire subtree is skipped — no orphans are created.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context as _;
use just_agent_common::agentid::AgentId;
use just_agent_common::policy::ToolPolicy;
use just_agent_runtime::config::AgentConfig;
use just_agent_runtime::persistence;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::agent::{SpawnArgs, spawn_agent};
use crate::state::{Agent, AgentEntry, SharedState};

/// One node in a supervisor chain, fully loaded from disk.
struct ChainNode {
    agent_id: AgentId,
    meta: persistence::AgentMeta,
    policy: ToolPolicy,
}

/// Pre-loaded data for all agents being restored.
/// Eliminates redundant disk reads during supervisor chain validation
/// by caching meta and policy loaded during the scan phase.
struct RestoreIndex {
    meta: HashMap<AgentId, persistence::AgentMeta>,
    policy: HashMap<AgentId, ToolPolicy>,
}

impl RestoreIndex {
    /// Look up agent metadata. Falls back to disk read on cache miss.
    fn get_meta(&self, id: &AgentId) -> anyhow::Result<persistence::AgentMeta> {
        match self.meta.get(id) {
            Some(m) => Ok(m.clone()),
            None => persistence::read_meta(id),
        }
    }

    /// Look up tool policy. Falls back to disk read on cache miss.
    fn get_policy(&self, id: &AgentId) -> anyhow::Result<ToolPolicy> {
        match self.policy.get(id) {
            Some(p) => Ok(p.clone()),
            None => {
                let dir = persistence::agent_dir(id).context("cannot resolve agent dir")?;
                persistence::load_policy(&dir).context("failed to load policy")
            }
        }
    }
}

/// Walk the supervisor chain starting from `supervisor_id`, resolving each
/// ancestor via the pre-loaded index (with transparent disk fallback on miss).
/// Returns nodes ordered from immediate supervisor to the root.
/// Fails on missing data or circular chains.
fn load_supervisor_chain(
    supervisor_id: &AgentId,
    index: &RestoreIndex,
) -> anyhow::Result<Vec<ChainNode>> {
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut current_id = supervisor_id.clone();

    loop {
        if !visited.insert(current_id.clone()) {
            anyhow::bail!("circular supervisor chain detected");
        }

        let meta = index
            .get_meta(&current_id)
            .context("incomplete supervisor chain")?;
        let policy = index
            .get_policy(&current_id)
            .context("cannot load supervisor policy")?;

        let parent_id = meta.created_by.clone();
        chain.push(ChainNode {
            agent_id: current_id,
            meta,
            policy,
        });

        match parent_id {
            Some(pid) => current_id = pid,
            None => break,
        }
    }

    Ok(chain)
}

/// Compute remaining delegation depth from a pre-loaded supervisor chain.
fn validate_depth_from_chain(
    workspace_root: &std::path::Path,
    chain: &[ChainNode],
) -> anyhow::Result<u8> {
    let supervisor = chain.first().context("subagent has no supervisor chain")?;

    if !workspace_root.starts_with(&supervisor.meta.workspace_root) {
        anyhow::bail!("workspace outside supervisor boundary");
    }

    let chain_depth = u8::try_from(chain.len()).unwrap_or(u8::MAX);
    Ok(just_agent_runtime::config::DEFAULT_MAX_DEPTH.saturating_sub(chain_depth))
}

/// Validate that `policy` is at least as strict as every ancestor's policy
/// in the pre-loaded chain. Checks adjacent-pair monotonicity to match the
/// original recursive semantics.
fn validate_policy_from_chain(
    agent_id: &AgentId,
    policy: &ToolPolicy,
    chain: &[ChainNode],
) -> anyhow::Result<()> {
    // Agent's policy must be >= immediate supervisor's policy.
    if let Some(supervisor) = chain.first() {
        policy
            .validate_at_least_as_strict_as(&supervisor.policy)
            .map_err(|violations| {
                anyhow::anyhow!(
                    "agent {agent_id}: policy is less strict than supervisor: {}",
                    violations.join("; ")
                )
            })?;
    }

    // Chain monotonicity: each ancestor's policy must be >= its own supervisor's.
    for window in chain.windows(2) {
        window[0]
            .policy
            .validate_at_least_as_strict_as(&window[1].policy)
            .map_err(|violations| {
                anyhow::anyhow!(
                    "agent {}: policy is less strict than supervisor: {}",
                    window[0].agent_id,
                    violations.join("; ")
                )
            })?;
    }

    Ok(())
}

/// Restore a single persisted agent to a running agent.
async fn restore_one(
    p: persistence::PendingRestore,
    shutdown: CancellationToken,
    shared_state: SharedState,
    index: &RestoreIndex,
) -> anyhow::Result<(AgentId, String, Agent)> {
    let restored = persistence::restore_agent(&p.agent_id, &p.agent_dir)?;

    let mut config = AgentConfig::load(None, vec![], Some(p.meta.workspace_root.clone()))?;
    config.agent_id = Some(p.agent_id.clone());
    config.created_by = p.meta.created_by.clone();

    let tool_policy = index
        .get_policy(&p.agent_id)
        .context("failed to load policy")?;

    if let Some(ref supervisor_id) = p.meta.created_by {
        let chain = load_supervisor_chain(supervisor_id, index)?;
        config.permissions.max_depth = validate_depth_from_chain(&p.meta.workspace_root, &chain)?;
        validate_policy_from_chain(&p.agent_id, &tool_policy, &chain)?;
    }

    // Resolve the model tier purely by depth (positional tiers — no persisted binding). Warn if
    // the agent's depth exceeds the tier list: it clamps to the lowest-capability tier.
    let depth = config.permissions.depth();
    let tier_count = shared_state.profiles.tiers().len();
    if depth >= tier_count {
        tracing::warn!(
            depth,
            tier_count,
            "agent depth exceeds tier count; clamping to the lowest tier"
        );
    }
    let tier = shared_state.profiles.select_profile(depth).clone();

    let store = Arc::new(tokio::sync::Mutex::new(restored.store));
    let approvals = Arc::new(tokio::sync::Mutex::new(restored.approvals));
    let (events_tx, _) = broadcast::channel(256);

    let auth_token = uuid::Uuid::new_v4().to_string();
    let env = SpawnArgs::default_env(&p.agent_id, &auth_token);

    let tool_policy = Arc::new(std::sync::RwLock::new(tool_policy));

    let agent = spawn_agent(SpawnArgs {
        agent_id: p.agent_id.clone(),
        store,
        approvals,
        agent_dir: restored.agent_dir,
        config,
        initial_prompt: None,
        shutdown_cancel: shutdown,
        events_tx,
        auth_token: auth_token.clone(),
        env,
        shared_state: shared_state.clone(),
        tool_policy,
        prompt_queue_size: shared_state.prompt_queue_size,
        prompt_channel: None,
        tier,
    })
    .await?;

    Ok((restored.agent_id, auth_token, agent))
}

/// Restore persisted agents top-down, level by level.
///
/// Root agents (no supervisor) are restored first, then their children, and
/// so on.  Siblings within each level are restored concurrently.  If an agent
/// fails to restore, its entire subtree is skipped — no orphans are created.
///
/// **Exempt from resource limits:** `max_agents` and `max_subagents` are not
/// enforced during restore. These agents were already running before the crash,
/// so refusing to restore them would be counterproductive. After restore,
/// `registry.len()` may exceed `max_agents`; new creation returns 503 until
/// agents are removed to make room.
pub async fn restore_agents(state: &SharedState) {
    let pending = persistence::scan_agents();
    if pending.is_empty() {
        return;
    }

    info!(count = pending.len(), "restoring agents");

    // Build index: meta from scan, policy loaded once per agent.
    let mut meta_map = HashMap::new();
    let mut policy_map = HashMap::new();
    for p in &pending {
        meta_map.insert(p.agent_id.clone(), p.meta.clone());
        if let Ok(policy) = persistence::load_policy(&p.agent_dir) {
            policy_map.insert(p.agent_id.clone(), policy);
        }
    }
    let index = RestoreIndex {
        meta: meta_map,
        policy: policy_map,
    };

    // Build restore tree from created_by relationships.
    let pending_set: HashSet<AgentId> = pending.iter().map(|p| p.agent_id.clone()).collect();
    let mut pending_map: HashMap<AgentId, persistence::PendingRestore> = pending
        .into_iter()
        .map(|p| (p.agent_id.clone(), p))
        .collect();

    let mut children_of: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
    let mut roots = Vec::new();
    let mut direct_skips = Vec::new();

    for (id, p) in &pending_map {
        match &p.meta.created_by {
            None => {
                roots.push(id.clone());
            }
            Some(supervisor_id) if pending_set.contains(supervisor_id) => {
                children_of
                    .entry(supervisor_id.clone())
                    .or_default()
                    .push(id.clone());
            }
            Some(supervisor_id) => {
                // Supervisor not in restore set (crash-loop or removed).
                // This agent and its descendants will not be restored.
                tracing::error!(
                    id = %id,
                    supervisor = %supervisor_id,
                    "skipping agent: supervisor not in restore set"
                );
                direct_skips.push(id.clone());
            }
        }
    }

    // Remove directly-skipped agents so the post-BFS pass does not double-log.
    for id in &direct_skips {
        pending_map.remove(id);
    }

    // Deterministic ordering within each level.
    roots.sort();

    // Level-by-level BFS restore.  Siblings within each level are restored
    // concurrently; children are only queued after their parent succeeds.
    let mut current_level = roots;
    while !current_level.is_empty() {
        // Take ownership of PendingRestores for this level.
        let tasks: Vec<(AgentId, persistence::PendingRestore)> = current_level
            .iter()
            .filter_map(|id| pending_map.remove(id).map(|p| (id.clone(), p)))
            .collect();

        // Restore all siblings concurrently.
        type RestoreOutcome = (AgentId, String, Agent);
        let results: Vec<(AgentId, anyhow::Result<RestoreOutcome>)> =
            futures_util::future::join_all(tasks.into_iter().map(|(id, p)| async {
                let result = restore_one(p, state.shutdown.clone(), state.clone(), &index).await;
                (id, result)
            }))
            .await;

        // Batch-register successes under a single lock, collect children.
        let mut next_level = Vec::new();
        let mut successes = Vec::new();
        for (id, result) in results {
            match result {
                Ok((registered_id, auth_token, agent)) => {
                    successes.push((registered_id, auth_token, agent));
                    if let Some(children) = children_of.get(&id) {
                        next_level.extend(children.iter().cloned());
                    }
                    info!(id = %id, "restored agent");
                }
                Err(e) => {
                    // Subtree is implicitly pruned — children not queued.
                    tracing::error!(id = %id, "restore failed: {e:#}");
                }
            }
        }

        if !successes.is_empty() {
            let mut registry = state.registry.write().await;
            for (id, auth_token, agent) in successes {
                registry.register(
                    id,
                    auth_token,
                    AgentEntry {
                        agent,
                        subagent_ids: vec![],
                    },
                );
            }
        }

        next_level.sort();
        current_level = next_level;
    }

    // Log transitively skipped agents (ancestors failed or cycles).
    for (id, p) in &pending_map {
        tracing::error!(
            id = %id,
            supervisor = ?p.meta.created_by,
            "skipping agent: ancestor was not restored"
        );
    }

    // Warn if restored agents exceed configured limits.
    {
        let registry = state.registry.read().await;
        if registry.len() > state.max_agents {
            tracing::warn!(
                count = registry.len(),
                max = state.max_agents,
                "restored agent count exceeds max_agents; new creation will return 503 until agents are removed"
            );
        }
        for (id, entry) in registry.iter() {
            if entry.subagent_ids.len() > state.max_subagents {
                tracing::warn!(
                    id = %id,
                    count = entry.subagent_ids.len(),
                    max = state.max_subagents,
                    "restored agent exceeds max_subagents limit"
                );
            }
        }
    }
}
