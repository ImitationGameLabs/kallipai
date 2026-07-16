//! Agent persistence restoration.
//!
//! Restores persisted agents top-down, level by level. Root agents
//! (no supervisor) are restored first, then their children, and so on.
//! Siblings within each level are restored concurrently. An agent that fails
//! to restore (missing workspace, policy validation failure, spawn failure,
//! or an absent supervisor) is registered in a `Faulted` state instead of
//! being dropped, so the supervisor chain stays intact and the entry remains
//! listable/removable. Its children are still attempted -- an intact child
//! restores live against a faulted parent, and a broken child is itself
//! registered faulted.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context as _;
use kallip_common::agentid::AgentId;
use kallip_common::authtoken::MintedToken;
use kallip_common::policy::ExecPolicy;
use kallip_runtime::config::AgentConfig;
use kallip_runtime::persistence;
use kallip_runtime::policy::classifier;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::agent::{SpawnArgs, spawn_agent};
use crate::state::{AgentEntry, AgentIdentity, FaultedEntry, RegistryEntry, SharedState};
use crate::token::AGENT;

/// One node in a supervisor chain, fully loaded from disk.
struct ChainNode {
    agent_id: AgentId,
    meta: persistence::AgentMeta,
    exec_policy: ExecPolicy,
}

/// Pre-loaded data for all agents being restored.
/// Eliminates redundant disk reads during supervisor chain validation
/// by caching meta and exec-policy loaded during the scan phase.
struct RestoreIndex {
    meta: HashMap<AgentId, persistence::AgentMeta>,
    exec: HashMap<AgentId, ExecPolicy>,
}

impl RestoreIndex {
    /// Look up agent metadata. Falls back to disk read on cache miss.
    fn get_meta(&self, id: &AgentId) -> anyhow::Result<persistence::AgentMeta> {
        match self.meta.get(id) {
            Some(m) => Ok(m.clone()),
            None => persistence::read_meta(id),
        }
    }

