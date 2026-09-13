//! Agent persistence restoration.
//!
//! Restores persisted agents top-down, level by level. The single root agent
//! (no supervisor) is restored first, then its children, and so on. Siblings
//! within each level are restored concurrently. An agent that fails
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
use std::path::PathBuf;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::lifecycle::SpawnArgs;
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
#[derive(Default)]
struct RestoreIndex {
    meta: HashMap<AgentId, persistence::AgentMeta>,
    exec: HashMap<AgentId, ExecPolicy>,
}

/// Restore one inactive-area agent back as a live registry entry —
/// the substrate of converge's restore action (and of nothing else:
/// boot restore walks the whole scan, this walks one body). The
/// caller has already moved the directory back into the live area
/// ([`persistence::reactivate_agent_dir`]) and passes the resulting
/// live-area dir plus its metadata; this builds the one-agent
/// restore index and runs the same [`restore_one`] path boot uses,
/// so identity reconstruction, workspace guards, and delegation
/// chain validation cannot drift between the two entry points.
/// Registration is the caller's job (it owns the write lock and the
/// batch's fail-fast policy).
pub(crate) async fn restore_inactive(
    shutdown: CancellationToken,
    shared_state: SharedState,
    p: persistence::PendingRestore,
) -> anyhow::Result<(AgentId, AgentEntry)> {
    let mut index = RestoreIndex::default();
    index.meta.insert(p.agent_id.clone(), p.meta.clone());
    if let Ok(exec) = persistence::load_exec_policy(&p.agent_dir) {
        index.exec.insert(p.agent_id.clone(), exec);
    }
    restore_one(p, shutdown, shared_state, &index).await
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

/// Validate the restored agent's `PermissionClass` against the supervisor
/// chain (the class invariant). Mirrors the policy/exec validators: the
/// agent's class must not exceed its immediate supervisor's, and the chain
/// must be monotonic. This is the restore-side guard against a tampered
/// `meta.json` elevating a child above its parent.
fn validate_permission_class_from_chain(
    agent_id: &AgentId,
    class: kallip_runtime::config::PermissionClass,
    chain: &[ChainNode],
) -> anyhow::Result<()> {
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

/// Restore-time set resolution. A dangling binding (a record that predates
/// set binding, or one naming a set the registry no longer offers) does not
/// fault the restore: the agent comes back unspecified, running against the
/// unconfigured placeholder, and delivery rejects inbound messages until a
/// live set is bound again.
fn set_for_restore(
    registry: &kallip_runtime::profile::ProfileRegistry,
    binding: Option<&str>,
) -> kallip_runtime::profile::ProfileSet {
    match registry.resolve_recorded_set(binding) {
        Ok(set) => set.clone(),
        Err(_) => crate::backend::unconfigured_set(),
    }
}

async fn restore_one(
    p: persistence::PendingRestore,
    shutdown: CancellationToken,
    shared_state: SharedState,
    index: &RestoreIndex,
) -> anyhow::Result<(AgentId, AgentEntry)> {
    let mut config = AgentConfig::load(None, vec![], Some(p.meta.workspace_root.clone()))?;
    // Config first: the tail-recovery budget derives from it (window size / 4)
    // and feeds the manifest-loss rebuild inside restore_agent.
    let mut restored =
        persistence::restore_agent(&p.agent_id, &p.agent_dir, config.tail_recovery_budget())?;

    // Surface non-fatal restore damage (missing history turns, skipped
    // corrupt lines) as structured warnings; a degraded agent still boots.
    for d in &restored.degraded {
        tracing::warn!(id = %p.agent_id, kind = ?d.kind, "agent restored degraded: {}", d.detail);
    }

    // Re-assemble image attachments: hydrated turns carry the text form
    // only, so each sidecar reference's bytes are fetched and the assembled
    // multimodal message swapped back in (invalidated references stay
    // excluded — a deterministic failure is marked once, never replayed).
    // Never fails the restore: a text-only boot is always possible, and
    // transient fetch failures simply leave the turn text-only for the
    // next restart to try again.
    let shared = shared_state.clone();
    type BoxedFetch = std::pin::Pin<
        Box<dyn std::future::Future<Output = kallip_runtime::context::FetchedImage> + Send>,
    >;
    let mut fetch = move |record_id, blob_id: Option<String>| -> BoxedFetch {
        let shared = shared.clone();
        Box::pin(async move {
            // fetch_record_bytes is only constructed here, not started:
            // fetch_local_first awaits it only when the local copy is
            // missing, so a local hit starts no files request.
            crate::files::fetch_local_first(
                shared.attachment_blobs.get(),
                record_id,
                crate::files::fetch_record_bytes(&shared.files_http, record_id),
                blob_id.as_deref(),
            )
            .await
        })
    };
    let reports = [
        kallip_runtime::context::reassemble_attachments(
            &mut restored.store,
            &restored.agent_dir,
            &mut fetch,
        )
        .await,
        // Pins persist as text plus references, so their bytes come back
        // through the same mechanism (invalidated references excluded).
        kallip_runtime::context::reassemble_pin_attachments(
            &mut restored.store,
            &restored.agent_dir,
            &mut fetch,
        )
        .await,
    ];
    for report in &reports {
        for (turn_id, record_id, reason) in &report.invalidated {
            tracing::warn!(
                id = %p.agent_id, turn_id, %record_id,
                "attachment reference invalidated during restore: {reason}"
            );
            kallip_runtime::history::HistoryWriter::new(restored.agent_dir.clone())
                .append(
                    None,
                    &[],
                    0,
                    kallip_runtime::history::RecordKind::System,
                    Some(kallip_runtime::history::SystemEvent::ReferenceInvalidated {
                        turn_id: *turn_id,
                        record_id: *record_id,
                        reason: reason.clone(),
                    }),
                    &[],
                )
                .ok();
        }
        for (_, record_id, reason) in &report.skipped {
            tracing::warn!(
                id = %p.agent_id, %record_id,
                "attachment reference left text-only after transient fetch failure: {reason}"
            );
        }
    }

    config.agent_id = Some(p.agent_id.clone());
    config.created_by = p.meta.created_by.clone();
    config.role = p.meta.role.clone();
    config.description = p.meta.description.clone();
    config.permissions_class = p.meta.permissions_class;
    config.delegation_mode = p.meta.delegation_mode;
    config.profile_set = p.meta.profile_set.clone();

    // Same data-dir overlap guard as `create_agent` (bidirectional, fail-closed).
    // An agent persisted before this guard existed with an overlapping workspace
    // fails restore here; `restore_agents` then registers it `Faulted` (its
    // children are still attempted) rather than restoring it into an unsafe
    // configuration.
    persistence::ensure_workspace_disjoint(&config.workspace_root)?;

    let exec_policy = index
        .get_exec_policy(&p.agent_id)
        .context("failed to load exec_policy")?;

    // The root's binding is derived: an unbound root record re-binds to
    // the current default set, so a bootless spawn followed by configuring
    // profiles needs no manual repair. A subagent's missing binding stays
    // dangling (unspecified) — only its supervisor can re-spawn it.
    if p.meta.created_by.is_none() && config.profile_set.is_none() {
        let default_set = shared_state.profiles.load().config.default.clone();
        config.profile_set = (!default_set.is_empty()).then_some(default_set);
    }
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
        validate_permission_class_from_chain(
            &p.agent_id,
            config.permissions_class,
            &supervisor_chain,
        )?;
        validate_exec_policy_from_chain(&p.agent_id, &exec_policy, &supervisor_chain)?;
    }
    let chain_ids: Vec<AgentId> = supervisor_chain
        .iter()
        .map(|n| n.agent_id.clone())
        .collect();
    // The root is the chain's terminal ancestor (or self for a root restore,
    // where the chain is empty).
    let root_agent_id = chain_ids
        .last()
        .cloned()
        .unwrap_or_else(|| p.agent_id.clone());

    // Resolve the profile set from the persisted binding.
    let set = {
        let bundle = shared_state.profiles.load();
        set_for_restore(&bundle.registry, config.profile_set.as_deref())
    };

    let store = Arc::new(tokio::sync::Mutex::new(restored.store));
    let approvals = Arc::new(tokio::sync::Mutex::new(restored.approvals));
    let (events_tx, _) = broadcast::channel(256);

    // Mint a fresh 256-bit `sk-agent-…` token. The plaintext goes into the agent shell env;
    // only its SHA-256 is indexed for auth lookup.
    let token = MintedToken::generate(AGENT);
    let env = SpawnArgs::default_env(
        &p.agent_id,
        token.secret(),
        p.meta.created_by.as_ref(),
        &root_agent_id,
    );

    let exec_policy = Arc::new(std::sync::RwLock::new(exec_policy));

    // Establish the workspace carve (forward handoff transfer + acquire +
    // reverse-transfer rollback guards) via the same shared helper as the spawn
    // path. Restore does NOT take the supervisor exec gate: at restore time the
    // server is not yet serving and no agent task is processing prompts, so no
    // shell fork can race the carve -- the dirlock's own mutex serializes the
    // state update. Taking the gate here would be purely destructive: BFS
    // restores siblings concurrently (`join_all`), they share one supervisor
    // gate, and only one would win its `try_write` while the rest bail to
    // Faulted. The helper has already restored the dirlock on any error; we
    // just bail so the caller registers this agent Faulted.
    let mut established =
        crate::lifecycle::establish_workspace_lock(&shared_state, &p.agent_id, &config, &chain_ids)
            .map_err(|e| anyhow::anyhow!("agent {}: {e}; skipping restore", p.agent_id))?;

    let (agent, identity) = (shared_state.spawn_fn)(SpawnArgs {
        agent_id: p.agent_id.clone(),
        root_agent_id: root_agent_id.clone(),
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
        set,
    })
    .await?;
    // Spawn succeeded: the agent owns the workspace lock for its lifetime.
    // Disarm both guards so their imminent Drops neither release the lock nor
    // reverse a handoff transfer. If spawn had failed, `established` would have
    // dropped on the `?` above in the right order (handoff reverse before
    // workspace release).
    established.disarm();
    drop(established);

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
        profile_set: meta.profile_set.clone(),
        permissions_class: meta.permissions_class,
        delegation_mode: meta.delegation_mode,
        ..AgentConfig::default()
    };
    FaultedEntry {
        identity: AgentIdentity {
            config,
            agent_dir: Some(agent_dir),
        },
        subagent_ids: vec![],
        reason,
        at: kallip_common::timefmt::now_epoch(),
    }
}