    /// Look up exec policy. Falls back to disk read on cache miss (missing file
    /// yields the default empty policy).
    fn get_exec_policy(&self, id: &AgentId) -> anyhow::Result<ExecPolicy> {
        match self.exec.get(id) {
            Some(p) => Ok(p.clone()),
            None => {
                let dir = persistence::agent_dir(id).context("cannot resolve agent dir")?;
                persistence::load_exec_policy(&dir).context("failed to load exec_policy")
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
        let exec_policy = index
            .get_exec_policy(&current_id)
            .context("cannot load supervisor exec_policy")?;

        let parent_id = meta.created_by.clone();
        chain.push(ChainNode {
            agent_id: current_id,
            meta,
            exec_policy,
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
    Ok(kallip_runtime::config::DEFAULT_MAX_DEPTH.saturating_sub(chain_depth))
}

/// Validate that `exec_policy` is at least as strict as every ancestor's
/// exec policy in the pre-loaded chain, comparing *effective* decisions against
/// the static catalog baseline.
fn validate_exec_policy_from_chain(
    agent_id: &AgentId,
    exec_policy: &ExecPolicy,
    chain: &[ChainNode],
) -> anyhow::Result<()> {
    if let Some(supervisor) = chain.first() {
        exec_policy
            .validate_at_least_as_strict_as(&supervisor.exec_policy, classifier::exec_baseline)
            .map_err(|violations| {
                anyhow::anyhow!(
                    "agent {agent_id}: exec_policy is less strict than supervisor: {}",
                    violations.join("; ")
                )
            })?;
    }

    for window in chain.windows(2) {
        window[0]
            .exec_policy
            .validate_at_least_as_strict_as(&window[1].exec_policy, classifier::exec_baseline)
            .map_err(|violations| {
                anyhow::anyhow!(
                    "agent {}: exec_policy is less strict than supervisor: {}",
                    window[0].agent_id,
                    violations.join("; ")
                )
            })?;
    }

    Ok(())
}

/// Validate the restored agent's `PermissionClass` against its tier ceiling and
/// the supervisor chain (§2.3 ceiling invariant). Mirrors the policy/exec
/// validators: the agent's class must not exceed its model tier's ceiling nor
/// its immediate supervisor's, and the chain must be monotonic. This is the
/// restore-side guard against a tampered `meta.json` elevating a child above
/// its parent — depth monotonicity alone does NOT imply this (the tier 0/1 and
/// 2/3 plateaus).
fn validate_permission_class_from_chain(
    agent_id: &AgentId,
    class: kallip_runtime::config::PermissionClass,
    depth: usize,
    chain: &[ChainNode],
) -> anyhow::Result<()> {
    use kallip_runtime::config::PermissionClass;

    let ceiling = PermissionClass::ceiling_for_tier(depth);
    if class > ceiling {
        anyhow::bail!(
            "agent {agent_id}: permission class {class} exceeds its tier ceiling {ceiling}"
        );
    }
    if let Some(supervisor) = chain.first() {
        let supervisor_class = supervisor.meta.permissions_class;
        if class > supervisor_class {
            anyhow::bail!(
                "agent {agent_id}: permission class {class} exceeds supervisor's {supervisor_class}"
            );
        }
    }
    for window in chain.windows(2) {
        let (child, parent) = (
            window[0].meta.permissions_class,
            window[1].meta.permissions_class,
        );
        if child > parent {
            anyhow::bail!(
                "agent {}: permission class {child} exceeds its supervisor's {parent}",
                window[0].agent_id
            );
        }
    }
    Ok(())
}

/// Restore a single persisted agent to a running agent.
async fn restore_one(
    p: persistence::PendingRestore,
    shutdown: CancellationToken,
    shared_state: SharedState,
    index: &RestoreIndex,
) -> anyhow::Result<(AgentId, AgentEntry)> {
    let restored = persistence::restore_agent(&p.agent_id, &p.agent_dir)?;

    let mut config = AgentConfig::load(None, vec![], Some(p.meta.workspace_root.clone()))?;
    config.agent_id = Some(p.agent_id.clone());
    config.created_by = p.meta.created_by.clone();
    config.role = p.meta.role.clone();
    config.description = p.meta.description.clone();
    config.permissions_class = p.meta.permissions_class;

    // Same data-dir overlap guard as `create_agent` (bidirectional, fail-closed).
    // An agent persisted before this guard existed with an overlapping workspace
    // fails restore here; `restore_agents` then registers it `Faulted` (its
    // children are still attempted) rather than restoring it into an unsafe
    // configuration.
    persistence::ensure_workspace_disjoint(&config.workspace_root)?;

    let exec_policy = index
        .get_exec_policy(&p.agent_id)
        .context("failed to load exec_policy")?;

    // Walk the delegation ancestor chain once and reuse it both for the
    // strictness validations below and for the workspace write-lock acquire
    // (the carve-out needs the ancestor ids so a nested lock is treated as
    // delegation, not conflict). Empty for root agents.
    let supervisor_chain: Vec<ChainNode> = match p.meta.created_by.as_ref() {
        Some(supervisor_id) => load_supervisor_chain(supervisor_id, index)?,
        None => Vec::new(),
    };
    if p.meta.created_by.is_some() {
        config.permissions.max_depth =
            validate_depth_from_chain(&p.meta.workspace_root, &supervisor_chain)?;
        let depth = config.permissions.depth();
        validate_permission_class_from_chain(
            &p.agent_id,
            config.permissions_class,
            depth,
            &supervisor_chain,
        )?;
        validate_exec_policy_from_chain(&p.agent_id, &exec_policy, &supervisor_chain)?;
    }
    let chain_ids: Vec<AgentId> = supervisor_chain
        .iter()
        .map(|n| n.agent_id.clone())
        .collect();

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

    // Mint a fresh 256-bit `sk-agent-…` token. The plaintext goes into the agent shell env;
    // only its SHA-256 is indexed for auth lookup.
    let token = MintedToken::generate(AGENT);
    let env = SpawnArgs::default_env(&p.agent_id, token.secret());

    let exec_policy = Arc::new(std::sync::RwLock::new(exec_policy));

    // Acquire the workspace write-lock (Normal only) -- the same invariant
    // `create_agent` establishes. Restore used to skip this ("restart
    // semantics"), which left the workspace outside the landlock writable set
    // (every write failed with EACCES -- e.g. `cargo run` writing
    // `target/debug/.cargo-lock`) and left same-workspace mutual exclusion
    // unenforced after a daemon restart. On conflict, bail so the caller skips
    // this agent and its subtree (the same fate as an
    // `ensure_workspace_disjoint` failure). The guard's `Drop` releases the
    // lock if `spawn_agent` below fails; disarmed on success so the lock
    // persists for the agent's lifetime.
    let workspace_lock = match super::agent::try_acquire_workspace_lock(
        &shared_state,
        &p.agent_id,
        &config,
        &chain_ids,
    ) {
        Ok(guard) => guard,
        Err(super::agent::WorkspaceAcquireFailure::Busy { holder, conflict }) => {
            anyhow::bail!(
                "workspace {} overlaps a write-lock on {} held by agent {}; \
                 skipping restore",
                config.workspace_root.display(),
                conflict.display(),
                holder
            );
        }
        Err(super::agent::WorkspaceAcquireFailure::Other(e)) => {
            anyhow::bail!("failed to acquire workspace lock: {e}");
        }
    };

    let (agent, identity) = spawn_agent(SpawnArgs {
        agent_id: p.agent_id.clone(),
        store,
        approvals,
        agent_dir: restored.agent_dir,
        config,
        initial_prompt: None,
        shutdown_cancel: shutdown,
        events_tx,
        auth_token_hash: token.hash().clone(),
        env,
        shared_state: shared_state.clone(),
        preset: shared_state.preset,
        exec_policy,
        prompt_queue_size: shared_state.prompt_queue_size,
        prompt_channel: None,
        tier,
    })
    .await?;
    // Spawn succeeded: the agent owns the workspace lock for its lifetime.
    // Disarm so the guard's (imminent) Drop does not release it.
    if let Some(mut guard) = workspace_lock {
        guard.disarm();
    }

    Ok((
        restored.agent_id,
        AgentEntry {
            identity,
            agent,
            subagent_ids: vec![],
        },
    ))
}

/// Build a faulted registry entry from on-disk metadata and a failure reason.
///
/// Used when `restore_one` fails or when an agent's supervisor is absent from
/// disk. Loads NO runtime resources (no task, no channel, no store, no policy):
/// the entry exists solely for visibility and lifecycle management -- so the
/// supervisor chain stays intact and the agent stays listable/removable. Fields
/// not carried by [`persistence::AgentMeta`] fall back to
/// [`AgentConfig::default`] (irrelevant for an agent that never runs).
///
/// Deliberately does NOT call [`AgentConfig::load`]: that re-canonicalizes the
/// workspace and would re-fail for the missing-workspace case that brought us
/// here. The meta's `workspace_root` is copied as-is.
fn faulted_from_meta(
    agent_id: &AgentId,
    agent_dir: std::path::PathBuf,
    meta: &persistence::AgentMeta,
    reason: String,
) -> FaultedEntry {
    let config = AgentConfig {
        agent_id: Some(agent_id.clone()),
        created_by: meta.created_by.clone(),
        role: meta.role.clone(),
        description: meta.description.clone(),
        workspace_root: meta.workspace_root.clone(),
        permissions_class: meta.permissions_class,
        ..AgentConfig::default()
    };
    FaultedEntry {
        identity: AgentIdentity {
            config,
            agent_dir: Some(agent_dir),
        },
        subagent_ids: vec![],
        reason,
    }
}

/// Restore persisted agents top-down, level by level.
///
/// Root agents (no supervisor) are restored first, then their children, and
/// so on. Siblings within each level are restored concurrently. An agent that
/// fails to restore is registered in a `Faulted` state (not dropped), and its
/// children are still attempted -- so the supervisor chain stays intact and
/// every on-disk agent remains listable and removable.
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

    // Build index: meta from scan, exec-policy loaded once per agent.
    let mut meta_map = HashMap::new();
    let mut exec_map = HashMap::new();
    for p in &pending {
        meta_map.insert(p.agent_id.clone(), p.meta.clone());
        if let Ok(exec) = persistence::load_exec_policy(&p.agent_dir) {
            exec_map.insert(p.agent_id.clone(), exec);
        }
    }
    let index = RestoreIndex {
        meta: meta_map,
        exec: exec_map,
    };

    // Build restore tree from created_by relationships.
    let pending_set: HashSet<AgentId> = pending.iter().map(|p| p.agent_id.clone()).collect();
    let mut pending_map: HashMap<AgentId, persistence::PendingRestore> = pending
        .into_iter()
        .map(|p| (p.agent_id.clone(), p))
        .collect();

    let mut children_of: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
    let mut roots = Vec::new();
    // Agents whose supervisor is absent from disk entirely (not merely
    // restore-failed). They cannot be restored and have no live supervisor to
    // link to, so they are registered faulted up front and their descendants
    // are enqueued into the BFS so each still gets a chance to restore (or be
    // registered faulted itself).
    let mut orphan_faulted: Vec<(AgentId, FaultedEntry)> = Vec::new();

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
                // Supervisor not on disk (crash-loop-pruned, archived, or
                // removed). Register this agent faulted with its chain intact
                // so it stays individually manageable. We do NOT fabricate a
                // ghost supervisor -- there is no source-of-truth metadata for
                // one. See the plan's "Known limitation".
                tracing::error!(
                    id = %id,
                    supervisor = %supervisor_id,
                    "supervisor not present on disk; registering agent as faulted"
                );
                orphan_faulted.push((
                    id.clone(),
                    faulted_from_meta(
                        id,
                        p.agent_dir.clone(),
                        &p.meta,
                        format!("supervisor {supervisor_id} not present on disk"),
                    ),
                ));
            }
        }
    }

    // Register the supervisor-absent orphans before BFS so their descendants
    // (enqueued below) link to them via `register`'s eager subagent-push.
    // Also seed the BFS with those descendants so each is restored or
    // registered faulted, rather than vanishing with the orphan.
    let mut orphan_children = Vec::new();
    for (id, _entry) in &orphan_faulted {
        pending_map.remove(id);
        if let Some(kids) = children_of.get(id) {
            orphan_children.extend(kids.iter().cloned());
        }
    }
    if !orphan_faulted.is_empty() {
        let mut registry = state.registry.write().await;
        for (id, entry) in orphan_faulted {
            registry.register(id, RegistryEntry::Faulted(entry));
        }
    }

    // Deterministic ordering within each level.
    roots.sort();
    orphan_children.sort();

    // Level-by-level BFS restore.  Siblings within each level are restored
    // concurrently; children are queued after their parent is processed,
    // whether it restored live or faulted (a faulted parent does not imply a
    // broken child -- the child has its own workspace).
    let mut current_level = roots;
    current_level.extend(orphan_children);
    while !current_level.is_empty() {
        // Take ownership of PendingRestores for this level.
        let tasks: Vec<(AgentId, persistence::PendingRestore)> = current_level
            .iter()
            .filter_map(|id| pending_map.remove(id).map(|p| (id.clone(), p)))
            .collect();

        // Restore all siblings concurrently.
        type RestoreOutcome = (AgentId, AgentEntry);
        // Tuple slots: (agent_id, created_by, role, meta, agent_dir, result).
        // `meta` and `agent_dir` are cloned before `p` moves into `restore_one`
        // so the `Err` arm can build a faulted entry from the on-disk metadata.
        type RestoreAttempt = (
            AgentId,
            Option<AgentId>,
            String,
            persistence::AgentMeta,
            std::path::PathBuf,
            anyhow::Result<RestoreOutcome>,
        );
        let results: Vec<RestoreAttempt> =
            futures_util::future::join_all(tasks.into_iter().map(|(id, p)| {
                let created_by = p.meta.created_by.clone();
                let role = p.meta.role.clone();
                let meta = p.meta.clone();
                let agent_dir = p.agent_dir.clone();
                // Bind a reference so the `async move` block captures `&RestoreIndex`
                // (Copy) instead of moving the loop-owned `index` on every iteration.
                let index = &index;
                async move {
                    let result = restore_one(p, state.shutdown.clone(), state.clone(), index).await;
                    (id, created_by, role, meta, agent_dir, result)
                }
            }))
            .await;

        // Batch-register outcomes under a single lock, collect children.
        let mut next_level = Vec::new();
        let mut successes = Vec::new();
        let mut faulted = Vec::new();
        for (id, created_by, role, meta, agent_dir, result) in results {
            match result {
                Ok((registered_id, entry)) => {
                    successes.push((registered_id, entry));
                    if let Some(children) = children_of.get(&id) {
                        next_level.extend(children.iter().cloned());
                    }
                    info!(
                        id = %id,
                        supervisor = ?created_by,
                        role = ?role,
                        "restored agent"
                    );
                }
                Err(e) => {
                    // Register the agent faulted (not silently dropped), and
                    // STILL enqueue its children: an intact child can restore
                    // live against a faulted parent, and a broken child becomes
                    // faulted itself. Either way each node stays manageable.
                    let reason = format!("restore failed: {e:#}");
                    tracing::error!(id = %id, "{reason}; registering as faulted");
                    faulted.push((id.clone(), faulted_from_meta(&id, agent_dir, &meta, reason)));
                    if let Some(children) = children_of.get(&id) {
                        next_level.extend(children.iter().cloned());
                    }
                }
            }
        }

        if !successes.is_empty() || !faulted.is_empty() {
            let mut registry = state.registry.write().await;
            for (id, entry) in successes {
                registry.register(id, RegistryEntry::Live(entry));
            }
            for (id, entry) in faulted {
                registry.register(id, RegistryEntry::Faulted(entry));
            }
        }

        next_level.sort();
        current_level = next_level;
    }

    // Any agent still pending here was never enqueued -- only possible for a
    // cycle that defeated the BFS seed. Log it; `scan_agents` already warned
    // about unreadable data dirs, and cycles are caught as restore errors above.
    for (id, p) in &pending_map {
        tracing::error!(
            id = %id,
            supervisor = ?p.meta.created_by,
            "agent was not reached by restore (cycle or scan gap); leaving on disk"
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
            if entry.subagent_ids().len() > state.max_subagents {
                tracing::warn!(
                    id = %id,
                    count = entry.subagent_ids().len(),
                    max = state.max_subagents,
                    "restored agent exceeds max_subagents limit"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChainNode, faulted_from_meta, validate_permission_class_from_chain};
    use kallip_common::agentid::AgentId;
    use kallip_common::policy::ExecPolicy;
    use kallip_runtime::config::PermissionClass;
    use kallip_runtime::persistence::AgentMeta;

    // A supervisor chain node carrying only the fields the validator reads
    // (permissions_class) — the rest are defaulted/minimal.
    fn node(id: &str, class: PermissionClass) -> ChainNode {
        ChainNode {
            agent_id: AgentId::from(id.to_owned()),
            meta: AgentMeta {
                workspace_root: std::path::PathBuf::from("/ws"),
                last_restored_at: None,
                consecutive_restart_count: 0,
                created_by: None,
                role: String::new(),
                description: String::new(),
                permissions_class: class,
            },
            exec_policy: ExecPolicy::default(),
        }
    }

    #[test]
    fn faulted_from_meta_carries_identity_and_reason() {
        // A faulted entry is built purely from on-disk meta + a reason: it
        // carries the durable identity (so the chain stays walkable) and no
        // runtime resources. The reason is surfaced verbatim.
        let id = AgentId::from("deadbeef".to_owned());
        let meta = AgentMeta {
            workspace_root: std::path::PathBuf::from("/ws/proj"),
            last_restored_at: None,
            consecutive_restart_count: 0,
            created_by: Some(AgentId::from("parent".to_owned())),
            role: "researcher".into(),
            description: "goners".into(),
            permissions_class: PermissionClass::Guest,
        };
        let entry = faulted_from_meta(
            &id,
            std::path::PathBuf::from("/data/agents/deadbeef"),
            &meta,
            "restore failed: missing workspace".into(),
        );
        assert_eq!(entry.reason, "restore failed: missing workspace");
        assert_eq!(
            entry.identity.config.created_by.as_ref(),
            Some(&AgentId::from("parent".to_owned()))
        );
        assert_eq!(entry.identity.config.role, "researcher");
        assert_eq!(
            entry.identity.config.permissions_class,
            PermissionClass::Guest
        );
        assert_eq!(entry.identity.config.agent_id.as_ref(), Some(&id));
        assert!(entry.subagent_ids.is_empty());
    }

    #[test]
    fn restore_accepts_downgraded_subagent() {
        // A Normal-tier (depth 1, ceiling Normal) child explicitly granted Guest
        // beneath a Normal supervisor must restore cleanly — the downgrade is
        // strictly lower than both the ceiling and the supervisor's class, and the
        // chain stays monotonic. Guards the restore-side gate (zero prior coverage).
        let child = AgentId::from("child".to_owned());
        let chain = vec![node("root", PermissionClass::Normal)];
        validate_permission_class_from_chain(&child, PermissionClass::Guest, 1, &chain).unwrap();
    }

    #[test]
    fn restore_rejects_class_above_downgraded_supervisor() {
        // The restore-side mirror of the M1 tightening: a child whose granted
        // class (Normal) exceeds its downgraded supervisor's (Guest) must fail
        // restore, even though it sits at its tier ceiling.
        let child = AgentId::from("child".to_owned());
        let chain = vec![node("root", PermissionClass::Guest)];
        let err = validate_permission_class_from_chain(&child, PermissionClass::Normal, 1, &chain)
            .unwrap_err();
        assert!(err.to_string().contains("supervisor"), "{}", err);
    }

    #[test]
    fn restore_enforces_chain_monotonicity() {
        // A two-level chain where the deeper ancestor (root) was downgraded to
        // Guest but the mid node persisted at Normal (tampered meta.json). The
        // agent itself sits validly at depth 2 (ceiling Guest, class Guest), and
        // beneath its immediate supervisor (mid = Normal) — so only the
        // `chain.windows(2)` monotonicity check catches the mid>root inversion.
        // This is the case depth monotonicity alone cannot detect.
        let deep = AgentId::from("deep".to_owned());
        // chain[0] = immediate supervisor (mid = Normal), chain[1] = root (Guest).
        let chain = vec![
            node("mid", PermissionClass::Normal),
            node("root", PermissionClass::Guest),
        ];
        let err = validate_permission_class_from_chain(&deep, PermissionClass::Guest, 2, &chain)
            .unwrap_err();
        assert!(
            err.to_string().contains("exceeds its supervisor"),
            "{}",
            err
        );
    }
}