/// Register scan-refused agents as faulted. Their meta.json is unreadable, so
/// the entry is built from defaults plus the scan error; `workspace_root` is
/// left empty (the one source of truth for it is the meta we cannot read).
/// Refused agents stay visible and removable instead of vanishing.
async fn register_refused(state: &SharedState, refused: &[persistence::RefusedRestore]) {
    if refused.is_empty() {
        return;
    }
    let mut registry = state.registry.write().await;
    for r in refused {
        let config = AgentConfig {
            agent_id: Some(r.agent_id.clone()),
            created_by: None,
            role: String::new(),
            description: String::new(),
            workspace_root: PathBuf::new(),
            permissions_class: kallip_runtime::config::PermissionClass::default(),
            delegation_mode: kallip_runtime::config::DelegationMode::default(),
            ..AgentConfig::default()
        };
        let entry = FaultedEntry {
            identity: AgentIdentity {
                config,
                agent_dir: Some(r.agent_dir.clone()),
            },
            subagent_ids: vec![],
            reason: format!("agent directory unreadable: {}", r.error),
            at: kallip_common::timefmt::now_epoch(),
        };
        tracing::error!(id = %r.agent_id, "scan refused agent; registering as faulted");
        registry.register(r.agent_id.clone(), RegistryEntry::Faulted(entry));
    }
}

/// Restore persisted agents top-down, level by level.
///
/// The single root agent (no supervisor) is restored first, then its children,
/// and so on. Siblings within each level are restored concurrently. An agent
/// that fails to restore is registered in a `Faulted` state (not dropped), and
/// its children are still attempted -- so the supervisor chain stays intact and
/// every on-disk agent remains listable and removable.
///
/// **Singleton root:** the tagma owns exactly one root. If the data dir holds
/// more than one root, restore fails fast (the operator must remove the extras);
/// otherwise the lone root is re-registered via `register_root`. After restore,
/// [`ensure_root_agent`](crate::routes::agent::ensure_root_agent) creates the root if the
/// data dir was empty.
///
/// **Exempt from resource limits:** `max_agents` and `max_subagents` are not
/// enforced during restore. These agents were already running before the crash,
/// so refusing to restore them would be counterproductive. After restore,
/// `registry.len()` may exceed `max_agents`; new creation returns 503 until
/// agents are removed to make room.
pub async fn restore_agents(state: &SharedState) -> anyhow::Result<()> {
    let (pending, refused) = persistence::scan_agents()?;
    register_refused(state, &refused).await;
    if pending.is_empty() {
        return Ok(());
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
    // Supervisors on disk whose scan was refused (unreadable meta): their
    // children's orphan reason must say which case it is.
    let refused_ids: HashSet<&AgentId> = refused.iter().map(|r| &r.agent_id).collect();

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
                // Supervisor not restorable: absent from disk entirely
                // (pruned, archived, or removed), or present but unreadable
                // (already registered faulted by `register_refused`).
                // Either way this agent is registered faulted with its chain
                // intact so it stays individually manageable; no ghost
                // supervisor is fabricated -- there is no source-of-truth
                // metadata for one.
                let reason = if refused_ids.contains(supervisor_id) {
                    format!("supervisor {supervisor_id} unreadable (registered faulted)")
                } else {
                    format!("supervisor {supervisor_id} not present on disk")
                };
                tracing::error!(
                    id = %id,
                    supervisor = %supervisor_id,
                    "supervisor not restorable; registering agent as faulted"
                );
                orphan_faulted.push((
                    id.clone(),
                    faulted_from_meta(id, p.agent_dir.clone(), &p.meta, reason),
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

    // Singleton invariant: the tagma owns exactly one root. More than one root
    // on disk is a legacy/corrupt state the tagma refuses to paper over; the
    // operator must remove the extras. The common case (one root) is unaffected.
    if roots.len() > 1 {
        anyhow::bail!(
            "multiple root agents on disk ({count}); the tagma owns exactly one \
             root. Remove the extras from the instance data root's agents/active and restart",
            count = roots.len()
        );
    }

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
            // Route the single root through `register_root` (the singleton-
            // enforcing path); subagents use the raw inserter. The restore
            // fail-fast above guarantees at most one root, so `register_root`
            // cannot conflict here.
            for (id, entry) in successes {
                let entry = RegistryEntry::Live(entry);
                if entry.identity().config.is_root() {
                    registry
                        .register_root(id, entry)
                        .map_err(|e| anyhow::anyhow!("register root during restore: {e}"))?;
                } else {
                    registry.register(id, entry);
                }
            }
            for (id, entry) in faulted {
                let entry = RegistryEntry::Faulted(entry);
                if entry.identity().config.is_root() {
                    registry
                        .register_root(id, entry)
                        .map_err(|e| anyhow::anyhow!("register root during restore: {e}"))?;
                } else {
                    registry.register(id, entry);
                }
            }
        }

        next_level.sort();
        current_level = next_level;
    }

    // Any agent still pending here was never enqueued -- only possible for a
    // cycle that defeated the BFS seed. Register them faulted so they stay
    // visible and manageable; every restore failure path must surface as a
    // faulted agent, never a log line the operator cannot act on.
    if !pending_map.is_empty() {
        let mut registry = state.registry.write().await;
        for (id, p) in &pending_map {
            tracing::error!(
                id = %id,
                supervisor = ?p.meta.created_by,
                "agent was not reached by restore (created_by cycle); registering as faulted"
            );
            let entry = faulted_from_meta(
                id,
                p.agent_dir.clone(),
                &p.meta,
                "not reached by restore: created_by cycle".to_string(),
            );
            registry.register_no_subagent_push(id.clone(), RegistryEntry::Faulted(entry));
        }
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ChainNode, faulted_from_meta, set_for_restore, validate_permission_class_from_chain,
    };
    use kallip_common::agentid::AgentId;
    use kallip_common::policy::ExecPolicy;
    use kallip_runtime::config::PermissionClass;
    use kallip_runtime::persistence::AgentMeta;

    // Minimal BackendSource: an empty registry never consults it.
    struct NilSource;
    impl kallip_runtime::profile::BackendSource for NilSource {
        fn get(&self, _: &str) -> anyhow::Result<std::sync::Arc<dyn just_llm_client::LlmBackend>> {
            anyhow::bail!("nil source")
        }
    }

    #[test]
    fn dangling_binding_restores_against_placeholder() {
        let reg = kallip_runtime::profile::ProfileRegistry::new(
            std::collections::BTreeMap::new(),
            std::sync::Arc::new(NilSource),
        )
        .expect("empty set map is constructible");
        // A record with no binding (predates set binding)...
        let set = set_for_restore(&reg, None);
        assert_eq!(set.active_profile().endpoint, crate::backend::UNCONFIGURED);
        // ...and one naming a set the registry no longer offers: both
        // come back unspecified instead of faulting the restore.
        let set = set_for_restore(&reg, Some("gone"));
        assert_eq!(set.active_profile().endpoint, crate::backend::UNCONFIGURED);
    }

    #[test]
    fn bound_record_restores_its_named_set() {
        let set = kallip_runtime::profile::ProfileSet {
            name: "research".into(),
            description: None,
            profiles: vec![kallip_runtime::profile::Profile {
                id: "p".into(),
                endpoint: "ds".into(),
                model: "m".into(),
                max_context_window: 500_000,
                store: None,
                effort: None,
                modalities: kallip_runtime::profile::Profile::default_modalities(),
            }],
        };
        let reg = kallip_runtime::profile::ProfileRegistry::new(
            std::collections::BTreeMap::from([("research".to_string(), set)]),
            std::sync::Arc::new(NilSource),
        )
        .expect("single set registry constructs");
        let set = set_for_restore(&reg, Some("research"));
        assert_eq!(set.name, "research");
        assert_eq!(set.active_profile().model, "m");
    }
    // A supervisor chain node carrying only the fields the validator reads
    // (permissions_class) — the rest are defaulted/minimal.
    fn node(id: &str, class: PermissionClass) -> ChainNode {
        ChainNode {
            agent_id: AgentId::from(id.to_owned()),
            meta: AgentMeta {
                workspace_root: std::path::PathBuf::from("/ws"),
                created_by: None,
                role: String::new(),
                description: String::new(),
                profile_set: None,
                permissions_class: class,
                delegation_mode: kallip_runtime::config::DelegationMode::CarveOut,
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
            created_by: Some(AgentId::from("parent".to_owned())),
            role: "researcher".into(),
            description: "goners".into(),
            profile_set: Some("research".into()),
            permissions_class: PermissionClass::Guest,
            delegation_mode: kallip_runtime::config::DelegationMode::CarveOut,
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
            entry.identity.config.profile_set.as_deref(),
            Some("research")
        );
        assert_eq!(
            entry.identity.config.permissions_class,
            PermissionClass::Guest
        );
        assert_eq!(entry.identity.config.agent_id.as_ref(), Some(&id));
        assert!(entry.subagent_ids.is_empty());
    }

    #[test]
    fn restore_accepts_downgraded_subagent() {
        // A child explicitly granted Guest beneath a Normal supervisor must
        // restore cleanly — the downgrade is strictly lower than the
        // supervisor's class, and the chain stays monotonic. Guards the
        // restore-side gate (zero prior coverage).
        let child = AgentId::from("child".to_owned());
        let chain = vec![node("root", PermissionClass::Normal)];
        validate_permission_class_from_chain(&child, PermissionClass::Guest, &chain).unwrap();
    }

    #[test]
    fn restore_rejects_class_above_downgraded_supervisor() {
        // The restore-side mirror of the downgrade tightening: a child whose granted
        // class (Normal) exceeds its downgraded supervisor's (Guest) must fail
        // restore (the supervisor, not a depth-derived table, is the limit).
        let child = AgentId::from("child".to_owned());
        let chain = vec![node("root", PermissionClass::Guest)];
        let err = validate_permission_class_from_chain(&child, PermissionClass::Normal, &chain)
            .unwrap_err();
        assert!(err.to_string().contains("supervisor"), "{}", err);
    }

    #[test]
    fn restore_enforces_chain_monotonicity() {
        // A two-level chain where the deeper ancestor (root) was downgraded to
        // Guest but the mid node persisted at Normal (tampered meta.json). The
        // agent itself sits validly at Guest, and
        // beneath its immediate supervisor (mid = Normal) — so only the
        // `chain.windows(2)` monotonicity check catches the mid>root inversion.
        // This is the case depth monotonicity alone cannot detect.
        let deep = AgentId::from("deep".to_owned());
        // chain[0] = immediate supervisor (mid = Normal), chain[1] = root (Guest).
        let chain = vec![
            node("mid", PermissionClass::Normal),
            node("root", PermissionClass::Guest),
        ];
        let err = validate_permission_class_from_chain(&deep, PermissionClass::Guest, &chain)
            .unwrap_err();
        assert!(
            err.to_string().contains("exceeds its supervisor"),
            "{}",
            err
        );
    }
    #[tokio::test]
    async fn register_refused_marks_unreadable_agent_faulted() {
        use crate::state::RegistryEntry;
        use crate::test_helpers::make_state;
        let state = make_state();
        let id = AgentId::from("refused-1".to_owned());
        let refused = vec![super::persistence::RefusedRestore {
            agent_id: id.clone(),
            agent_dir: std::path::PathBuf::from("/data/agents/refused-1"),
            error: "reading meta.json: no such file".to_string(),
        }];
        super::register_refused(&state, &refused).await;
        let registry = state.registry.read().await;
        match registry.get(&id) {
            Some(RegistryEntry::Faulted(entry)) => {
                assert!(entry.reason.contains("unreadable"));
                assert!(entry.reason.contains("meta.json"));
                assert!(
                    entry.identity.config.workspace_root.as_os_str().is_empty(),
                    "workspace root stays empty when the meta cannot be read"
                );
            }
            _ => panic!("expected faulted registration for {id}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn restore_agents_registers_cycle_stragglers_faulted() {
        // See the ensure-root test in routes::agent for why the data dir is
        // created under /dev/shm rather than /tmp: this test mutates
        // XDG_DATA_HOME and must not overlap concurrently-running tests'
        // /tmp workspaces.
        let tmp = tempfile::TempDir::new_in("/dev/shm").unwrap();
        let base = tmp
            .path()
            .join("kallipai")
            .join("tagmata")
            .join("cycle")
            .join("agents")
            .join("active");
        std::fs::create_dir_all(&base).unwrap();
        let a = AgentId::from("cycle-a".to_owned());
        let b = AgentId::from("cycle-b".to_owned());
        let write_meta = |id: &AgentId, created_by: Option<&AgentId>| {
            std::fs::create_dir_all(base.join(id.as_ref())).unwrap();
            std::fs::write(
                base.join(id.as_ref()).join("meta.json"),
                serde_json::to_string(&AgentMeta {
                    workspace_root: std::path::PathBuf::from("/ws"),
                    created_by: created_by.cloned(),
                    role: String::new(),
                    description: String::new(),
                    profile_set: None,
                    permissions_class: PermissionClass::Normal,
                    delegation_mode: kallip_runtime::config::DelegationMode::CarveOut,
                })
                .unwrap(),
            )
            .unwrap();
        };
        write_meta(&a, Some(&b));
        write_meta(&b, Some(&a));
        let path = tmp.path().to_str().unwrap().to_owned();
        temp_env::with_vars(
            [
                ("KALLIP_TAGMA_SLUG", Some("cycle")),
                ("XDG_DATA_HOME", Some(path.as_str())),
            ],
            || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    use crate::state::RegistryEntry;
                    use crate::test_helpers::make_state;
                    let state = make_state();
                    super::restore_agents(&state).await.unwrap();
                    let registry = state.registry.read().await;
                    for id in [&a, &b] {
                        assert!(
                            matches!(registry.get(id), Some(RegistryEntry::Faulted(_))),
                            "cycle agent {id} must be registered faulted, not dropped"
                        );
                    }
                });
            },
        );
    }
}
